//! `bitrouter cloud login` / `logout` device-flow entry points.
//!
//! The CLI in `apps/bitrouter/src/main.rs` parses the subcommand + flags
//! into [`LoginInputs`] and hands off to the functions here.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;

use super::credentials::{
    CredentialKind, Credentials, CredentialsStore, StoredCredential, default_credentials_path,
};
use super::flow;
use super::metadata::{self, AsMetadata};
use super::settings::{Settings, resolve_from_env};

/// User-supplied flag values for `bitrouter cloud login` / `logout`.
/// The CLI passes them straight through.
#[derive(Debug, Default, Clone)]
pub struct LoginInputs {
    /// `--oauth-as <URL>`.
    pub authorization_server: Option<String>,
    /// `--client-id <ID>`.
    pub client_id: Option<String>,
    /// `--scope <SCOPE>`.
    pub scope: Option<String>,
    /// `--api-key <BRK_API_KEY>`.
    pub api_key: Option<String>,
}

/// Build a fresh reqwest client preconfigured with the bitrouter
/// user-agent + sensible defaults. Centralised so every HTTP call from
/// this module sends a consistent UA (RFC 9110 §10.1.5).
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("bitrouter/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building reqwest client")
}

/// Run the device-authorization grant against the configured AS and
/// persist the resulting tokens. Used by `bitrouter cloud login`.
pub async fn login(inputs: LoginInputs) -> Result<StoredCredential> {
    if let Some(api_key) = inputs.api_key {
        if inputs.client_id.is_some() || inputs.scope.is_some() {
            anyhow::bail!("--api-key cannot be combined with --client-id or --scope");
        }
        let settings = resolve_from_env(inputs.authorization_server.as_deref(), None, None)?;
        let path = default_credentials_path().context("resolving credentials path")?;
        let stored = login_api_key_at_path(api_key, settings.authorization_server, &path)?;
        eprintln!();
        eprintln!(
            "  Signed in with an API key. Credentials saved to {}",
            path.display()
        );
        return Ok(stored);
    }
    let settings = resolve_from_env(
        inputs.authorization_server.as_deref(),
        inputs.client_id.as_deref(),
        inputs.scope.as_deref(),
    )?;
    let client = http_client()?;
    let metadata = metadata::fetch(&client, &settings.authorization_server)
        .await
        .with_context(|| format!("fetching AS metadata for {}", settings.authorization_server))?;
    let token_set = flow::run_device_flow(&client, &metadata, &settings, |device| {
        // RFC 8628 §3.2 — when the AS returns `verification_uri_complete`
        // (the URL with `user_code` already embedded as a query
        // parameter), the approval page auto-fills the code, so the
        // user only opens the link. Showing both the URL and "and
        // enter the code …" in that case is confusing — the page
        // already did. Only print the separate-code prompt when we
        // fall back to the bare `verification_uri`.
        eprintln!();
        if let Some(complete) = device.verification_uri_complete.as_deref() {
            eprintln!("  Open this URL in your browser:");
            eprintln!("    {complete}");
        } else {
            eprintln!("  Open this URL in your browser, then enter the code:");
            eprintln!("    {}", device.verification_uri);
            eprintln!("    Code: {}", device.user_code);
        }
        eprintln!();
        eprintln!(
            "  Waiting for authorization (the code expires in {}s)…",
            device.expires_in
        );
    })
    .await?;
    let credentials = flow::credentials_from_token_set(token_set, &settings);
    let stored = StoredCredential::from(credentials.clone());
    let mut store = CredentialsStore::default_path().context("opening credentials store")?;
    store
        .save(stored.clone())
        .context("persisting credentials")?;
    eprintln!();
    eprintln!(
        "  Signed in. Credentials saved to {}",
        store.path().display()
    );
    if let Some(sub) = credentials.subject.as_deref() {
        eprintln!("    subject: {sub}");
    }
    eprintln!("    scope:   {}", credentials.scope);
    Ok(stored)
}

fn validate_api_key(api_key: &str) -> Result<()> {
    let Some(value) = api_key.strip_prefix("brk_") else {
        anyhow::bail!("invalid BitRouter API key: expected brk_<token_id>.<secret>");
    };
    let mut parts = value.split('.');
    let token_id = parts.next().unwrap_or_default();
    let secret = parts.next().unwrap_or_default();
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    if !valid_part(token_id) || !valid_part(secret) || parts.next().is_some() {
        anyhow::bail!("invalid BitRouter API key: expected brk_<token_id>.<secret>");
    }
    Ok(())
}

fn login_api_key_at_path(
    api_key: String,
    base_url: String,
    path: &Path,
) -> Result<StoredCredential> {
    validate_api_key(&api_key)?;
    super::settings::require_secure_url(&base_url)?;
    let parsed = url::Url::parse(&base_url).context("parsing BitRouter Cloud base URL")?;
    if parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        anyhow::bail!(
            "BitRouter Cloud base URL must be an origin URL without credentials, query, or fragment"
        );
    }
    let base_url = parsed.as_str().trim_end_matches('/').to_owned();
    let stored = StoredCredential::api_key(api_key, base_url);
    let mut store = CredentialsStore::load(path).context("opening credentials store")?;
    store
        .save(stored.clone())
        .context("persisting credentials")?;
    Ok(stored)
}

/// Revoke the stored tokens (best-effort) and clear the local file.
/// Used by `bitrouter cloud logout`.
pub async fn logout(inputs: LoginInputs) -> Result<()> {
    let path = default_credentials_path().context("resolving credentials path")?;
    logout_at_path(inputs, &path).await
}

async fn logout_at_path(inputs: LoginInputs, path: &Path) -> Result<()> {
    let mut store = CredentialsStore::load(path).context("opening credentials store")?;
    let prior = match store.current().cloned() {
        Some(p) => p,
        None => {
            eprintln!("  No stored credentials; nothing to do.");
            return Ok(());
        }
    };
    if prior.kind() == CredentialKind::ApiKey {
        store.clear().context("removing credentials file")?;
        eprintln!("  Signed out. Credentials file removed.");
        return Ok(());
    }
    let prior = prior
        .oauth()
        .context("stored OAuth credential is missing its token payload")?
        .clone();
    // For revoke we re-resolve settings so an explicit `--oauth-as` override
    // wins. When no flag/env override is set we use the AS recorded in the
    // credentials file. This handles the case where the user logged in
    // against one AS and is now in a shell with a different env var set —
    // the file's AS is the source of truth for revocation.
    let settings = resolve_from_env(
        inputs.authorization_server.as_deref(),
        inputs.client_id.as_deref(),
        inputs.scope.as_deref(),
    )
    .unwrap_or(Settings {
        authorization_server: prior.authorization_server.clone(),
        client_id: prior.client_id.clone(),
        scope: prior.scope.clone(),
    });
    let client = http_client()?;
    // Metadata discovery is best-effort for logout — if the AS is
    // unreachable we still delete the local file (so logout is never
    // blocked by network).
    let metadata = metadata::fetch(&client, &settings.authorization_server)
        .await
        .ok();
    if let Some(metadata) = metadata {
        best_effort_revoke(&client, &metadata, &settings, &prior).await;
    } else {
        tracing::debug!(
            "AS metadata fetch failed during logout; skipping revoke and removing local file"
        );
    }
    store.clear().context("removing credentials file")?;
    eprintln!("  Signed out. Credentials file removed.");
    Ok(())
}

async fn best_effort_revoke(
    client: &reqwest::Client,
    metadata: &AsMetadata,
    settings: &Settings,
    credentials: &Credentials,
) {
    let Some(endpoint) = metadata.revocation_endpoint.as_deref() else {
        // RFC 7009 is optional; nothing to call.
        return;
    };
    // RFC 7009 §2.1: revoking a refresh_token typically invalidates
    // associated access tokens, so we issue both calls in the order
    // refresh → access. Errors are logged but never propagated — the
    // local file is about to be deleted.
    if let Some(rt) = credentials.refresh_token.as_deref()
        && let Err(e) =
            flow::revoke(client, endpoint, &settings.client_id, rt, "refresh_token").await
    {
        tracing::debug!(error = %e, "refresh_token revoke failed (ignored)");
    }
    if let Err(e) = flow::revoke(
        client,
        endpoint,
        &settings.client_id,
        &credentials.access_token,
        "access_token",
    )
    .await
    {
        tracing::debug!(error = %e, "access_token revoke failed (ignored)");
    }
}

/// Print local identity. Does NOT call the AS — answers from the
/// on-disk file only, so it's fast and works offline.
pub async fn whoami() -> Result<()> {
    let store = CredentialsStore::default_path().context("opening credentials store")?;
    match store.current() {
        Some(credential) => {
            let Some(creds) = credential.oauth() else {
                println!("authentication:       api_key");
                println!("authorization server: {}", credential.base_url());
                println!("credentials file:     {}", store.path().display());
                return Ok(());
            };
            println!("authentication:       oauth");
            let now = Utc::now();
            let status = if now < creds.expires_at {
                let remaining = (creds.expires_at - now).num_seconds().max(0);
                format!("valid ({remaining}s remaining)")
            } else {
                let elapsed = (now - creds.expires_at).num_seconds().max(0);
                format!("EXPIRED ({elapsed}s ago)")
            };
            println!("authorization server: {}", creds.authorization_server);
            println!("client id:            {}", creds.client_id);
            println!("scope:                {}", creds.scope);
            println!(
                "namespace:            {}",
                creds.namespace_id.as_deref().unwrap_or("(none)")
            );
            println!(
                "subject:              {}",
                creds.subject.as_deref().unwrap_or("(unknown)")
            );
            println!("access token:         {status}");
            println!("expires at (UTC):     {}", creds.expires_at.to_rfc3339());
            if let Some(rt_exp) = creds.refresh_token_expires_at {
                println!("refresh expires (UTC):{}", rt_exp.to_rfc3339());
            }
            println!("credentials file:     {}", store.path().display());
            Ok(())
        }
        None => {
            let default = default_credentials_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unresolved>".into());
            println!("not signed in (no credentials at {default})");
            println!("  run `bitrouter cloud login` to sign in");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::auth::credentials::CredentialKind;

    fn tmp_credentials_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cloud-login-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("account-credentials.json")
    }

    #[test]
    fn validates_brk_api_key_shape() {
        assert!(validate_api_key("brk_AAAAAAAAAAAAAAAA.secret").is_ok());
        assert!(validate_api_key("sk-not-bitrouter").is_err());
        assert!(validate_api_key("brk_missing-dot").is_err());
        assert!(validate_api_key("brk_.secret").is_err());
        assert!(validate_api_key("brk_token.").is_err());
        assert!(validate_api_key("brk_token.secret.extra").is_err());
        assert!(validate_api_key("brk_token.sec ret").is_err());
    }

    #[test]
    fn api_key_login_persists_without_discovery() {
        let path = tmp_credentials_path("api-key");
        let credential = login_api_key_at_path(
            "brk_AAAAAAAAAAAAAAAA.secret".to_owned(),
            "https://api.bitrouter.ai".to_owned(),
            &path,
        )
        .unwrap();

        assert_eq!(credential.kind(), CredentialKind::ApiKey);
        assert_eq!(credential.base_url(), "https://api.bitrouter.ai");
        assert_eq!(
            CredentialsStore::load(path)
                .unwrap()
                .current()
                .unwrap()
                .kind(),
            CredentialKind::ApiKey
        );
    }

    #[test]
    fn api_key_login_rejects_invalid_base_url() {
        for base_url in ["https://", "https://api.bitrouter.ai/#fragment"] {
            let path = tmp_credentials_path("invalid-base-url");
            let result = login_api_key_at_path(
                "brk_AAAAAAAAAAAAAAAA.secret".to_owned(),
                base_url.to_owned(),
                &path,
            );
            assert!(result.is_err(), "{base_url} must be rejected");
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn api_key_logout_is_local_only() {
        let path = tmp_credentials_path("api-key-logout");
        login_api_key_at_path(
            "brk_AAAAAAAAAAAAAAAA.secret".to_owned(),
            "https://unreachable.invalid".to_owned(),
            &path,
        )
        .unwrap();

        logout_at_path(LoginInputs::default(), &path).await.unwrap();

        assert!(CredentialsStore::load(path).unwrap().current().is_none());
    }
}

//! `bitrouter auth <login|logout|whoami>` entry points.
//!
//! The CLI in `apps/bitrouter/src/main.rs` parses the subcommand + flags
//! into [`LoginInputs`] and hands off to the functions here.

use anyhow::{Context, Result};
use chrono::Utc;

use super::credentials::{Credentials, CredentialsStore, default_credentials_path};
use super::flow;
use super::metadata::{self, AsMetadata};
use super::settings::{Settings, resolve_from_env};

/// User-supplied flag values for `bitrouter auth login` / `logout`.
/// The CLI passes them straight through.
#[derive(Debug, Default, Clone)]
pub struct LoginInputs {
    /// `--oauth-as <URL>`.
    pub authorization_server: Option<String>,
    /// `--client-id <ID>`.
    pub client_id: Option<String>,
    /// `--scope <SCOPE>`.
    pub scope: Option<String>,
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
/// persist the resulting tokens. Used by `bitrouter auth login`.
pub async fn login(inputs: LoginInputs) -> Result<Credentials> {
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
    let mut store = CredentialsStore::default_path().context("opening credentials store")?;
    store
        .save(credentials.clone())
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
    Ok(credentials)
}

/// Revoke the stored tokens (best-effort) and clear the local file.
/// Used by `bitrouter auth logout`.
pub async fn logout(inputs: LoginInputs) -> Result<()> {
    let mut store = CredentialsStore::default_path().context("opening credentials store")?;
    let prior = match store.current().cloned() {
        Some(p) => p,
        None => {
            eprintln!("  No stored credentials; nothing to do.");
            return Ok(());
        }
    };
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
        Some(creds) => {
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
            println!("  run `bitrouter auth login` to sign in");
            Ok(())
        }
    }
}

/// Return the current access token, transparently refreshing if it's
/// within ~60s of expiry. Exposed as the §5 helper for any future call
/// site that needs to attach `Authorization: Bearer …` to an outgoing
/// request when no explicit per-call key was passed.
pub async fn current_bearer() -> Result<Option<String>> {
    let mut store = CredentialsStore::default_path()?;
    if store.current().is_none() {
        return Ok(None);
    }
    let client = http_client()?;
    let as_url = store
        .current()
        .map(|c| c.authorization_server.clone())
        .expect("checked above");
    let metadata = metadata::fetch(&client, &as_url)
        .await
        .with_context(|| format!("fetching AS metadata for {as_url}"))?;
    let token = store.current_token(&client, &metadata).await?;
    Ok(Some(token))
}

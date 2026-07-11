//! Orchestrate the full PKCE Authorization Code login for a registered
//! provider — bind the loopback listener, build the `/authorize` URL,
//! tell the user to open it, race the listener against a manual-paste
//! fallback, verify `state`, and exchange the code for an
//! [`OAuthToken`].
//!
//! The caller wires the user interaction (where to print the URL, how to
//! read a pasted redirect) through the [`LoginUx`] trait so this module
//! stays unit-testable. `apps/bitrouter` provides a stdin/stderr
//! implementation; tests can plug in a scripted one.

use std::time::Duration;

use crate::oauth::auth_code::{
    AuthCodeError, build_authorize_url, exchange_code, parse_pasted_redirect,
};
use crate::oauth::credential_store::OAuthToken;
use crate::oauth::listener::{CallbackOutcome, ListenerError, LoopbackListener};
use crate::oauth::pkce;
use crate::oauth::registry::PkceProvider;

/// User-facing I/O hooks the login flow drives. Implementors typically
/// print to stderr and read from stdin; see `apps/bitrouter`.
#[async_trait::async_trait]
pub trait LoginUx: Send + Sync {
    /// Show the `/authorize` URL the user should open in their browser,
    /// plus a one-line hint about what comes next.
    async fn show_authorize_url(&self, url: &str, hint: &str);
    /// Prompt for a pasted redirect URL (manual fallback path). Returns
    /// the raw string the user typed. Implementors typically block until
    /// the user presses enter; the caller times the wait out via
    /// [`AuthCodeError::Malformed`] if the paste never arrives.
    async fn prompt_pasted_redirect(&self) -> Result<String, LoginError>;
}

/// Errors raised by the login orchestration.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// Loopback listener bind failed AND the provider doesn't expose a
    /// manual-paste fallback.
    #[error(
        "could not bind loopback listener for {provider} and no manual fallback is configured: {source}"
    )]
    NoListenerNoFallback {
        /// Provider id whose flow failed.
        provider: &'static str,
        /// Underlying listener error.
        #[source]
        source: ListenerError,
    },
    /// A provider with a pinned loopback port (the port is registered
    /// against the upstream OAuth client; dynamic ports are rejected)
    /// couldn't bind because another process holds it. Common cause: the
    /// official vendor CLI (e.g. `codex login`) is running concurrently
    /// against the same port.
    #[error(
        "{provider} requires loopback port {port} but it's already in use — \
         quit any other client signing in to this provider (e.g. the official \
         vendor CLI) and retry. ({source})"
    )]
    PinnedPortInUse {
        /// Provider id whose flow failed.
        provider: &'static str,
        /// The port the provider pinned in its OAuth client registration.
        port: u16,
        /// Underlying listener bind error.
        #[source]
        source: ListenerError,
    },
    /// User pasted nothing (or only whitespace) in manual-fallback mode.
    #[error(
        "manual redirect paste was empty — re-run `bitrouter providers login <provider>` to try again"
    )]
    EmptyPaste,
    /// `state` echoed by the redirect didn't match what we sent on the
    /// authorize URL. Signals a CSRF attempt or a stale paste from an
    /// earlier flow.
    #[error("OAuth state mismatch — refusing to redeem this code")]
    StateMismatch,
    /// Authorization server reported an `error=` on the redirect.
    #[error("OAuth server returned error '{error}'{}", description.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    Server {
        /// The RFC 6749 §4.1.2.1 error code (e.g. `access_denied`).
        error: String,
        /// Optional human-readable description.
        description: Option<String>,
    },
    /// Loopback listener I/O / timeout.
    #[error("loopback listener error: {0}")]
    Listener(#[from] ListenerError),
    /// Authorization-code exchange failed.
    #[error("token exchange error: {0}")]
    AuthCode(#[from] AuthCodeError),
    /// User-supplied I/O hook returned an error.
    #[error("user I/O error: {0}")]
    UserIo(String),
}

/// Outcome of [`run_login`].
#[derive(Debug)]
pub struct LoginOutcome {
    /// The freshly-minted token to persist in the credential store.
    pub token: OAuthToken,
    /// Whether the manual-paste fallback was used (vs. the listener
    /// catching the redirect). Useful for messages like "we used the
    /// manual flow because port 1455 was in use".
    pub manual_fallback_used: bool,
}

/// Drive the full PKCE Authorization Code login for `provider`.
///
/// Behaviour:
/// 1. Tries to bind a loopback listener on the provider's preferred port.
///    If the bind succeeds, the redirect URI is the listener's URL.
/// 2. If the bind fails AND the provider exposes a `manual_redirect_uri`,
///    fall back to that and prompt the user to paste the redirect URL.
/// 3. With the chosen redirect URI, generate PKCE + state, build the
///    authorize URL, hand it to [`LoginUx::show_authorize_url`], race the
///    listener (if any) against the paste prompt, verify `state`, and
///    exchange the code for an OAuth token.
///
/// `manual_paste_timeout` bounds the wait on the user's manual-paste
/// input — generous values are appropriate (the user has to complete a
/// browser sign-in first).
pub async fn run_login(
    client: &reqwest::Client,
    provider: &PkceProvider,
    ux: &dyn LoginUx,
    manual_paste_timeout: Duration,
) -> Result<LoginOutcome, LoginError> {
    let pkce_pair = pkce::generate();
    let state = pkce::generate_state();

    // Attempt loopback bind on the preferred port (or OS-assigned when
    // `loopback_port` is None).
    let bound =
        match LoopbackListener::bind(provider.loopback_port.unwrap_or(0), provider.redirect_path)
            .await
        {
            Ok(l) => Some(l),
            Err(e) => match provider.manual_redirect_uri {
                Some(_) => None, // fall back to manual paste
                None => {
                    // Pinned-port providers (e.g. openai-codex on 1455)
                    // can't recover by trying a different port — the
                    // OAuth client registration rejects mismatched
                    // redirect URIs. Surface the conflict explicitly so
                    // users know to quit the colliding process.
                    if let (Some(port), ListenerError::Bind { .. }) = (provider.loopback_port, &e) {
                        return Err(LoginError::PinnedPortInUse {
                            provider: provider.provider_id,
                            port,
                            source: e,
                        });
                    }
                    return Err(LoginError::NoListenerNoFallback {
                        provider: provider.provider_id,
                        source: e,
                    });
                }
            },
        };

    let (redirect_uri, manual_only) = match (&bound, provider.manual_redirect_uri) {
        (Some(l), _) => (l.redirect_uri().to_string(), false),
        (None, Some(manual)) => (manual.to_string(), true),
        (None, None) => unreachable!("guarded by the match above"),
    };

    let url = build_authorize_url(&provider.auth, &redirect_uri, &state, &pkce_pair.challenge)?;
    let hint = if manual_only {
        "After signing in, paste the redirect URL here."
    } else {
        "After signing in, the browser will redirect back to bitrouter automatically. \
         If that fails, paste the redirect URL here."
    };
    ux.show_authorize_url(&url, hint).await;

    // Race the listener (if bound) against a manual paste prompt. When
    // both are available the first one to complete wins.
    let (code, returned_state, used_manual) = match bound {
        Some(listener) => {
            let listener_fut = listener.accept_one(manual_paste_timeout);
            let paste_fut = ux.prompt_pasted_redirect();
            tokio::select! {
                outcome = listener_fut => {
                    let outcome = outcome?;
                    parse_listener_outcome(outcome).map(|(c, s)| (c, s, false))?
                }
                pasted = paste_fut => {
                    let pasted = pasted?;
                    parse_pasted(&pasted).map(|(c, s)| (c, s, true))?
                }
            }
        }
        None => {
            let pasted = ux.prompt_pasted_redirect().await?;
            let (code, state) = parse_pasted(&pasted)?;
            (code, state, true)
        }
    };

    validate_state(returned_state.as_deref(), &state, used_manual)?;

    let token = exchange_code(
        client,
        &provider.auth,
        &code,
        &pkce_pair.verifier,
        &redirect_uri,
    )
    .await?;

    Ok(LoginOutcome {
        token,
        manual_fallback_used: used_manual,
    })
}

fn parse_listener_outcome(
    outcome: CallbackOutcome,
) -> Result<(String, Option<String>), LoginError> {
    match outcome {
        CallbackOutcome::Success { code, state } => Ok((code, state)),
        CallbackOutcome::Error {
            error, description, ..
        } => Err(LoginError::Server { error, description }),
    }
}

fn parse_pasted(input: &str) -> Result<(String, Option<String>), LoginError> {
    let parsed = parse_pasted_redirect(input).ok_or(LoginError::EmptyPaste)?;
    Ok((parsed.code, parsed.state))
}

/// Compare the `state` parameter we sent on `/authorize` to whatever the
/// flow echoed back. Behaviour differs by branch:
///
/// - **Loopback listener** (`used_manual == false`): state MUST be
///   present and match. The browser always includes whatever query
///   params the authorization server placed on the redirect, so a
///   missing `state` here means a third-party hit our
///   `127.0.0.1:<port>/callback` directly with a forged `code=…`.
///   Without this guard, a malicious local process could redeem a code
///   on the user's behalf — defeats the point of `state`.
///
/// - **Manual paste** (`used_manual == true`): state MAY be absent. Some
///   redirect display surfaces (e.g. Anthropic's
///   `console.anthropic.com/oauth/code/callback` page) show `code#state`
///   and users sometimes paste only the `code`. We accept that and rely
///   on PKCE alone — the verifier is still bound to this process.
fn validate_state(
    returned: Option<&str>,
    expected: &str,
    used_manual: bool,
) -> Result<(), LoginError> {
    match returned {
        Some(r) if r != expected => Err(LoginError::StateMismatch),
        None if !used_manual => Err(LoginError::StateMismatch),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::oauth::auth_code::AuthCodeParams;
    use std::collections::BTreeMap;

    /// Scripted UX harness — captures the URL the flow showed and feeds
    /// a canned paste back when prompted.
    struct ScriptedUx {
        captured_url: Mutex<Option<String>>,
        paste: Mutex<Option<String>>,
    }

    impl ScriptedUx {
        fn with_paste(paste: &str) -> Self {
            Self {
                captured_url: Mutex::new(None),
                paste: Mutex::new(Some(paste.into())),
            }
        }
        fn captured(&self) -> Option<String> {
            self.captured_url.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl LoginUx for ScriptedUx {
        async fn show_authorize_url(&self, url: &str, _hint: &str) {
            *self.captured_url.lock().unwrap() = Some(url.to_string());
        }
        async fn prompt_pasted_redirect(&self) -> Result<String, LoginError> {
            let p = self
                .paste
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| LoginError::UserIo("paste exhausted".into()))?;
            Ok(p)
        }
    }

    fn pkce_provider_for_test(server_uri: &str) -> PkceProvider {
        PkceProvider {
            provider_id: "test",
            display_name: "Test",
            // No loopback port so the bind succeeds on an OS-assigned
            // port; tests don't actually exercise the listener path
            // here (we want manual-paste to fire first).
            loopback_port: None,
            redirect_path: "/callback",
            manual_redirect_uri: Some("https://example.com/oauth/code/callback"),
            auth: AuthCodeParams {
                client_id: "client-test".into(),
                // We intentionally don't run the HTTPS guard against
                // these (wiremock binds http://), so direct exchange
                // tests in `oauth::auth_code` cover the wire-format.
                // The login orchestration test below uses the manual
                // paste path which doesn't hit the listener; the
                // exchange does run against `token_endpoint` but we
                // override the URL to wiremock (http) — which the
                // built-in HTTPS check rejects. So the unit test
                // covers the URL-build + paste-parse path; an
                // integration test would need a TLS-terminated mock.
                authorize_endpoint: "https://example.com/oauth/authorize".into(),
                token_endpoint: format!("{server_uri}/oauth/token"),
                scope: "read".into(),
                extra_authorize: BTreeMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn url_is_built_with_pkce_and_state_in_query() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "OK",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;
        let provider = pkce_provider_for_test(&server.uri());
        let ux = Arc::new(ScriptedUx::with_paste("code=AC&state=ST"));
        // Token endpoint is http (wiremock) — the auth_code exchange
        // refuses, so the call errors after URL build. We only assert
        // the URL build path here.
        let _ = run_login(
            &reqwest::Client::new(),
            &provider,
            ux.as_ref(),
            Duration::from_millis(50),
        )
        .await;
        let url = ux.captured().expect("URL must be shown to the user");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        assert!(url.contains("client_id=client-test"));
    }

    #[test]
    fn parse_pasted_handles_full_url_and_query() {
        let (code, state) = parse_pasted("http://x/cb?code=AC&state=ST").unwrap();
        assert_eq!(code, "AC");
        assert_eq!(state.as_deref(), Some("ST"));

        let (code2, state2) = parse_pasted("code=AC2&state=ST2").unwrap();
        assert_eq!(code2, "AC2");
        assert_eq!(state2.as_deref(), Some("ST2"));
    }

    #[test]
    fn parse_pasted_rejects_empty() {
        let err = parse_pasted("").unwrap_err();
        assert!(matches!(err, LoginError::EmptyPaste));
    }

    /// Provider whose pinned loopback port can't be bound and which
    /// offers no manual-paste fallback — exercises the `PinnedPortInUse`
    /// branch.
    fn pinned_port_provider(port: u16) -> PkceProvider {
        PkceProvider {
            provider_id: "test-pinned",
            display_name: "Test Pinned",
            loopback_port: Some(port),
            redirect_path: "/auth/callback",
            manual_redirect_uri: None,
            auth: AuthCodeParams {
                client_id: "client-pinned".into(),
                authorize_endpoint: "https://example.com/oauth/authorize".into(),
                token_endpoint: "https://example.com/oauth/token".into(),
                scope: "read".into(),
                extra_authorize: BTreeMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn pinned_port_collision_surfaces_specific_error() {
        // Bind the port ourselves so the run_login bind attempt fails
        // with EADDRINUSE. Use 0 to get an OS-assigned port, then read
        // it back — avoids hard-coding a port that might be in use on
        // the dev box.
        let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = blocker.local_addr().unwrap().port();
        let provider = pinned_port_provider(port);
        let ux = Arc::new(ScriptedUx::with_paste("ignored"));
        let err = run_login(
            &reqwest::Client::new(),
            &provider,
            ux.as_ref(),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        match err {
            LoginError::PinnedPortInUse {
                provider: p,
                port: returned_port,
                ..
            } => {
                assert_eq!(p, "test-pinned");
                assert_eq!(returned_port, port);
            }
            other => panic!("expected PinnedPortInUse, got: {other:?}"),
        }
    }

    #[test]
    fn validate_state_listener_branch_requires_match() {
        // Loopback listener (used_manual = false): the browser always
        // forwards every query param, so `None` here means a third
        // party hit our callback directly. Reject.
        assert!(matches!(
            validate_state(None, "EXPECTED", false),
            Err(LoginError::StateMismatch)
        ));
        // Mismatch is always a rejection.
        assert!(matches!(
            validate_state(Some("WRONG"), "EXPECTED", false),
            Err(LoginError::StateMismatch)
        ));
        // Matching state passes.
        assert!(validate_state(Some("EXPECTED"), "EXPECTED", false).is_ok());
    }

    #[test]
    fn validate_state_manual_paste_branch_tolerates_missing_state() {
        // Manual paste (used_manual = true): missing state is OK
        // because some console pages (e.g. Anthropic's
        // `console.anthropic.com/oauth/code/callback`) render
        // `code#state` and users sometimes paste only the `code`. PKCE
        // is still binding.
        assert!(validate_state(None, "EXPECTED", true).is_ok());
        // But a wrong state is still a rejection — CSRF protection
        // applies when the user actually pastes one.
        assert!(matches!(
            validate_state(Some("WRONG"), "EXPECTED", true),
            Err(LoginError::StateMismatch)
        ));
        assert!(validate_state(Some("EXPECTED"), "EXPECTED", true).is_ok());
    }
}

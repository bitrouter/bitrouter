#![cfg(feature = "pkce")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bitrouter_ai::auth::login::{LoginError, LoginUx, run_login};
use bitrouter_ai::providers::login::PkceProvider;
use tokio::sync::{Mutex, Notify, oneshot};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct PendingPaste {
    dropped: Arc<AtomicBool>,
}

impl Drop for PendingPaste {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct CaptureUx {
    shown: Mutex<Option<oneshot::Sender<(String, bool)>>>,
    paste_started: Notify,
    paste_dropped: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl LoginUx for CaptureUx {
    async fn show_authorize_url(&self, url: &str, manual_only: bool) {
        if let Some(sender) = self.shown.lock().await.take() {
            let _ = sender.send((url.to_owned(), manual_only));
        }
    }

    async fn prompt_pasted_redirect(&self) -> Result<String, LoginError> {
        let _guard = PendingPaste {
            dropped: self.paste_dropped.clone(),
        };
        self.paste_started.notify_one();
        std::future::pending().await
    }
}

fn provider(port: Option<u16>, manual: Option<&'static str>) -> PkceProvider {
    PkceProvider {
        provider_id: "selected",
        loopback_port: port,
        redirect_path: "/callback",
        manual_redirect_uri: manual,
        auth: bitrouter_ai::auth::auth_code::AuthCodeParams {
            client_id: "caller-client".into(),
            authorize_endpoint: "https://selected.invalid/authorize".into(),
            token_endpoint: "https://selected.invalid/token".into(),
            scope: "caller-scope".into(),
            extra_authorize: Default::default(),
        },
    }
}

#[tokio::test]
async fn pkce_timeout_bounds_manual_fallback_and_releases_prompt() -> TestResult {
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = blocker.local_addr()?.port();
    let (sender, receiver) = oneshot::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let ux = CaptureUx {
        shown: Mutex::new(Some(sender)),
        paste_started: Notify::new(),
        paste_dropped: dropped.clone(),
    };
    let result = run_login(
        &reqwest::Client::new(),
        &provider(Some(port), Some("https://selected.invalid/manual")),
        &ux,
        Duration::from_millis(30),
    )
    .await;
    assert!(matches!(result, Err(LoginError::Timeout)));
    let (url, manual) = receiver.await?;
    assert!(manual);
    assert!(url.contains("caller-client"));
    assert!(dropped.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn cancelling_pkce_releases_actual_listener_and_pending_prompt() -> TestResult {
    let (sender, receiver) = oneshot::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let ux = Arc::new(CaptureUx {
        shown: Mutex::new(Some(sender)),
        paste_started: Notify::new(),
        paste_dropped: dropped.clone(),
    });
    let task_ux = ux.clone();
    let task = tokio::spawn(async move {
        run_login(
            &reqwest::Client::new(),
            &provider(None, None),
            task_ux.as_ref(),
            Duration::from_secs(10),
        )
        .await
    });
    let (url, manual) = receiver.await?;
    assert!(!manual);
    let url = url::Url::parse(&url)?;
    let redirect = url
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .ok_or("missing redirect URI")?;
    let port = url::Url::parse(&redirect)?
        .port()
        .ok_or("missing listener port")?;
    ux.paste_started.notified().await;
    task.abort();
    let error = task.await.err().ok_or("cancelled login completed")?;
    assert!(error.is_cancelled());
    assert!(dropped.load(Ordering::SeqCst));
    let _rebound = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    Ok(())
}

#[test]
fn public_pkce_and_device_code_debug_redact_login_secrets() {
    let pair = bitrouter_ai::auth::pkce::generate();
    assert!(!format!("{pair:?}").contains(&pair.verifier));
    let device = bitrouter_ai::auth::device_code::DeviceCodeResponse {
        device_code: "device-secret".into(),
        user_code: "user-secret".into(),
        verification_uri: "https://selected.invalid/verify".into(),
        verification_uri_complete: Some("https://selected.invalid/verify?code=user-secret".into()),
        interval: 1,
        expires_in: 1,
    };
    let debug = format!("{device:?}");
    assert!(!debug.contains("device-secret"));
    assert!(!debug.contains("user-secret"));
}

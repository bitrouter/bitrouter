//! Admission gate shared by the local control socket and HTTP server.
//!
//! A handoff token is issued only while no accepted operation is live. The
//! gate remains closed until the token is consumed, aborted, or expires.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

struct TrackedBody {
    inner: Pin<Box<Body>>,
    _admission: Admission,
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.get_mut().inner.as_mut().poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

const TOKEN_LIFETIME: Duration = Duration::from_secs(30);

#[derive(Default)]
struct GateState {
    active: usize,
    hold: Option<(String, Instant)>,
}

#[derive(Clone, Default)]
pub struct HandoffGate(Arc<Mutex<GateState>>);

pub struct Admission(HandoffGate);

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.active = state.active.saturating_sub(1);
    }
}

impl HandoffGate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        match self.0.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn admit(&self) -> Option<Admission> {
        let mut state = self.state();
        if state
            .hold
            .as_ref()
            .is_some_and(|(_, until)| *until <= Instant::now())
        {
            state.hold = None;
        }
        if state.hold.is_some() {
            return None;
        }
        state.active += 1;
        Some(Admission(self.clone()))
    }

    /// Passive, point-in-time observation for `bro status`.
    pub fn observed_activity(&self) -> (usize, bool) {
        let state = self.state();
        let held = state
            .hold
            .as_ref()
            .is_some_and(|(_, until)| *until > Instant::now());
        (state.active, held)
    }

    pub fn prepare(&self) -> Result<String, &'static str> {
        let mut state = self.state();
        if state
            .hold
            .as_ref()
            .is_some_and(|(_, until)| *until <= Instant::now())
        {
            state.hold = None;
        }
        if state.hold.is_some() {
            return Err("another handoff is in progress");
        }
        if state.active != 0 {
            return Err("daemon operations are in progress");
        }
        let token = uuid::Uuid::new_v4().to_string();
        state.hold = Some((token.clone(), Instant::now() + TOKEN_LIFETIME));
        Ok(token)
    }

    pub fn valid(&self, token: &str) -> bool {
        let state = self.state();
        state
            .hold
            .as_ref()
            .is_some_and(|(held, until)| held == token && *until > Instant::now())
    }

    pub fn abort(&self, token: &str) -> bool {
        let mut state = self.state();
        if !state.hold.as_ref().is_some_and(|(held, _)| held == token) {
            return false;
        }
        state.hold = None;
        true
    }
}

/// Hold admission until the response body is fully consumed or dropped. This
/// includes streaming inference responses, not only handler execution.
pub async fn http_admission(
    State(gate): State<HandoffGate>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(admission) = gate.admit() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon handoff in progress; retry",
        )
            .into_response();
    };
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(TrackedBody {
            inner: Box::pin(body),
            _admission: admission,
        }),
    )
}

use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::HandoffGate;

    #[tokio::test]
    async fn http_response_body_keeps_daemon_busy() -> anyhow::Result<()> {
        use axum::{Router, routing::get};
        use tower::ServiceExt;

        let gate = HandoffGate::default();
        let app = Router::new()
            .route("/health", get(|| async { "ready" }))
            .layer(axum::middleware::from_fn_with_state(
                gate.clone(),
                super::http_admission,
            ));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())?,
            )
            .await?;
        assert!(gate.prepare().is_err());
        drop(response);
        assert!(gate.prepare().is_ok());
        Ok(())
    }

    #[test]
    fn admission_and_prepare_are_atomic() {
        let gate = HandoffGate::default();
        let active = gate.admit();
        assert!(active.is_some());
        assert!(gate.prepare().is_err());
        drop(active);
        let token = gate.prepare();
        assert!(token.is_ok());
        if let Ok(token) = token {
            assert!(gate.valid(&token));
            assert!(gate.admit().is_none());
            assert!(gate.abort(&token));
            assert!(gate.admit().is_some());
        }
    }
}

//! One-shot loopback HTTP listener for the OAuth Authorization Code
//! redirect — a tiny `127.0.0.1` server that accepts exactly one request,
//! parses `?code=&state=` from its target line, replies with a short HTML
//! success page, and shuts down.
//!
//! Hand-rolled on top of [`tokio::net::TcpListener`] so this module pulls
//! no extra HTTP-server dependency on top of what `bitrouter-providers`
//! already needs for the OAuth side. The request shape we have to handle
//! is fixed (a browser redirect — `GET /<path>?<query> HTTP/1.1`), so a
//! one-line parser is enough.
//!
//! Bind strategy: caller passes a preferred port (some providers — Codex
//! — pin a specific port in the OAuth client registration). If the port
//! is taken, callers fall back to the manual-paste path; bind retries are
//! deliberately not attempted because a different port would mismatch
//! what's registered upstream.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the OAuth server placed on the redirect query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    /// Successful authorization — the `code` to exchange and the `state`
    /// to compare against the one we sent on `/authorize`.
    Success {
        /// Authorization code to exchange for tokens at the token endpoint.
        code: String,
        /// `state` parameter the server echoed back; the caller checks it
        /// matches what they sent on the authorize URL.
        state: Option<String>,
    },
    /// Server reported `error=` on the redirect (RFC 6749 §4.1.2.1). The
    /// caller maps the code to a user-facing message.
    Error {
        /// The `error` code (e.g. `access_denied`, `invalid_request`).
        error: String,
        /// Optional human-readable description the server provided.
        description: Option<String>,
        /// `state` parameter, if echoed.
        state: Option<String>,
    },
}

/// Errors raised by the loopback listener.
#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    /// `TcpListener::bind` failed — typically because the requested port
    /// is in use by another process.
    #[error("could not bind loopback listener on 127.0.0.1:{port}: {source}")]
    Bind {
        /// Port the caller asked for.
        port: u16,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Accept / read / write on the accepted connection failed.
    #[error("loopback listener I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The accepted connection didn't carry a recognisable request line.
    #[error("redirect request was malformed: {0}")]
    Malformed(String),
    /// The caller's `accept_one` future was cancelled (race / timeout).
    /// Useful for callers running the listener concurrently with a manual
    /// paste prompt.
    #[error("loopback listener timed out waiting for the redirect")]
    Timeout,
}

/// A bound listener waiting for the OAuth redirect callback.
///
/// Created by [`LoopbackListener::bind`]; consumed exactly once via
/// [`accept_one`](Self::accept_one). Hold the value around just long enough
/// to spawn the browser at [`redirect_uri`](Self::redirect_uri) — the
/// listener can be dropped if the caller decides to fall back to manual
/// paste before the redirect arrives.
pub struct LoopbackListener {
    listener: TcpListener,
    redirect_uri: String,
    redirect_path: String,
}

impl LoopbackListener {
    /// Bind on `127.0.0.1:<port>` with the given redirect path (e.g.
    /// `/auth/callback`). The returned [`redirect_uri`](Self::redirect_uri)
    /// is what you pass as `redirect_uri=` on the `/authorize` URL.
    pub async fn bind(port: u16, redirect_path: &str) -> Result<Self, ListenerError> {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ListenerError::Bind { port, source })?;
        let bound = listener
            .local_addr()
            .map_err(|source| ListenerError::Bind { port, source })?;
        let path = if redirect_path.starts_with('/') {
            redirect_path.to_string()
        } else {
            format!("/{redirect_path}")
        };
        let redirect_uri = format!("http://127.0.0.1:{}{}", bound.port(), path);
        Ok(Self {
            listener,
            redirect_uri,
            redirect_path: path,
        })
    }

    /// The full `http://127.0.0.1:<port>/<path>` URL to send as
    /// `redirect_uri` on the authorize request.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait up to `timeout` for the browser to hit the redirect, parse
    /// the query, send a short HTML success page, and return what we
    /// parsed. Times out → [`ListenerError::Timeout`].
    pub async fn accept_one(self, timeout: Duration) -> Result<CallbackOutcome, ListenerError> {
        let accept = async {
            let (mut stream, _peer) = self.listener.accept().await?;
            // Read a bounded amount — enough for the GET line and headers
            // of a redirect; protects against a misbehaving client filling
            // the buffer indefinitely.
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).await?;
            let head = std::str::from_utf8(&buf[..n])
                .map_err(|e| ListenerError::Malformed(format!("non-utf8 request: {e}")))?;
            let outcome = parse_redirect_query(head, &self.redirect_path)?;
            // Reply with a tiny HTML page so the browser shows "you may
            // close this tab" instead of a connection-reset error.
            let body = match &outcome {
                CallbackOutcome::Success { .. } => SUCCESS_BODY,
                CallbackOutcome::Error { .. } => ERROR_BODY,
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await.ok();
            Ok::<_, ListenerError>(outcome)
        };
        match tokio::time::timeout(timeout, accept).await {
            Ok(r) => r,
            Err(_) => Err(ListenerError::Timeout),
        }
    }
}

const SUCCESS_BODY: &str =
    "<!doctype html><meta charset=utf-8><title>bitrouter — signed in</title>\
     <body style='font:14px system-ui;padding:2rem;max-width:32rem'>\
     <h1>You're signed in 👍</h1>\
     <p>You can close this tab and return to your terminal.</p></body>";

const ERROR_BODY: &str =
    "<!doctype html><meta charset=utf-8><title>bitrouter — sign-in failed</title>\
     <body style='font:14px system-ui;padding:2rem;max-width:32rem'>\
     <h1>Sign-in failed</h1>\
     <p>The authorization server reported an error. Check the terminal for details.</p></body>";

/// Parse the redirect's query string out of a raw HTTP request head.
/// Pulled out so tests can hammer it without a real TCP connection.
fn parse_redirect_query(
    head: &str,
    expected_path: &str,
) -> Result<CallbackOutcome, ListenerError> {
    // The request line is the first line of the buffer. Shape:
    //   GET /auth/callback?code=…&state=… HTTP/1.1
    let request_line = head
        .lines()
        .next()
        .ok_or_else(|| ListenerError::Malformed("empty request".into()))?;
    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    let target = parts
        .next()
        .ok_or_else(|| ListenerError::Malformed(format!("no target in: {request_line}")))?;
    // `target` is `<path>?<query>` — split on the first `?`.
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    if path != expected_path {
        return Err(ListenerError::Malformed(format!(
            "unexpected path {path:?} (want {expected_path:?})"
        )));
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let decoded = percent_decode(v);
        match k {
            "code" => code = Some(decoded),
            "state" => state = Some(decoded),
            "error" => error = Some(decoded),
            "error_description" => error_description = Some(decoded),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Ok(CallbackOutcome::Error {
            error,
            description: error_description,
            state,
        });
    }
    let code = code.ok_or_else(|| {
        ListenerError::Malformed(format!("redirect missing both `code` and `error` in: {query}"))
    })?;
    Ok(CallbackOutcome::Success { code, state })
}

/// Minimal percent-decoder for query-string values. `+` is treated as
/// literal — the values OAuth servers send (codes, states, error names)
/// are already URL-safe, so this only has to undo `%XX` escapes the
/// server uses for the rare punctuation in error descriptions.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex(bytes[i + 1]);
            let lo = hex(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_redirect() {
        let head =
            "GET /auth/callback?code=abc-123&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let got = parse_redirect_query(head, "/auth/callback").unwrap();
        assert_eq!(
            got,
            CallbackOutcome::Success {
                code: "abc-123".into(),
                state: Some("xyz".into())
            }
        );
    }

    #[test]
    fn parses_error_redirect() {
        let head = "GET /auth/callback?error=access_denied&error_description=user%20clicked%20deny&state=xyz HTTP/1.1\r\n\r\n";
        let got = parse_redirect_query(head, "/auth/callback").unwrap();
        assert_eq!(
            got,
            CallbackOutcome::Error {
                error: "access_denied".into(),
                description: Some("user clicked deny".into()),
                state: Some("xyz".into())
            }
        );
    }

    #[test]
    fn rejects_wrong_path() {
        let head = "GET /elsewhere?code=x HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_redirect_query(head, "/auth/callback"),
            Err(ListenerError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_missing_code_without_error() {
        let head = "GET /auth/callback?state=only HTTP/1.1\r\n\r\n";
        assert!(matches!(
            parse_redirect_query(head, "/auth/callback"),
            Err(ListenerError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn binds_and_round_trips_a_real_request() {
        // Pick an OS-assigned port (0) so we can run this in parallel with
        // anything else on the dev box.
        let listener = LoopbackListener::bind(0, "/auth/callback").await.unwrap();
        let uri = listener.redirect_uri().to_string();
        let accept = tokio::spawn(listener.accept_one(Duration::from_secs(2)));
        let body = reqwest::get(format!("{uri}?code=hello&state=world"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("signed in"));
        let outcome = accept.await.unwrap().unwrap();
        assert_eq!(
            outcome,
            CallbackOutcome::Success {
                code: "hello".into(),
                state: Some("world".into())
            }
        );
    }

    #[tokio::test]
    async fn times_out_when_no_request_arrives() {
        let listener = LoopbackListener::bind(0, "/auth/callback").await.unwrap();
        let err = listener
            .accept_one(Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(matches!(err, ListenerError::Timeout));
    }
}

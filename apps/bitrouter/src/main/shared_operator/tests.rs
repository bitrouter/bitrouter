use super::*;
use bitrouter::output::{Format, error::envelope_from_anyhow};
use serde_json::json;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

#[path = "tests/http.rs"]
mod http;

const TOKEN: &str = "fixture-operator-cli-0123456789abcdef";

fn target(endpoint: &str) -> Target {
    Target {
        endpoint: endpoint.into(),
        token_env: "FIXTURE_OPERATOR".into(),
    }
}

fn input() -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"operation_id":"..", "command":{"action":"import_credential", "input":{
            "provider":"provider", "account":"default", "resource_pool_id":"pool", "expected_version":null,
            "credential":{"type":"api_key","value":"${LITERAL_SECRET}"}
        }}}),
    )?)
}

struct Counted {
    bytes: std::io::Cursor<Vec<u8>>,
    count: Arc<AtomicUsize>,
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.bytes).poll_read(cx, buffer);
        self.count
            .fetch_add(buffer.filled().len() - before, Ordering::Relaxed);
        result
    }
}

#[tokio::test]
async fn mutation_reader_is_bounded_literal_and_redacts_invalid_json() -> Result<()> {
    let limit = 2 * 1024 * 1024;
    let mut boundary = input()?;
    boundary.resize(limit, b' ');
    let decoded = read_mutation(&boundary[..]).await?;
    ensure!(decoded.operation_id == "..");
    let serialized = serde_json::to_value(decoded)?;
    ensure!(serialized["command"]["input"]["credential"]["value"] == "${LITERAL_SECRET}");
    let count = Arc::new(AtomicUsize::new(0));
    let error = read_mutation(Counted {
        bytes: std::io::Cursor::new(vec![b' '; limit + 1024]),
        count: count.clone(),
    })
    .await
    .err()
    .context("accepted oversized input")?;
    ensure!(error.to_string() == "operator mutation exceeds 2 MiB");
    ensure!(count.load(Ordering::Relaxed) == limit + 1);
    let mut trailing = input()?;
    trailing.extend_from_slice(b"{}");
    let valid = String::from_utf8(input()?)?;
    let duplicate = format!("{{\"operation_id\":\"SECRET_SENTINEL\",{}", &valid[1..]);
    let unknown = format!("{{\"unknown\":\"SECRET_SENTINEL\",{}", &valid[1..]);
    for invalid in [
        trailing,
        vec![255],
        duplicate.into_bytes(),
        br#"{"operation_id":"..","command":{"action":"import_credential","input":{"credential":"SECRET_SENTINEL"}}}"#.to_vec(),
        unknown.into_bytes(),
    ] {
        let error = read_mutation(&invalid[..]).await.err().context("accepted malformed input")?;
        for format in [Format::Json, Format::Human] {
            let text = String::from_utf8(Output::new(format).render_to_vec(&envelope_from_anyhow(&error)))?;
            ensure!(text.contains("invalid operator mutation JSON") && !text.contains("SECRET_SENTINEL"));
        }
    }
    Ok(())
}

#[tokio::test]
async fn target_and_token_reject_before_stdin_or_file_reads() -> Result<()> {
    for path in ["-", "/not-an-operator-input"] {
        for (endpoint, token, expected) in [
            (
                "http://SECRET_SENTINEL@localhost",
                Some(TOKEN.into()),
                "invalid client options: operator endpoint cannot contain userinfo, query or fragment",
            ),
            (
                "http://localhost",
                Some("SECRET_SENTINEL invalid".into()),
                "invalid client options: operator token must contain 32..4096 Bearer-compatible bytes",
            ),
            (
                "http://localhost",
                None,
                "operator token environment variable is unavailable",
            ),
        ] {
            let action = Action::Apply {
                target: target(endpoint),
                input: path.into(),
            };
            let error = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                execute(&action, |_| token.clone()),
            )
            .await?
            .err()
            .context("accepted invalid target/token")?;
            let text = format!("{error:#}");
            ensure!(text == expected, "wrong preflight failure");
        }
    }
    let action = Action::Apply {
        target: Target {
            endpoint: "http://localhost".into(),
            token_env: "${SECRET}".into(),
        },
        input: "-".into(),
    };
    let calls = std::cell::Cell::new(0);
    ensure!(
        execute(&action, |_| {
            calls.set(calls.get() + 1);
            None
        })
        .await
        .is_err()
    );
    ensure!(calls.get() == 0);
    Ok(())
}

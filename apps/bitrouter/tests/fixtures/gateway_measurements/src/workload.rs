use super::host::{Host, bounded_body};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

pub(super) const WARMUP: usize = 16;
pub(super) const REQUESTS: usize = 4096;
pub(super) const CONCURRENCY: usize = 8;

#[derive(Serialize)]
pub(super) struct Mode {
    elapsed_ms: f64,
    requests_per_second: f64,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
}

#[derive(Serialize)]
pub(super) struct Sample {
    label: String,
    round: usize,
    startup_ms: f64,
    idle_rss_kib: Vec<u64>,
    after_load_rss_kib: u64,
    ordinary: Mode,
    streaming: Mode,
}

async fn provider() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .and(body_partial_json(json!({"stream":false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"measurement","object":"chat.completion","model":"probe",
            "choices":[{"index":0,"message":{"role":"assistant","content":"measured response"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
        }))).mount(&server).await;
    let chunks = [
        json!({"id":"measurement","object":"chat.completion.chunk","model":"probe","choices":[{"index":0,"delta":{"role":"assistant","content":"measured response"},"finish_reason":null}]}),
        json!({"id":"measurement","object":"chat.completion.chunk","model":"probe","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}),
    ].into_iter().map(|chunk|format!("data: {chunk}\n\n")).collect::<String>() + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"stream":true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chunks),
        )
        .mount(&server)
        .await;
    server
}

async fn request(client: &reqwest::Client, endpoint: &str, streaming: bool) -> Result<f64> {
    let started = Instant::now();
    let response = client.post(format!("{endpoint}/v1/chat/completions"))
        .json(&json!({"model":"fixture:probe","stream":streaming,"messages":[{"role":"user","content":"Return a short fixed response."}]}))
        .send().await?;
    let status = response.status();
    if streaming && status.is_success() {
        let media_type = response
            .headers()
            .get("content-type")
            .context("stream media type missing")?
            .to_str()?;
        ensure!(
            media_type.split(';').next() == Some("text/event-stream"),
            "unexpected stream media type"
        );
    }
    let body = String::from_utf8(bounded_body(response, 64 * 1024).await?)?;
    ensure!(status.is_success(), "inference failed: {status}: {body}");
    if streaming {
        validate_stream(&body)?;
    } else {
        let value: Value = serde_json::from_str(&body)?;
        ensure!(value["choices"][0]["message"]["content"] == "measured response");
        ensure!(value["choices"][0]["finish_reason"] == "stop");
        validate_usage(&value["usage"])?;
    }
    Ok(started.elapsed().as_secs_f64() * 1000.0)
}

fn validate_usage(usage: &Value) -> Result<()> {
    ensure!(
        usage["prompt_tokens"] == 10
            && usage["completion_tokens"] == 2
            && usage["total_tokens"] == 12,
        "unexpected usage"
    );
    Ok(())
}

fn validate_stream(body: &str) -> Result<()> {
    // SSE dispatches data at a blank line; EOF alone does not dispatch an event.
    // https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream
    // This validates the fixture's small Chat stream, not arbitrary provider SSE.
    let body = body.replace("\r\n", "\n");
    ensure!(body.ends_with("\n\n"), "incomplete SSE frame");
    let mut content = String::new();
    let mut finished = false;
    let mut usage_seen = false;
    let mut done = false;
    for frame in body.split("\n\n").filter(|frame| !frame.is_empty()) {
        let mut data = Vec::new();
        for line in frame.lines() {
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => data.push(value),
                "event" | "id" | "retry" => {}
                _ => anyhow::bail!("unexpected SSE fixture field"),
            }
        }
        if data.is_empty() {
            continue;
        }
        ensure!(!done, "data after terminal DONE");
        let data = data.join("\n");
        if data == "[DONE]" {
            ensure!(finished && usage_seen, "DONE before completion and usage");
            done = true;
            continue;
        }
        let value: Value = serde_json::from_str(&data).context("invalid SSE JSON")?;
        let choices = value["choices"]
            .as_array()
            .context("missing stream choices")?;
        ensure!(choices.len() <= 1, "unexpected multiple stream choices");
        if let Some(choice) = choices.first() {
            ensure!(choice["index"] == 0, "unexpected choice index");
            if let Some(part) = choice["delta"]["content"].as_str() {
                ensure!(!finished, "content after completion");
                content.push_str(part);
            }
            if !choice["finish_reason"].is_null() {
                ensure!(
                    !finished && choice["finish_reason"] == "stop",
                    "unexpected or repeated completion"
                );
                finished = true;
            }
        }
        if !value["usage"].is_null() {
            ensure!(!usage_seen, "duplicate stream usage");
            validate_usage(&value["usage"])?;
            usage_seen = true;
        }
    }
    ensure!(done && finished && usage_seen, "incomplete stream");
    ensure!(
        content == "measured response",
        "incorrect reconstructed stream content"
    );
    Ok(())
}

async fn mode(client: &reqwest::Client, endpoint: &str, streaming: bool) -> Result<Mode> {
    let started = Instant::now();
    let mut times = stream::iter(0..REQUESTS)
        .map(|_| request(client, endpoint, streaming))
        .buffer_unordered(CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    let elapsed = started.elapsed().as_secs_f64();
    times.sort_by(f64::total_cmp);
    Ok(Mode {
        elapsed_ms: elapsed * 1000.0,
        requests_per_second: REQUESTS as f64 / elapsed,
        latency_p50_ms: times[times.len() / 2],
        latency_p95_ms: times[times.len() * 95 / 100],
    })
}

pub(super) async fn measure(binary: &Path, label: &str, round: usize) -> Result<Sample> {
    let provider = provider().await;
    let mut host = Host::start(binary, &provider.uri()).await?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    for i in 0..WARMUP {
        request(&client, &host.endpoint, i % 2 == 1).await?;
    }
    let mut idle_rss_kib = Vec::new();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        idle_rss_kib.push(host.rss_kib().await?);
    }
    let ordinary = mode(&client, &host.endpoint, false).await?;
    let streaming = mode(&client, &host.endpoint, true).await?;
    let after_load_rss_kib = host.rss_kib().await?;
    let calls = provider
        .received_requests()
        .await
        .context("upstream request log missing")?;
    ensure!(
        calls.len() == WARMUP + 2 * REQUESTS,
        "unexpected retries or missing dispatch: {}",
        calls.len()
    );
    let sample = Sample {
        label: label.into(),
        round,
        startup_ms: host.startup_ms,
        idle_rss_kib,
        after_load_rss_kib,
        ordinary,
        streaming,
    };
    host.stop().await?;
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> [String; 3] {
        [
            format!(
                "data: {}\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"measured response"},"finish_reason":null}]})
            ),
            format!(
                "data: {}\n\n",
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}})
            ),
            "data: [DONE]\n\n".into(),
        ]
    }

    #[test]
    fn exact_complete_stream_accepts_lf_and_crlf() -> Result<()> {
        let body = frames().concat();
        validate_stream(&body)?;
        validate_stream(&body.replace('\n', "\r\n"))?;
        Ok(())
    }

    #[test]
    fn truncated_corrupt_and_incomplete_streams_fail() {
        let [content, completed, done] = frames();
        for body in [
            format!("{content}{completed}"),
            format!("{content}{done}"),
            format!("{completed}{done}"),
            format!("{content}{content}{completed}{done}"),
            format!("{content}{completed}{done}{content}"),
            format!("{content}{completed}{done}{done}"),
            format!("{content}{completed}{done}").trim_end().to_owned(),
            format!("{content}data: invalid\n\n{completed}{done}"),
            format!("{content}{completed}{done}").replace("\"stop\"", "null"),
            format!("{content}{completed}{done}").replace("\"usage\"", "\"missing_usage\""),
            format!("{content}{completed}{done}")
                .replace("\"total_tokens\":12", "\"total_tokens\":13"),
        ] {
            assert!(
                validate_stream(&body).is_err(),
                "invalid stream was accepted: {body}"
            );
        }
    }
}

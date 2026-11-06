use super::host::{Host, bounded_body, http_client};
use super::metrics::{Sampler, Window};
use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream::FuturesUnordered};
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
pub(super) const OBSERVATION_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Serialize)]
struct Accounting {
    planned: usize,
    attempted: usize,
    not_started: usize,
    successful: usize,
    refused: usize,
    failed: usize,
}

#[derive(Debug)]
struct Refused(u16);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP refusal {}", self.0)
    }
}
impl std::error::Error for Refused {}

struct Timing {
    latency_ms: f64,
    ttft_ms: Option<f64>,
}

#[derive(Serialize)]
pub(super) struct Mode {
    elapsed_ms: f64,
    requests_per_second: f64,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
    latency_p99_ms: f64,
    ttft_p50_ms: Option<f64>,
    ttft_p95_ms: Option<f64>,
    accounting: Accounting,
    during_load_samples: usize,
    process: Window,
}

#[derive(Serialize)]
pub(super) struct Sample {
    label: String,
    round: usize,
    startup_ms: f64,
    idle_rss_kib: Vec<u64>,
    after_load_rss_kib: u64,
    warmed_idle: Window,
    after_load: Window,
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

async fn request(client: &reqwest::Client, endpoint: &str, streaming: bool) -> Result<Timing> {
    let started = Instant::now();
    let response = client.post(format!("{endpoint}/v1/chat/completions"))
        .json(&json!({"model":"fixture:probe","stream":streaming,"messages":[{"role":"user","content":"Return a short fixed response."}]}))
        .send().await?;
    let status = response.status();
    // Count a known HTTP refusal even if its error body would be invalid or
    // stall. No refused request contributes a successful latency or throughput.
    if !status.is_success() {
        return Err(Refused(status.as_u16()).into());
    }
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
    let ttft_ms = if streaming {
        let (body, first_content_ms) = stream_body(response, started).await?;
        validate_stream(&body)?;
        Some(first_content_ms.context("stream had no nonempty content frame")?)
    } else {
        let body = bounded_body(response, 64 * 1024).await?;
        let value: Value = serde_json::from_slice(&body)?;
        validate_success_envelope(&value)?;
        ensure!(value["choices"][0]["message"]["content"] == "measured response");
        ensure!(value["choices"][0]["finish_reason"] == "stop");
        validate_usage(&value["usage"])?;
        None
    };
    Ok(Timing {
        latency_ms: started.elapsed().as_secs_f64() * 1000.0,
        ttft_ms,
    })
}

async fn stream_body(
    mut response: reqwest::Response,
    started: Instant,
) -> Result<(String, Option<f64>)> {
    let mut body = Vec::new();
    let mut first_content_ms = None;
    while let Some(chunk) = response.chunk().await? {
        // Timestamp arrival, not validation or complete-body consumption.
        let arrived_ms = started.elapsed().as_secs_f64() * 1000.0;
        ensure!(
            chunk.len() <= 64 * 1024 - body.len(),
            "stream exceeds 64 KiB"
        );
        body.extend_from_slice(&chunk);
        if first_content_ms.is_none() && has_content_frame(&body)? {
            first_content_ms = Some(arrived_ms);
        }
    }
    Ok((String::from_utf8(body)?, first_content_ms))
}

fn has_content_frame(mut bytes: &[u8]) -> Result<bool> {
    // Only a complete frame can dispatch content. Ignore comments, role-only
    // and usage frames. UTF-8 may be split across network chunks.
    // https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream
    while let Some((end, delimiter)) = bytes.iter().enumerate().find_map(|(i, _)| {
        if bytes[i..].starts_with(b"\n\n") {
            Some((i, 2))
        } else if bytes[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else {
            None
        }
    }) {
        let frame = std::str::from_utf8(&bytes[..end])?;
        let data = frame
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .map(|value| value.strip_prefix(' ').unwrap_or(value))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !data.is_empty() && data != "[DONE]" {
            let value: Value = serde_json::from_str(&data)?;
            if value["choices"].as_array().is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice["delta"]["content"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                })
            }) {
                return Ok(true);
            }
        }
        bytes = &bytes[end + delimiter..];
    }
    Ok(false)
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

fn validate_success_envelope(value: &Value) -> Result<()> {
    ensure!(
        value["error"].is_null() && value["type"] != "error",
        "response contains an explicit error"
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
        let mut event = "";
        for line in frame.lines() {
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => data.push(value),
                "event" => event = value,
                "id" | "retry" => {}
                _ => anyhow::bail!("unexpected SSE fixture field"),
            }
        }
        ensure!(event != "error", "stream contains an explicit error event");
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
        validate_success_envelope(&value)?;
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

fn account(results: &[Result<Timing>], planned: usize) -> Result<Accounting> {
    ensure!(results.len() <= planned, "unexpected extra requests");
    let mut counts = Accounting {
        planned,
        attempted: results.len(),
        not_started: planned - results.len(),
        successful: 0,
        refused: 0,
        failed: 0,
    };
    for result in results {
        match result {
            Ok(_) => counts.successful += 1,
            Err(error) if error.downcast_ref::<Refused>().is_some() => counts.refused += 1,
            Err(_) => counts.failed += 1,
        }
    }
    ensure!(
        counts.successful > 0
            && counts.refused == 0
            && counts.failed == 0
            && counts.not_started == 0,
        "invalid measurement: {}; every request must succeed; first error: {}",
        serde_json::to_string(&counts)?,
        results
            .iter()
            .find_map(|v| v.as_ref().err())
            .map(ToString::to_string)
            .unwrap_or_else(|| "no requests".into())
    );
    Ok(counts)
}

async fn requests(
    client: &reqwest::Client,
    endpoint: &str,
    streaming: bool,
) -> Vec<Result<Timing>> {
    let mut pending = FuturesUnordered::new();
    let mut started = CONCURRENCY.min(REQUESTS);
    for _ in 0..started {
        pending.push(request(client, endpoint, streaming));
    }
    let mut results = Vec::with_capacity(REQUESTS);
    let mut accepting = true;
    while let Some(result) = pending.next().await {
        accepting &= result.is_ok();
        results.push(result);
        // Stop issuing on the first error; drain only the already-issued
        // requests so their refusals/failures remain counted, not cancelled.
        if accepting && started < REQUESTS {
            pending.push(request(client, endpoint, streaming));
            started += 1;
        }
    }
    results
}

async fn mode(
    client: &reqwest::Client,
    host: &mut Host,
    sampler: &Sampler,
    streaming: bool,
) -> Result<Mode> {
    let before = host.observe(sampler).await?;
    let endpoint = host.endpoint.clone();
    let (finished, mut done) = tokio::sync::oneshot::channel();
    // Poll the request workload independently of metric subprocesses. A slow
    // ps/lsof observation must not suspend body consumption or request timers.
    let load = async {
        let started = Instant::now();
        let results = requests(client, &endpoint, streaming).await;
        let elapsed = started.elapsed().as_secs_f64();
        if finished.send(()).is_err() {
            eprintln!("process observations ended before workload completion");
        }
        (results, elapsed)
    };
    let observe = async {
        let mut observations = vec![before];
        loop {
            tokio::select! {
                biased;
                _ = &mut done => break,
                _ = tokio::time::sleep(OBSERVATION_INTERVAL) => {
                    observations.push(host.observe(sampler).await?);
                }
            }
        }
        let during = observations.len() - 1;
        observations.push(host.observe(sampler).await?);
        Ok::<_, anyhow::Error>((during, Window::new(observations)))
    };
    let ((results, elapsed), process) = tokio::join!(load, observe);
    summarize(results, elapsed, streaming, process)
}

fn summarize(
    results: Vec<Result<Timing>>,
    elapsed: f64,
    streaming: bool,
    process: Result<(usize, Window)>,
) -> Result<Mode> {
    let accounting = account(&results, REQUESTS)?;
    ensure!(
        accounting.attempted == REQUESTS && elapsed > 0.0,
        "incomplete workload"
    );
    let (during_load_samples, process) = process?;
    let timings = results.into_iter().collect::<Result<Vec<_>>>()?;
    let mut times = timings.iter().map(|v| v.latency_ms).collect::<Vec<_>>();
    let mut ttft = timings.iter().filter_map(|v| v.ttft_ms).collect::<Vec<_>>();
    ensure!(
        ttft.len() == if streaming { REQUESTS } else { 0 },
        "missing TTFT observations"
    );
    times.sort_by(f64::total_cmp);
    ttft.sort_by(f64::total_cmp);
    Ok(Mode {
        elapsed_ms: elapsed * 1000.0,
        requests_per_second: REQUESTS as f64 / elapsed,
        latency_p50_ms: times[times.len() / 2],
        latency_p95_ms: times[times.len() * 95 / 100],
        latency_p99_ms: times[times.len() * 99 / 100],
        ttft_p50_ms: ttft.get(ttft.len() / 2).copied(),
        ttft_p95_ms: ttft.get(ttft.len() * 95 / 100).copied(),
        accounting,
        during_load_samples,
        process,
    })
}

pub(super) async fn measure(binary: &Path, label: &str, round: usize) -> Result<Sample> {
    let provider = provider().await;
    let mut host = Host::start(binary, &provider.uri()).await?;
    let client = http_client(Duration::from_secs(10))?;
    for i in 0..WARMUP {
        request(&client, &host.endpoint, i % 2 == 1)
            .await
            .with_context(|| format!("warmup request {} failed; no completed report", i + 1))?;
    }
    let sampler = Sampler::new().await;
    let mut idle = Vec::new();
    for _ in 0..3 {
        tokio::time::sleep(OBSERVATION_INTERVAL).await;
        idle.push(host.observe(&sampler).await?);
    }
    let idle_rss_kib = idle.iter().map(|v| v.rss_kib).collect();
    let ordinary = mode(&client, &mut host, &sampler, false).await?;
    let streaming = mode(&client, &mut host, &sampler, true).await?;
    let first_after = host.observe(&sampler).await?;
    let after_load_rss_kib = first_after.rss_kib;
    tokio::time::sleep(OBSERVATION_INTERVAL).await;
    let last_after = host.observe(&sampler).await?;
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
        warmed_idle: Window::new(idle),
        after_load: Window::new(vec![first_after, last_after]),
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

    #[test]
    fn explicit_errors_cannot_be_hidden_by_complete_streams() -> Result<()> {
        let [content, completed, done] = frames();
        for body in [
            format!("{content}{completed}event: error\n{done}"),
            format!("{content}event: error\n{completed}{done}"),
            format!(
                "{content}{completed}data: {{\"choices\":[],\"error\":{{\"message\":\"failed\"}}}}\n\n{done}"
            ),
            format!("{content}{completed}data: {{\"choices\":[],\"type\":\"error\"}}\n\n{done}"),
        ] {
            assert!(validate_stream(&body).is_err(), "accepted error: {body}");
            assert!(validate_stream(&body.replace('\n', "\r\n")).is_err());
        }
        validate_stream(&format!(
            "{content}{completed}data: {{\"choices\":[],\"error\":null}}\n\n{done}"
        ))?;
        Ok(())
    }

    #[tokio::test]
    async fn successful_http_status_cannot_hide_an_ordinary_error_envelope() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices":[{"message":{"content":"measured response"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12},
                "error":{"message":"failed"}
            })))
            .mount(&server)
            .await;
        let client = http_client(Duration::from_secs(2))?;
        let result = request(&client, &server.uri(), false).await;
        let counts = account(&[result], 1)
            .err()
            .context("ordinary error envelope was accepted")?
            .to_string();
        assert!(counts.contains("\"successful\":0"), "{counts}");
        assert!(counts.contains("\"refused\":0"), "{counts}");
        assert!(counts.contains("\"failed\":1"), "{counts}");
        Ok(())
    }

    #[tokio::test]
    async fn redirects_are_refused_without_following_successful_targets() -> Result<()> {
        let server = MockServer::start().await;
        for streaming in [false, true] {
            let target = if streaming { "/stream" } else { "/ordinary" };
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .and(body_partial_json(json!({"stream":streaming})))
                .respond_with(
                    ResponseTemplate::new(307)
                        .insert_header("location", format!("{}{target}", server.uri())),
                )
                .mount(&server)
                .await;
            let success = if streaming {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(frames().concat())
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices":[{"message":{"content":"measured response"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
                }))
            };
            Mock::given(method("POST"))
                .and(path(target))
                .respond_with(success)
                .mount(&server)
                .await;
        }
        let client = http_client(Duration::from_secs(2))?;
        for streaming in [false, true] {
            let error = request(&client, &server.uri(), streaming)
                .await
                .err()
                .context("redirect was followed")?;
            assert!(matches!(
                error.downcast_ref::<Refused>(),
                Some(Refused(307))
            ));
        }
        let calls = server
            .received_requests()
            .await
            .context("missing request log")?;
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| call.url.path() == "/v1/chat/completions")
        );
        Ok(())
    }

    #[test]
    fn mode_report_preserves_the_total_latency_p99_tail() -> Result<()> {
        for streaming in [false, true] {
            let results = (0..REQUESTS)
                .rev()
                .map(|index| {
                    Ok(Timing {
                        latency_ms: match index {
                            0..3880 => 1.0,
                            3880..4050 => 10.0,
                            _ => 1000.0,
                        },
                        ttft_ms: streaming.then_some(0.5),
                    })
                })
                .collect();
            let report = serde_json::to_value(summarize(
                results,
                4.0,
                streaming,
                Ok((0, Window::new(Vec::new()))),
            )?)?;
            assert_eq!(report["latency_p50_ms"], 1.0);
            assert_eq!(report["latency_p95_ms"], 10.0);
            assert_eq!(report["latency_p99_ms"], 1000.0);
            assert_eq!(report["accounting"]["successful"], REQUESTS);
            assert_eq!(report["accounting"]["failed"], 0);
            assert_eq!(report["requests_per_second"], 1024.0);
            if streaming {
                assert_eq!(report["ttft_p95_ms"], 0.5);
            } else {
                assert!(report["ttft_p50_ms"].is_null());
                assert!(report["ttft_p95_ms"].is_null());
            }
        }
        Ok(())
    }

    #[test]
    fn first_content_waits_for_complete_frame_and_ignores_metadata() -> Result<()> {
        let metadata =
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n: keepalive\n\n";
        assert!(!has_content_frame(metadata.as_bytes())?);
        assert!(!has_content_frame(b"data: [DONE]\n\n")?);
        for frame in [
            "data: {\"choices\":[{\"delta\":{\"content\":\"\u{4e2d}\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"text\"}}]}\r\n\r\n",
        ] {
            for end in 0..frame.len() {
                let mut input = metadata.as_bytes().to_vec();
                input.extend_from_slice(&frame.as_bytes()[..end]);
                assert!(!has_content_frame(&input)?, "partial frame at byte {end}");
            }
            assert!(has_content_frame(frame.as_bytes())?);
        }
        assert!(has_content_frame(b"data: invalid\n\n").is_err());
        Ok(())
    }

    #[test]
    fn refused_failed_and_zero_success_never_form_a_successful_mode() -> Result<()> {
        let success = || {
            Ok(Timing {
                latency_ms: 1.0,
                ttft_ms: None,
            })
        };
        assert_eq!(account(&[success(), success()], 2)?.successful, 2);
        let error = account(
            &[
                success(),
                Err(Refused(429).into()),
                Err(anyhow::anyhow!("truncated body")),
            ],
            4,
        )
        .err()
        .context("mixed failures were accepted")?
        .to_string();
        for count in [
            "\"attempted\":3",
            "\"successful\":1",
            "\"refused\":1",
            "\"failed\":1",
            "\"not_started\":1",
        ] {
            assert!(error.contains(count), "{error}");
        }
        assert!(account(&[], 0).is_err());
        assert!(account(&[success()], 2).is_err());
        assert!(account(&[Err(Refused(503).into())], 1).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn incremental_stream_body_keeps_completion_validation() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(frames().concat()))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let started = Instant::now();
        let (body, ttft) = stream_body(client.get(server.uri()).send().await?, started).await?;
        validate_stream(&body)?;
        let ttft = ttft.context("missing first content timestamp")?;
        assert!(ttft >= 0.0 && ttft <= started.elapsed().as_secs_f64() * 1000.0);
        Ok(())
    }

    #[tokio::test]
    async fn refusal_stops_new_requests_but_accounts_for_every_inflight_request() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let results = requests(&client, &server.uri(), false).await;
        assert_eq!(results.len(), CONCURRENCY);
        assert!(results.iter().all(|r| {
            r.as_ref()
                .err()
                .is_some_and(|e| e.downcast_ref::<Refused>().is_some())
        }));
        assert_eq!(
            server
                .received_requests()
                .await
                .context("missing requests")?
                .len(),
            CONCURRENCY
        );
        assert!(account(&results, REQUESTS).is_err());
        Ok(())
    }
}

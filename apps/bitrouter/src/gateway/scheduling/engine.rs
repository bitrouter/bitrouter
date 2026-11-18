//! Compiled-in vLLM and SGLang modules. The scheduler sees only normalized samples.

use std::collections::BTreeMap;
use std::time::Duration;

use bitrouter_ai::types::Prompt;
use tokio::time::Instant;

use super::observation::output_allowance;
use crate::config::inference_routing::{EngineConfig, EngineModule};
use crate::gateway::{
    error::Result,
    model::protocol::{OutboundAdapter, chat_completions::ChatCompletionsAdapter},
    pipeline::types::RoutingTarget,
};

const BODY_LIMIT: usize = 1_048_576;
const COUNT_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EngineSample {
    pub(super) running: u64,
    pub(super) waiting: u64,
    pub(super) kv_millis: u16,
    pub(super) at: Instant,
}

pub(super) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .build()
        .map_err(|_| super::unavailable())
}

pub(super) async fn run(inner: std::sync::Arc<super::Inner>, client: reqwest::Client) {
    let mut delay = Duration::ZERO;
    loop {
        tokio::select! {
            () = inner.stop.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        if inner.stop.is_cancelled() {
            return;
        }
        let targets: Vec<(usize, EngineConfig)> = {
            let state = inner.lock();
            state
                .destinations
                .iter()
                .enumerate()
                .filter_map(|(index, destination)| {
                    destination
                        .config
                        .engine
                        .clone()
                        .map(|engine| (index, engine))
                })
                .collect()
        };
        let period = targets
            .iter()
            .map(|(_, engine)| engine.max_metric_age_ms)
            .min()
            .unwrap_or(1_000)
            .clamp(50, 60_000);
        delay = Duration::from_millis(u64::from(period));
        for (index, engine) in targets {
            if inner.stop.is_cancelled() {
                return;
            }
            let sample = scrape_metrics(&client, &engine).await;
            {
                let mut state = inner.lock();
                if let Some(destination) = state.destinations.get_mut(index) {
                    destination.engine_sample = sample;
                }
            }
            inner.changed.notify_one();
        }
    }
}

pub(super) async fn scrape_metrics(
    client: &reqwest::Client,
    engine: &EngineConfig,
) -> Option<EngineSample> {
    let mut request = client
        .get(&engine.metrics_url)
        .timeout(Duration::from_millis(u64::from(engine.max_metric_age_ms)));
    match resolve_bearer(engine.metrics_bearer_env.as_deref()) {
        Bearer::Absent => {}
        Bearer::Value(value) => {
            let mut header =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")).ok()?;
            header.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, header);
        }
        Bearer::Rejected => return None,
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = read_limited(response, BODY_LIMIT).await?;
    let text = std::str::from_utf8(&body).ok()?;
    let normalized = normalize(engine.module, text)?;
    Some(EngineSample {
        running: normalized.running,
        waiting: normalized.waiting,
        kv_millis: normalized.kv_millis,
        at: Instant::now(),
    })
}

/// Count is a context gate. Failure refuses this destination and does not fall back.
pub(super) async fn admit_count(
    client: &reqwest::Client,
    engine: &EngineConfig,
    endpoint: &str,
    credential: &str,
    prompt: &Prompt,
    deadline: Instant,
) -> Result<()> {
    if !engine.count_input {
        return Ok(());
    }
    match engine.module {
        EngineModule::Sglang => Ok(()),
        EngineModule::Vllm => {
            match count_vllm(client, endpoint, credential, prompt, deadline).await {
                Count::Counted {
                    input_tokens,
                    max_context_tokens,
                } => {
                    let allowance = output_allowance(prompt.params.max_tokens);
                    if input_tokens > 0
                        && input_tokens
                            .checked_add(allowance)
                            .is_some_and(|total| total <= max_context_tokens)
                    {
                        Ok(())
                    } else {
                        Err(super::unavailable())
                    }
                }
                Count::Failed => Err(super::unavailable()),
            }
        }
    }
}

enum Count {
    Counted {
        input_tokens: u64,
        max_context_tokens: u64,
    },
    Failed,
}

async fn count_vllm(
    client: &reqwest::Client,
    endpoint: &str,
    credential: &str,
    prompt: &Prompt,
    deadline: Instant,
) -> Count {
    let Some(url) = tokenize_url(endpoint) else {
        return Count::Failed;
    };
    let Ok(body) = ChatCompletionsAdapter.render_request(prompt) else {
        return Count::Failed;
    };
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Count::Failed;
    }
    let mut request = client
        .post(url)
        .timeout(timeout.min(COUNT_LIMIT))
        .json(&body);
    if !credential.is_empty() {
        let Ok(mut header) =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {credential}"))
        else {
            return Count::Failed;
        };
        header.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, header);
    }
    let Ok(response) = request.send().await else {
        return Count::Failed;
    };
    if !response.status().is_success() {
        return Count::Failed;
    }
    let Some(bytes) = read_limited(response, BODY_LIMIT).await else {
        return Count::Failed;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Count::Failed;
    };
    let Some(count) = value.get("count").and_then(json_u64) else {
        return Count::Failed;
    };
    let Some(max_context_tokens) = value.get("max_model_len").and_then(json_u64) else {
        return Count::Failed;
    };
    let Some(tokens) = value.get("tokens").and_then(serde_json::Value::as_array) else {
        return Count::Failed;
    };
    if count == 0 || tokens.len() as u64 != count || max_context_tokens == 0 {
        return Count::Failed;
    }
    Count::Counted {
        input_tokens: count,
        max_context_tokens,
    }
}

fn tokenize_url(endpoint: &str) -> Option<String> {
    let mut url = url::Url::parse(endpoint).ok()?;
    url.set_path("/tokenize");
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > limit {
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Some(body),
            Err(_) => return None,
        }
    }
}

enum Bearer {
    Absent,
    Value(String),
    Rejected,
}

fn resolve_bearer(name: Option<&str>) -> Bearer {
    let Some(name) = name else {
        return Bearer::Absent;
    };
    match std::env::var(name) {
        Ok(value) if bearer_value_ok(&value) => Bearer::Value(value),
        _ => Bearer::Rejected,
    }
}

fn bearer_value_ok(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(' ')
        && !value.ends_with(' ')
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

struct Normalized {
    running: u64,
    waiting: u64,
    kv_millis: u16,
}

fn normalize(module: EngineModule, text: &str) -> Option<Normalized> {
    let series = parse_exposition(text);
    match module {
        EngineModule::Vllm => {
            let running = whole_sum(select(&series, "vllm:num_requests_running", false))?;
            let waiting = whole_sum(select(&series, "vllm:num_requests_waiting", false))?;
            let kv_millis = max_fraction(select(&series, "vllm:kv_cache_usage_perc", false))?;
            Some(Normalized {
                running,
                waiting,
                kv_millis,
            })
        }
        EngineModule::Sglang => {
            let running = whole_sum(select(&series, "sglang:num_running_reqs", true))?;
            let waiting = whole_sum(select(&series, "sglang:num_queue_reqs", true))?;
            let kv_millis = max_fraction(select(&series, "sglang:token_usage", true))?;
            Some(Normalized {
                running,
                waiting,
                kv_millis,
            })
        }
    }
}

struct Series {
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
}

fn select<'a>(series: &'a [Series], name: &str, priority: bool) -> Vec<&'a Series> {
    let matched: Vec<_> = series.iter().filter(|series| series.name == name).collect();
    if !priority {
        return matched;
    }
    if matched
        .iter()
        .any(|series| series.labels.contains_key("priority"))
    {
        matched
            .into_iter()
            .filter(|series| series.labels.get("priority").map(String::as_str) == Some(""))
            .collect()
    } else {
        matched
    }
}

fn whole_sum(series: Vec<&Series>) -> Option<u64> {
    if series.is_empty() {
        return None;
    }
    let mut total = 0u64;
    for series in series {
        let rounded = series.value.round();
        if !series.value.is_finite()
            || series.value < 0.0
            || (series.value - rounded).abs() > 1e-6
            || rounded > u64::MAX as f64
        {
            return None;
        }
        total = total.checked_add(rounded as u64)?;
    }
    Some(total)
}

fn max_fraction(series: Vec<&Series>) -> Option<u16> {
    if series.is_empty() {
        return None;
    }
    let mut max = 0u16;
    for series in series {
        if !series.value.is_finite() || !(0.0..=1.0).contains(&series.value) {
            return None;
        }
        let millis = (series.value * 1000.0).round();
        if !(0.0..=1000.0).contains(&millis) {
            return None;
        }
        max = max.max(millis as u16);
    }
    Some(max)
}

fn parse_exposition(text: &str) -> Vec<Series> {
    let mut series = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(parsed) = parse_sample(line) {
            series.push(parsed);
        }
    }
    series
}

fn parse_sample(line: &str) -> Option<Series> {
    let mut rest = line;
    let name = take_name(&mut rest)?;
    let labels = if rest.starts_with('{') {
        take_labels(&mut rest)?
    } else {
        BTreeMap::new()
    };
    let value = rest.split_whitespace().next()?.parse::<f64>().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some(Series {
        name,
        labels,
        value,
    })
}

fn take_name(input: &mut &str) -> Option<String> {
    let length = input
        .char_indices()
        .take_while(|(index, ch)| {
            if *index == 0 {
                ch.is_ascii_alphabetic() || *ch == '_' || *ch == ':'
            } else {
                ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':'
            }
        })
        .map(|(index, ch)| index + ch.len_utf8())
        .last()?;
    let name = input[..length].to_string();
    *input = &input[length..];
    Some(name)
}

fn take_labels(input: &mut &str) -> Option<BTreeMap<String, String>> {
    *input = input.strip_prefix('{')?;
    let mut labels = BTreeMap::new();
    if let Some(rest) = input.strip_prefix('}') {
        *input = rest;
        return Some(labels);
    }
    loop {
        let key = take_name(input)?;
        *input = input.strip_prefix('=')?.trim_start();
        *input = input.strip_prefix('"')?;
        let mut value = String::new();
        let mut chars = input.chars();
        loop {
            match chars.next()? {
                '\\' => {
                    let escaped = chars.next()?;
                    value.push(match escaped {
                        'n' => '\n',
                        '\\' => '\\',
                        '"' => '"',
                        other => other,
                    });
                }
                '"' => break,
                ch => value.push(ch),
            }
        }
        *input = chars.as_str().trim_start();
        labels.insert(key, value);
        if let Some(rest) = input.strip_prefix('}') {
            *input = rest;
            return Some(labels);
        }
        *input = input.strip_prefix(',')?.trim_start();
    }
}

pub(super) fn credential(target: &RoutingTarget) -> &str {
    target.effective_api_key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::inference_routing::KvUsage;

    fn normalized(
        module: EngineModule,
        text: &str,
    ) -> std::result::Result<Normalized, &'static str> {
        normalize(module, text).ok_or("engine sample was unavailable")
    }

    #[test]
    fn vllm_usage_stays_a_fraction_and_sums_engine_series() -> std::result::Result<(), &'static str>
    {
        let text = r#"
vllm:num_requests_running{engine="0",model_name="m"} 2
vllm:num_requests_running{engine="1",model_name="m"} 3
vllm:num_requests_waiting{engine="0"} 1
vllm:num_requests_waiting{engine="1"} 0
vllm:kv_cache_usage_perc{engine="0"} 0.20
vllm:kv_cache_usage_perc{engine="1"} 0.80
"#;
        let sample = normalized(EngineModule::Vllm, text)?;
        assert_eq!(
            (sample.running, sample.waiting, sample.kv_millis),
            (5, 1, 800)
        );
        Ok(())
    }

    #[test]
    fn dividing_vllm_usage_is_not_the_module_contract() -> std::result::Result<(), &'static str> {
        let text = "vllm:num_requests_running 0\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.95\n";
        assert_eq!(normalized(EngineModule::Vllm, text)?.kv_millis, 950);
        Ok(())
    }

    #[test]
    fn sglang_reads_the_empty_priority_total_and_keeps_token_usage()
    -> std::result::Result<(), &'static str> {
        let text = r#"
sglang:num_running_reqs{priority=""} 4
sglang:num_running_reqs{priority="1"} 4
sglang:num_queue_reqs{priority=""} 2
sglang:num_queue_reqs{priority="1"} 2
sglang:num_grammar_queue_reqs 9
sglang:token_usage{priority=""} 0.28
sglang:token_usage{priority="1"} 0.9
"#;
        let sample = normalized(EngineModule::Sglang, text)?;
        assert_eq!(
            (sample.running, sample.waiting, sample.kv_millis),
            (4, 2, 280)
        );
        Ok(())
    }

    #[test]
    fn a_missing_or_scaled_gauge_is_unavailable() {
        assert!(normalize(EngineModule::Vllm, "vllm:num_requests_running 1\n").is_none());
        assert!(normalize(
            EngineModule::Vllm,
            "vllm:num_requests_running 1\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 95\n"
        )
        .is_none());
        assert!(
            normalize(
                EngineModule::Sglang,
                "sglang:num_running_reqs 1\nsglang:num_queue_reqs 0\nsglang:token_usage 1.2\n"
            )
            .is_none()
        );
    }

    #[test]
    fn bearer_values_reject_breaks_and_padding() {
        assert!(bearer_value_ok("metrics-token"));
        assert!(!bearer_value_ok(""));
        assert!(!bearer_value_ok(" token"));
        assert!(!bearer_value_ok("token\n"));
    }

    #[test]
    fn tokenize_replaces_the_endpoint_path() {
        assert_eq!(
            tokenize_url("http://127.0.0.1:8000/v1").as_deref(),
            Some("http://127.0.0.1:8000/tokenize")
        );
    }

    #[test]
    fn kv_ceiling_rejects_zero_and_extra_precision() -> std::result::Result<(), &'static str> {
        let usage = serde_json::from_value::<KvUsage>(serde_json::json!(0.8))
            .map_err(|_| "expected 0.8")?;
        assert_eq!(usage.millis(), 800);
        assert!(serde_json::from_value::<KvUsage>(serde_json::json!(0)).is_err());
        assert!(serde_json::from_value::<KvUsage>(serde_json::json!(1.0001)).is_err());
        assert!(serde_json::from_value::<KvUsage>(serde_json::json!(1)).is_ok());
        Ok(())
    }

    fn engine_fixture(
        metrics: &str,
        bearer: Option<&str>,
    ) -> std::result::Result<EngineConfig, String> {
        let mut value = serde_json::json!({
            "module": "vllm",
            "metrics_url": metrics,
            "max_metric_age_ms": 1000,
            "max_running": 4,
            "max_waiting": 4,
            "max_kv_usage": 0.9
        });
        if let Some(name) = bearer {
            value["metrics_bearer_env"] = serde_json::Value::String(name.to_owned());
        }
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn prompt() -> std::result::Result<bitrouter_ai::types::Prompt, String> {
        serde_json::from_value(serde_json::json!({
            "model": "local-model",
            "stream": false,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
        }))
        .map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn scrape_keeps_the_fraction_and_a_redirect_stays_unavailable()
    -> std::result::Result<(), String> {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/metrics"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
                "vllm:num_requests_running 1\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.95\n",
            ))
            .mount(&server)
            .await;
        let client = client().map_err(|_| "metrics client")?;
        let engine = engine_fixture(&format!("{}/metrics", server.uri()), None)?;
        let sample = scrape_metrics(&client, &engine)
            .await
            .ok_or("scrape produced no sample")?;
        assert_eq!(
            (sample.running, sample.waiting, sample.kv_millis),
            (1, 0, 950)
        );
        let requests = server.received_requests().await.ok_or("no request log")?;
        let authorization = requests
            .first()
            .and_then(|request| request.headers.get("authorization"));
        assert!(authorization.is_none());

        let redirected = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("location", "http://127.0.0.1:1/other"),
            )
            .mount(&redirected)
            .await;
        let shifted = engine_fixture(&format!("{}/metrics", redirected.uri()), None)?;
        assert!(scrape_metrics(&client, &shifted).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_metrics_bearer_sends_nothing() -> std::result::Result<(), String> {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let client = client().map_err(|_| "metrics client")?;
        let engine = engine_fixture(
            &format!("{}/metrics", server.uri()),
            Some("BITROUTER_ENGINE_METRICS_BEARER_ABSENT"),
        )?;
        assert!(scrape_metrics(&client, &engine).await.is_none());
        let requests = server.received_requests().await.ok_or("no request log")?;
        assert!(requests.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn vllm_count_must_fit_and_a_mismatch_does_not_admit() -> std::result::Result<(), String>
    {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/tokenize"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "count": 2,
                    "max_model_len": 5000,
                    "tokens": [1, 2]
                })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/tokenize"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "count": 2,
                    "max_model_len": 5000,
                    "tokens": [1]
                })),
            )
            .mount(&server)
            .await;
        let client = client().map_err(|_| "count client")?;
        let engine = engine_fixture(&format!("{}/metrics", server.uri()), None)?;
        let prompt = prompt()?;
        let endpoint = format!("{}/v1", server.uri());
        admit_count(
            &client,
            &engine,
            &endpoint,
            "inference-key",
            &prompt,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await
        .map_err(|error| error.to_string())?;
        admit_count(
            &client,
            &engine,
            &endpoint,
            "inference-key",
            &prompt,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await
        .err()
        .ok_or("mismatched token array was admitted")?;
        let requests = server.received_requests().await.ok_or("no request log")?;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer inference-key")
        }));
        Ok(())
    }
}

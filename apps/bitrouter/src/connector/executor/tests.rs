use super::*;
use anyhow::{Result, ensure};
use axum::{
    Router,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn request(stream: bool) -> Result<LocalRequest> {
    Ok(serde_json::from_value(serde_json::json!({
        "request_id":"one","pool_id":"pool","pool_generation":"generation",
        "deadline_ms":1000,"request":{"model":"fixture-model",
        "messages":[{"role":"user","content":"private prompt"}],"max_tokens":4,"stream":stream}
    }))?)
}

fn completion() -> Value {
    serde_json::json!({"id":"chat-one","model":"fixture-model","choices":[{
        "index":0,"message":{"role":"assistant","content":"ok","tool_calls":[],"function_call":null},"finish_reason":"stop"
    }],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
}

#[test]
fn completion_requires_bound_single_choice_usage_and_finish() -> Result<()> {
    let request = request(false)?;
    validate_completion(&serde_json::to_vec(&completion())?, &request, 3)
        .map_err(anyhow::Error::msg)?;
    let mut failures = Vec::new();
    for field in ["id", "model", "usage", "choices"] {
        let mut value = completion();
        value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("fixture object"))?
            .remove(field);
        failures.push(value);
    }
    for (path, value) in [
        ("/model", serde_json::json!("other-model")),
        ("/choices/0/finish_reason", Value::Null),
        ("/choices/0/finish_reason", serde_json::json!("tool_calls")),
        ("/choices/0/index", serde_json::json!(1)),
        (
            "/choices/0/message/content",
            serde_json::json!({"unsupported":"shape"}),
        ),
        (
            "/choices/0/message/tool_calls",
            serde_json::json!([{"id":"tool-one"}]),
        ),
        ("/choices/0/message/tool_calls", serde_json::json!({})),
        (
            "/choices/0/message/function_call",
            serde_json::json!({"name":"tool-one"}),
        ),
        ("/usage/prompt_tokens", serde_json::json!(2)),
        ("/usage/completion_tokens", serde_json::json!(5)),
    ] {
        let mut broken = completion();
        *broken
            .pointer_mut(path)
            .ok_or_else(|| anyhow::anyhow!("fixture path"))? = value;
        failures.push(broken);
    }
    let mut multiple = completion();
    multiple["choices"] = serde_json::json!([
        multiple["choices"][0].clone(),
        multiple["choices"][0].clone()
    ]);
    failures.push(multiple);
    for value in failures {
        ensure!(validate_completion(&serde_json::to_vec(&value)?, &request, 3).is_err());
    }
    for absent in [Value::Null, serde_json::json!([])] {
        let mut value = completion();
        value["choices"][0]["message"]["tool_calls"] = absent;
        validate_completion(&serde_json::to_vec(&value)?, &request, 3)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

#[test]
fn request_shape_cannot_override_token_or_target_boundaries() -> Result<()> {
    let base = serde_json::to_value(request(false)?)?;
    for (name, value) in [
        ("input_tokens", serde_json::json!(1)),
        ("n", serde_json::json!(2)),
        ("chat_template", serde_json::json!("override")),
        ("max_completion_tokens", serde_json::json!(99999)),
        ("tools", serde_json::json!([])),
        ("base_url", serde_json::json!("http://elsewhere")),
    ] {
        let mut invalid = base.clone();
        invalid["request"][name] = value;
        ensure!(serde_json::from_value::<LocalRequest>(invalid).is_err());
    }
    let local = request(true)?;
    let counting = local.tokenize_body();
    let inference = local.inference_body();
    for field in [
        "model",
        "messages",
        "add_generation_prompt",
        "continue_final_message",
        "add_special_tokens",
    ] {
        ensure!(counting[field] == inference[field]);
    }
    ensure!(inference["max_tokens"] == 4 && inference["n"] == 1);
    Ok(())
}

fn chunk(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id":"chat-one","model":"fixture-model",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        })
    )
}

fn valid_stream() -> String {
    format!(
        "{}{}data: {}\n\ndata: [DONE]\n\n",
        chunk(
            serde_json::json!({"content":"ok","tool_calls":[],"function_call":null}),
            Value::Null
        ),
        chunk(
            serde_json::json!({"content":null,"tool_calls":[],"function_call":null}),
            serde_json::json!("stop")
        ),
        serde_json::json!({"id":"chat-one","model":"fixture-model","choices":[],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
    )
}

async fn check_stream(body: String, limit: usize) -> Result<Result<(), &'static str>> {
    let app = Router::new().route(
        "/",
        get(move || {
            let body = body.clone();
            async move { ([("content-type", "text/event-stream")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::builder().no_proxy().build()?;
    let model = ModelClient::with_http_client(
        HttpTimeouts::default(),
        std::sync::Arc::new(OutboundDispatch::builtin()),
        client.clone(),
    );
    let executor = Executor {
        client: client.clone(),
        model,
    };
    let response = client.get(format!("http://{address}/")).send().await?;
    let (send, mut receive) = mpsc::channel(32);
    let drain = tokio::spawn(async move { while receive.recv().await.is_some() {} });
    let result = executor
        .stream(response, &request(true)?, 3, limit, &send)
        .await;
    drop(send);
    drain.await?;
    task.abort();
    Ok(result)
}

#[tokio::test]
async fn strict_stream_terminal_rejects_truncation_extra_data_and_usage_violations() -> Result<()> {
    ensure!(check_stream(valid_stream(), 8192).await?.is_ok());
    for broken in [
        valid_stream().replace("data: [DONE]\n\n", ""),
        "data: [DONE]\n\n".to_owned(),
        format!("{}data: {{broken", valid_stream()),
        format!("{}data: {{broken}}\n\n", valid_stream()),
        format!("{}malformed trailer\n\n", valid_stream()),
        format!("data: {{broken}}\n\n{}", valid_stream()),
        valid_stream().replace("\"completion_tokens\":2", "\"completion_tokens\":99"),
        valid_stream().replace("\"prompt_tokens\":3", "\"prompt_tokens\":2"),
        valid_stream().replace("\"index\":0", "\"index\":1"),
        valid_stream().replace("\"model\":\"fixture-model\"", "\"model\":\"wrong-model\""),
        format!(
            "{}{}",
            valid_stream(),
            chunk(serde_json::json!({"content":"late"}), Value::Null)
        ),
    ] {
        ensure!(check_stream(broken, 8192).await?.is_err());
    }
    ensure!(check_stream(valid_stream(), 32).await?.is_err());
    Ok(())
}

#[derive(Clone)]
enum CountReply {
    Json(Value),
    Raw(String),
    Redirect,
    Delay,
}

async fn check_count(
    reply: CountReply,
    timeout_ms: u64,
) -> Result<(Result<u32, &'static str>, Value, usize)> {
    let captured = Arc::new(Mutex::new(Value::Null));
    let capture = captured.clone();
    let escaped = Arc::new(AtomicUsize::new(0));
    let escaped_counter = escaped.clone();
    let app = Router::new()
        .route("/tokenize", post(move |axum::Json(body): axum::Json<Value>| {
            let capture = capture.clone();
            let reply = reply.clone();
            async move {
                if let Ok(mut slot) = capture.lock() { *slot = body; }
                match reply {
                    CountReply::Json(value) => axum::Json(value).into_response(),
                    CountReply::Raw(value) => value.into_response(),
                    CountReply::Redirect => (axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/escaped")]).into_response(),
                    CountReply::Delay => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        axum::Json(serde_json::json!({"count":3,"tokens":[1,2,3],"max_model_len":8192})).into_response()
                    }
                }
            }
        }))
        .route("/escaped", post(move || {
            escaped_counter.fetch_add(1, Ordering::SeqCst);
            async { axum::Json(serde_json::json!({"count":3,"tokens":[1,2,3],"max_model_len":8192})) }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut config = ConnectorConfig::from_yaml(include_str!(
        "../../../../../docs/specs/configuration/provider-connector-v1.example.yaml"
    ))?;
    config.pool.members[0].inference_endpoint = format!("http://{address}/v1");
    let execution = config
        .execution
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("execution fixture"))?;
    execution.max_response_bytes = 1024;
    execution.max_input_tokens = 64;
    let executor = Executor::new(&config).map_err(anyhow::Error::msg)?;
    let local = request(false)?;
    let result = executor
        .count_input(
            &config,
            &config.pool.members[0],
            &local,
            Instant::now() + Duration::from_millis(timeout_ms),
        )
        .await;
    let body = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("fixture capture unavailable"))?
        .clone();
    let escaped_count = escaped.load(Ordering::SeqCst);
    task.abort();
    Ok((result, body, escaped_count))
}

#[tokio::test]
async fn tokenization_is_bound_complete_bounded_and_redacted() -> Result<()> {
    let valid = serde_json::json!({"count":3,"tokens":[1,2,3],"max_model_len":8192});
    let (result, body, escaped) = check_count(CountReply::Json(valid), 1000).await?;
    ensure!(result == Ok(3) && escaped == 0);
    let local = request(false)?;
    ensure!(body == local.tokenize_body());
    for invalid in [
        serde_json::json!({"count":3,"tokens":[1,2],"max_model_len":8192}),
        serde_json::json!({"count":3,"tokens":[1,2,-3],"max_model_len":8192}),
        serde_json::json!({"count":3,"tokens":[1,2,3],"max_model_len":6}),
        serde_json::json!({"count":3,"tokens":[1,2,3],"max_model_len":0}),
        serde_json::json!({"count":0,"tokens":[],"max_model_len":8192}),
        serde_json::json!({"count":65,"tokens":vec![1;65],"max_model_len":8192}),
        serde_json::json!({"count":3,"max_model_len":8192}),
        serde_json::json!({"count":"private-upstream-secret","tokens":[],"max_model_len":8192}),
    ] {
        let (result, _, escaped) = check_count(CountReply::Json(invalid), 1000).await?;
        ensure!(result.is_err() && escaped == 0);
        ensure!(!format!("{result:?}").contains("private-upstream-secret"));
    }
    for reply in [
        CountReply::Raw("private-upstream-secret".to_owned()),
        CountReply::Raw("private-upstream-secret".repeat(200)),
        CountReply::Redirect,
    ] {
        let (result, _, escaped) = check_count(reply, 1000).await?;
        ensure!(result.is_err() && escaped == 0);
        ensure!(!format!("{result:?}").contains("private-upstream-secret"));
    }
    let (result, _, escaped) = check_count(CountReply::Delay, 10).await?;
    ensure!(result == Err("deadline") && escaped == 0);
    Ok(())
}

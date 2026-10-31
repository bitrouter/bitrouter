//! Persistent OpenCode setup uses isolated config instances and scoped receipts.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        for name in [
            "home", "bro", "instance", "project", "config", "data", "cache", "state",
        ] {
            std::fs::create_dir_all(root.join(name))?;
        }
        Ok(Self { _temp: temp, root })
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(self.root.join("project"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("BITROUTER_HOME", self.root.join("bro"))
            .env("BITROUTER_API_KEY", "fixture-key")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("OPENCODE_CONFIG_CONTENT")
            .env_remove("BITROUTER_CONFIG")
            .env_remove("BITROUTER_CONTEXT")
            .kill_on_drop(true);
        command
    }

    async fn bro(&self, args: &[&str], success: bool) -> Result<Value> {
        self.bro_env(args, success, &[]).await
    }

    async fn bro_env(
        &self,
        args: &[&str],
        success: bool,
        environment: &[(&str, &Path)],
    ) -> Result<Value> {
        let mut command = self.command(Path::new(env!("CARGO_BIN_EXE_bro")));
        command.args(args);
        for (name, value) in environment {
            command.env(name, value);
        }
        let output = tokio::time::timeout(Duration::from_secs(30), command.output()).await??;
        assert_eq!(
            output.status.success(),
            success,
            "args={args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if success {
            Ok(serde_json::from_slice(&output.stdout)?)
        } else {
            Ok(Value::Null)
        }
    }

    async fn connect(&self, server: &MockServer, protocol: &str) -> Result<Value> {
        self.bro(
            &[
                "connect",
                "opencode",
                "--instance",
                self.root
                    .join("instance")
                    .to_str()
                    .context("invalid instance")?,
                "--base-url",
                &server.uri(),
                "--model",
                "wire/automatos",
                "--display-name",
                "bitrouter/automatos",
                "--protocol",
                protocol,
                "--yes",
            ],
            true,
        )
        .await
    }

    async fn remove(&self) -> Result<Value> {
        self.bro(
            &[
                "connect",
                "opencode",
                "--instance",
                self.root
                    .join("instance")
                    .to_str()
                    .context("invalid instance")?,
                "--remove",
                "--yes",
            ],
            true,
        )
        .await
    }
}

async fn gateway() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"wire/automatos"}]})),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn isolated_opencode_preserves_jsonc_default_and_later_native_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    let config = fixture.root.join("instance/opencode.jsonc");
    let original = "{\n // user comment\n \"model\": \"native/model\",\n \"provider\": {\"native\": {\"models\": {\"model\": {\"name\":\"Native\"}}}},\n}\n";
    std::fs::write(&config, original)?;
    let server = gateway().await;
    fixture.connect(&server, "chat-completions").await?;
    let connected = std::fs::read_to_string(&config)?;
    assert!(connected.contains("// user comment"));
    assert!(connected.contains("\"id\": \"wire/automatos\""));
    assert!(connected.contains("\"name\": \"bitrouter/automatos\""));
    assert!(connected.contains("@ai-sdk/openai-compatible"));
    assert!(connected.contains("\"model\": \"native/model\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config)?.permissions().mode() & 0o777,
            0o600
        );
    }
    // A native UI can select the newly added model, change another provider,
    // and add a comment. Removal restores just the still-owned default choice.
    let changed = connected
        .replace(
            "\"model\": \"native/model\"",
            "\"model\": \"bitrouter/automatos\"",
        )
        .replace("\"name\":\"Native\"", "\"name\":\"Native changed\"")
        .replace("// user comment", "// later user comment");
    std::fs::write(&config, changed)?;
    fixture
        .bro(
            &[
                "connect",
                "opencode",
                "--instance",
                fixture
                    .root
                    .join("instance")
                    .to_str()
                    .context("invalid instance")?,
                "--yes",
            ],
            true,
        )
        .await?;
    fixture.remove().await?;
    let restored = std::fs::read_to_string(&config)?;
    assert!(restored.contains("// later user comment"));
    assert!(restored.contains("Native changed"));
    assert!(restored.contains("\"model\": \"native/model\""));
    assert!(!restored.contains("fixture-key"));
    assert!(!restored.contains("@ai-sdk/openai-compatible"));
    assert!(!fixture.root.join("project/opencode.json").exists());
    Ok(())
}

#[tokio::test]
async fn opencode_protocols_restore_and_provider_edits_are_rejected() -> Result<()> {
    let fixture = Fixture::new()?;
    let config = fixture.root.join("instance/opencode.json");
    let original = "{\"model\":\"native/model\",\"theme\":\"system\"}\n";
    std::fs::write(&config, original)?;
    let server = gateway().await;
    for (protocol, package) in [
        ("responses", "@ai-sdk/openai"),
        ("messages", "@ai-sdk/anthropic"),
    ] {
        fixture.connect(&server, protocol).await?;
        let connected: Value = serde_json::from_str(&std::fs::read_to_string(&config)?)?;
        assert_eq!(connected["provider"]["bitrouter"]["npm"], package);
        assert_eq!(
            connected["provider"]["bitrouter"]["models"]["automatos"]["id"],
            "wire/automatos"
        );
        fixture.remove().await?;
        assert_eq!(std::fs::read_to_string(&config)?, original);
    }
    fixture.connect(&server, "chat-completions").await?;
    let connected = std::fs::read_to_string(&config)?;
    std::fs::write(&config, connected.replace("wire/automatos", "user/edited"))?;
    fixture
        .bro(
            &[
                "connect",
                "opencode",
                "--instance",
                fixture
                    .root
                    .join("instance")
                    .to_str()
                    .context("invalid instance")?,
                "--remove",
                "--yes",
            ],
            false,
        )
        .await?;
    assert!(std::fs::read_to_string(&config)?.contains("user/edited"));
    std::fs::write(&config, connected)?;
    fixture.remove().await?;
    assert_eq!(std::fs::read_to_string(&config)?, original);
    Ok(())
}

#[tokio::test]
async fn opencode_rejects_existing_provider_and_disabled_provider_without_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    let config = fixture.root.join("instance/opencode.json");
    let server = gateway().await;
    for original in [
        "{\"provider\":{\"bitrouter\":{\"name\":\"User owned\"}}}",
        "{\"disabled_providers\":[\"bitrouter\"]}",
        "{\"enabled_providers\":[\"native\"]}",
    ] {
        std::fs::write(&config, original)?;
        fixture
            .bro(
                &[
                    "connect",
                    "opencode",
                    "--instance",
                    fixture
                        .root
                        .join("instance")
                        .to_str()
                        .context("invalid instance")?,
                    "--base-url",
                    &server.uri(),
                    "--model",
                    "wire/automatos",
                    "--yes",
                ],
                false,
            )
            .await?;
        assert_eq!(std::fs::read_to_string(&config)?, original);
    }
    Ok(())
}

#[tokio::test]
async fn opencode_resolves_real_xdg_custom_file_and_custom_directory_instances() -> Result<()> {
    let fixture = Fixture::new()?;
    let global = fixture.root.join("config/opencode/opencode.jsonc");
    let custom_file = fixture.root.join("custom.jsonc");
    let custom_directory = fixture.root.join("custom-directory");
    let custom_directory_file = custom_directory.join("opencode.jsonc");
    std::fs::create_dir_all(global.parent().context("global has no parent")?)?;
    std::fs::create_dir_all(&custom_directory)?;
    let original = "{ /* native settings */ \"model\": \"native/model\", }\n";
    for path in [&global, &custom_file, &custom_directory_file] {
        std::fs::write(path, original)?;
    }
    let server = gateway().await;
    let cases: Vec<(&Path, Vec<(&str, &Path)>)> = vec![
        (
            &custom_directory_file,
            vec![
                ("OPENCODE_CONFIG", custom_file.as_path()),
                ("OPENCODE_CONFIG_DIR", custom_directory.as_path()),
            ],
        ),
        (
            &custom_file,
            vec![("OPENCODE_CONFIG", custom_file.as_path())],
        ),
        (&global, vec![]),
    ];
    for (expected, environment) in cases {
        let report = fixture
            .bro_env(
                &[
                    "connect",
                    "opencode",
                    "--base-url",
                    &server.uri(),
                    "--model",
                    "wire/automatos",
                    "--yes",
                ],
                true,
                &environment,
            )
            .await?;
        assert_eq!(report["instance"].as_str(), expected.to_str());
        assert!(std::fs::read_to_string(expected)?.contains("fixture-key"));
        for other in [&global, &custom_file, &custom_directory_file] {
            if other.as_path() != expected {
                assert_eq!(std::fs::read_to_string(other)?, original);
            }
        }
        fixture
            .bro_env(
                &["connect", "opencode", "--remove", "--yes"],
                true,
                &environment,
            )
            .await?;
        assert_eq!(std::fs::read_to_string(expected)?, original);
    }
    Ok(())
}

fn sse(events: Vec<(&str, Value)>) -> String {
    events
        .into_iter()
        .map(|(event, value)| format!("event: {event}\ndata: {value}\n\n"))
        .collect()
}

fn chat_stream(tool_result: bool) -> String {
    let delta = if tool_result {
        json!({"content":"BITROUTER_STREAM_TOOL_ACCEPTED"})
    } else {
        json!({"tool_calls":[{"index":0,"id":"acceptance_call","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf BITROUTER_TOOL_OK\",\"description\":\"Acceptance tool\"}"}}]})
    };
    let chunk = json!({"id":"fixture-completion","object":"chat.completion.chunk","created":1,"model":"wire/automatos","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let finish = json!({"id":"fixture-completion","object":"chat.completion.chunk","created":1,"model":"wire/automatos","choices":[{"index":0,"delta":{},"finish_reason":if tool_result {"stop"} else {"tool_calls"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
    format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

fn responses_stream(tool_result: bool) -> String {
    let arguments =
        "{\"command\":\"printf BITROUTER_TOOL_OK\",\"description\":\"Acceptance tool\"}";
    let item = if tool_result {
        json!({"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"BITROUTER_STREAM_TOOL_ACCEPTED","annotations":[]}],"status":"completed"})
    } else {
        json!({"type":"function_call","id":"fc_fixture","call_id":"acceptance_call","name":"bash","arguments":arguments,"status":"completed"})
    };
    let mut response = json!({"id":"resp_fixture","object":"response","created_at":1,"model":"wire/automatos","status":"in_progress","output":[]});
    let mut events = vec![(
        "response.created",
        json!({"type":"response.created","response":response}),
    )];
    events.push(("response.output_item.added",json!({"type":"response.output_item.added","output_index":0,"item":if tool_result {json!({"type":"message","id":"msg_fixture","role":"assistant","content":[],"status":"in_progress"})}else{json!({"type":"function_call","id":"fc_fixture","call_id":"acceptance_call","name":"bash","arguments":"","status":"in_progress"})}})));
    if tool_result {
        events.push(("response.content_part.added",json!({"type":"response.content_part.added","item_id":"msg_fixture","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})));
        events.push(("response.output_text.delta",json!({"type":"response.output_text.delta","item_id":"msg_fixture","output_index":0,"content_index":0,"delta":"BITROUTER_STREAM_TOOL_ACCEPTED"})));
        events.push(("response.output_text.done",json!({"type":"response.output_text.done","item_id":"msg_fixture","output_index":0,"content_index":0,"text":"BITROUTER_STREAM_TOOL_ACCEPTED"})));
    } else {
        events.push(("response.function_call_arguments.delta",json!({"type":"response.function_call_arguments.delta","item_id":"fc_fixture","output_index":0,"delta":arguments})));
        events.push(("response.function_call_arguments.done",json!({"type":"response.function_call_arguments.done","item_id":"fc_fixture","output_index":0,"arguments":arguments})));
    }
    events.push((
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
    ));
    response["status"] = json!("completed");
    response["output"] = json!([item]);
    response["usage"] = json!({"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}});
    events.push((
        "response.completed",
        json!({"type":"response.completed","response":response}),
    ));
    sse(events)
}

fn messages_stream(tool_result: bool) -> String {
    let content = if tool_result {
        json!({"type":"text","text":""})
    } else {
        json!({"type":"tool_use","id":"acceptance_call","name":"bash","input":{}})
    };
    let delta = if tool_result {
        json!({"type":"text_delta","text":"BITROUTER_STREAM_TOOL_ACCEPTED"})
    } else {
        json!({"type":"input_json_delta","partial_json":"{\"command\":\"printf BITROUTER_TOOL_OK\",\"description\":\"Acceptance tool\"}"})
    };
    sse(vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":"wire/automatos","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":content}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":delta}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":if tool_result {"end_turn"}else{"tool_use"},"stop_sequence":null},"usage":{"output_tokens":5}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ])
}

fn tool_result(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(tool_result),
        Value::Object(object) => {
            object.get("role").and_then(Value::as_str) == Some("tool")
                || object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "tool_result" | "function_call_output"))
                || object.values().any(tool_result)
        }
        _ => false,
    }
}

/// Opt-in real OpenCode CLI acceptance. Standard CI has no client installation.
#[tokio::test]
#[ignore = "requires OPENCODE_ACCEPTANCE_BINARY pointing to the installed native CLI"]
async fn native_opencode_streams_and_executes_tool_roundtrip() -> Result<()> {
    let binary = std::env::var_os("OPENCODE_ACCEPTANCE_BINARY")
        .context("OPENCODE_ACCEPTANCE_BINARY is required")?;
    for (protocol, route) in [
        ("chat-completions", "/v1/chat/completions"),
        ("responses", "/v1/responses"),
        ("messages", "/v1/messages"),
    ] {
        let fixture = Fixture::new()?;
        let auth = fixture.root.join("data/opencode/auth.json");
        std::fs::create_dir_all(auth.parent().context("auth has no parent")?)?;
        let original_auth = json!({"native":{"type":"api","key":"native-preserved"},"bitrouter":{"type":"api","key":"unowned-stale-key"}}).to_string();
        std::fs::write(&auth, &original_auth)?;
        let server = gateway().await;
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(|request: &wiremock::Request| {
                let body: Option<Value> = serde_json::from_slice(&request.body).ok();
                let result = body.as_ref().is_some_and(tool_result);
                let stream = match request.url.path() {
                    "/v1/responses" => responses_stream(result),
                    "/v1/messages" => messages_stream(result),
                    _ => chat_stream(result),
                };
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(stream)
            })
            .mount(&server)
            .await;
        fixture.connect(&server, protocol).await?;
        let config = fixture.root.join("instance/opencode.json");
        let output = tokio::time::timeout(
            Duration::from_secs(120),
            fixture
                .command(Path::new(&binary))
                .env("OPENCODE_CONFIG", &config)
                .args([
                    "run",
                    "--format",
                    "json",
                    "--model",
                    "bitrouter/automatos",
                    "Run the acceptance tool and report its result.",
                ])
                .output(),
        )
        .await??;
        assert!(
            output.status.success(),
            "{protocol}: OpenCode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("BITROUTER_TOOL_OK"),
            "{protocol}: missing executed tool result: {stdout}"
        );
        assert!(
            stdout.contains("BITROUTER_STREAM_TOOL_ACCEPTED"),
            "{protocol}: missing streamed final text: {stdout}"
        );
        let requests = server
            .received_requests()
            .await
            .context("missing requests")?;
        let inference: Vec<Value> = requests
            .iter()
            .filter(|request| request.url.path() == route)
            .map(|request| serde_json::from_slice(&request.body))
            .collect::<std::result::Result<_, _>>()?;
        assert!(inference.len() >= 2, "{protocol}");
        assert!(
            inference
                .iter()
                .all(|body| body["model"] == "wire/automatos" && body["stream"] == true),
            "{protocol}"
        );
        assert!(
            inference.iter().any(tool_result),
            "{protocol}: missing tool result on wire"
        );
        for request in requests
            .iter()
            .filter(|request| request.url.path() == route)
        {
            let bearer = request
                .headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer fixture-key");
            let api_key = request
                .headers
                .get("x-api-key")
                .is_some_and(|value| value == "fixture-key");
            assert!(
                bearer || api_key,
                "{protocol}: missing configured credential"
            );
        }
        if let Some(directory) = std::env::var_os("OPENCODE_ACCEPTANCE_EVIDENCE") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory)?;
            std::fs::write(
                directory.join(format!("opencode-{protocol}-native.jsonl")),
                &output.stdout,
            )?;
            std::fs::write(
                directory.join(format!("opencode-{protocol}-wire.json")),
                serde_json::to_vec_pretty(&inference)?,
            )?;
        }
        fixture.remove().await?;
        assert_eq!(std::fs::read_to_string(&auth)?, original_auth);
        if config.exists() {
            // OpenCode may add $schema on first load. That native edit survives.
            let restored: Value = serde_json::from_str(&std::fs::read_to_string(&config)?)?;
            assert!(restored.pointer("/provider/bitrouter").is_none());
            assert!(!std::fs::read_to_string(&config)?.contains("fixture-key"));
        }
    }
    Ok(())
}

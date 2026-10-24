//! Controlled upstream tool calls executed by real workers in an isolated cwd.
//! The judge fixture reads recorded test output; it never executes the checks.

use super::*;

#[path = "coding/publication.rs"]
mod publication;

const PROMPT: &str = "NATIVE_CODE_EXERCISE: implement add(a, b) in native_sum.rs and run two unit tests, covering positive and negative inputs. Do not request a review or create a PR. Finish with a brief result.";

fn command() -> Result<String> {
    // Workers have an isolated HOME. Invoke the toolchain binary rather than
    // asking a rustup proxy to find the developer's toolchain from that home.
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()?;
    ensure!(output.status.success(), "Rust toolchain unavailable");
    let sysroot = String::from_utf8(output.stdout)?;
    let compiler = Path::new(sysroot.trim()).join("bin/rustc");
    Ok(r#"cat > native_sum.rs <<'RS'
fn add(a: i32, b: i32) -> i32 { a + b }
#[test]
fn positive() { assert_eq!(add(2, 3), 5); }
#[test]
fn negative() { assert_eq!(add(-4, 1), -3); }
RS
rustc --test native_sum.rs -o native_sum_tests && ./native_sum_tests"#
        .replace(
            "\nrustc --test",
            &format!("\n{} --test", shell_word(&compiler)),
        ))
}

fn tool_call(body: &Value) -> Result<Value> {
    let command = command()?;
    let command = if body.to_string().contains("NATIVE_FAILING_IMPLEMENTATION") {
        command.replace("a + b", "a - b")
    } else {
        command
    };
    let functions = body["tools"]
        .as_array()
        .context("worker did not advertise tools")?
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    let (name, arguments) = if let Some(name) =
        functions.iter().find(|name| **name == "exec_command")
    {
        (
            *name,
            json!({"cmd":command,"yield_time_ms":10000,"max_output_tokens":2000}),
        )
    } else if let Some(name) = functions.iter().find(|name| **name == "Bash") {
        (
            *name,
            json!({"command":command,"description":"Write the isolated sum implementation and execute its unit tests","timeout":10000}),
        )
    } else if let Some(name) = functions.iter().find(|name| **name == "shell_command") {
        (*name, json!({"command":command,"timeout_ms":10000}))
    } else {
        bail!("worker has no recognized shell tool: {functions:?}")
    };
    Ok(
        json!({"id":"native-code-check","type":"function","function":{"name":name,"arguments":serde_json::to_string(&arguments)?}}),
    )
}

fn judge(body: &Value) -> Result<String> {
    let input: Value = serde_json::from_str(
        body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_str())
            .context("judge evidence missing")?,
    )?;
    let packet: EvidencePacket = serde_json::from_value(input["evidence"].clone())?;
    let tests = packet
        .items
        .iter()
        .filter(|item| {
            item.kind == EvidenceKind::ToolObservation
                && item.content.to_string().contains("running 2 tests")
        })
        .map(|item| item.citation.clone())
        .collect::<Vec<_>>();
    ensure!(
        !tests.is_empty(),
        "recorded executed test results missing from judge packet: {:?}",
        packet
            .items
            .iter()
            .filter(|item| item.kind == EvidenceKind::ToolObservation)
            .map(|item| &item.content)
            .collect::<Vec<_>>()
    );
    let failed = packet.items.iter().any(|item| {
        item.kind == EvidenceKind::ToolObservation
            && item
                .content
                .to_string()
                .contains("test result: FAILED. 0 passed; 2 failed;")
    });
    ensure!(
        failed
            || packet
                .items
                .iter()
                .any(|item| item.kind == EvidenceKind::ToolObservation
                    && item
                        .content
                        .to_string()
                        .contains("test result: ok. 2 passed; 0 failed;")),
        "unit-test outcome missing"
    );
    let obligations = packet
        .items
        .iter()
        .filter(|item| item.kind == EvidenceKind::UserMessage)
        .map(|item| item.citation.clone())
        .collect::<Vec<_>>();
    ensure!(!obligations.is_empty(), "recorded coding request missing");
    serde_json::to_string(&RubricEvaluation {
        rubric_version: rubric::RUBRIC_VERSION.into(),
        items: rubric::library().into_iter().map(|template| {
            let applicable = template.mandatory || template.id == "verification";
            RubricItem {
                criterion_id: template.id.into(),
                applicability: if applicable { Applicability::Applicable } else { Applicability::NotApplicable },
                selection_reason: "Controlled coding request requires implementation and tests; no review, PR or user outcome feedback.".into(),
                score: if applicable { CriterionScore::Scored { value_ppm: if failed { 100_000 } else { 950_000 } } } else { CriterionScore::NotApplicable },
                evidence: if applicable { tests.clone() } else { obligations.clone() },
                explanation: "Fixture label grounded in recorded tool output; no calibration claim.".into(),
            }
        }).collect(),
        diagnostics: vec![], severe_violation: false, violation_evidence: vec![],
        summary: "Recorded unit tests executed against the controlled coding artifact.".into(),
    }).map_err(Into::into)
}

#[test]
fn rust_coding_judge_uses_executed_pass_and_fail_results() -> Result<()> {
    for failed in [false, true] {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("home");
        std::fs::create_dir(&home)?;
        let command = command()?;
        let command = if failed {
            command.replace("a + b", "a - b")
        } else {
            command
        };
        let output = Command::new("sh")
            .args(["-c", &command])
            .env_clear()
            .env("PATH", inherited_path()?)
            .env("HOME", home)
            .current_dir(directory.path())
            .output()?;
        ensure!(
            output.status.success() != failed,
            "unexpected Rust test exit status"
        );
        let evidence = json!({
            "checkpoint_id": "rust-coding-fixture", "prefix_digest": "fixture", "gaps": [],
            "items": [
                {"citation": {"node_id": "fixture:1", "digest": "user"}, "kind": "user_message", "method": "session/prompt", "call_id": null, "content": PROMPT},
                {"citation": {"node_id": "fixture:2", "digest": "tests"}, "kind": "tool_observation", "method": "terminal/output", "call_id": 1,
                 "content": {"stdout": String::from_utf8(output.stdout)?, "stderr": String::from_utf8(output.stderr)?, "exitCode": output.status.code()}}
            ]
        });
        let body = json!({"messages": [{"content": json!({"evidence": evidence}).to_string()}]});
        let evaluation: RubricEvaluation = serde_json::from_str(&judge(&body)?)?;
        let verification = evaluation
            .items
            .iter()
            .find(|item| item.criterion_id == "verification")
            .context("verification criterion missing")?;
        ensure!(
            matches!(verification.score, CriterionScore::Scored { value_ppm } if value_ppm == if failed { 100_000 } else { 950_000 }),
            "judge did not use the executed test outcome"
        );
    }
    Ok(())
}

fn response(request: &Request) -> Result<ResponseTemplate> {
    let body: Value = serde_json::from_slice(&request.body)?;
    let model = body["model"].as_str().context("model missing")?;
    let (message, finish) = if model == "judge" {
        (json!({"role":"assistant","content":judge(&body)?}), "stop")
    } else {
        let tool_result = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
        // The worker also sends auxiliary requests containing the conversation.
        // Only its tool-capable coding request can execute this fixture command.
        let has_shell_tool = body["tools"].as_array().is_some_and(|tools| {
            tools.iter().any(|tool| {
                matches!(
                    tool["function"]["name"].as_str(),
                    Some("exec_command" | "Bash" | "shell_command")
                )
            })
        });
        if body.to_string().contains("NATIVE_CODE_EXERCISE") && !tool_result && has_shell_tool {
            (
                json!({"role":"assistant","tool_calls":[tool_call(&body)?]}),
                "tool_calls",
            )
        } else {
            let content = if body
                .to_string()
                .contains("Controlled follow-up after the trial.")
            {
                "NATIVE_BASELINE_OK"
            } else {
                "NATIVE_CODE_DONE"
            };
            (json!({"role":"assistant","content":content}), "stop")
        }
    };
    let usage = json!({"prompt_tokens":20,"completion_tokens":10,"total_tokens":30});
    let reply = if body["stream"].as_bool() == Some(true) {
        let mut delta = message;
        if let Some(calls) = delta["tool_calls"].as_array_mut() {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
        }
        let chunks = [
            json!({"id":"native-coding","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"id":"native-coding","object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":usage}),
        ];
        let mut stream = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        stream.push_str("data: [DONE]\n\n");
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(stream)
    } else {
        ResponseTemplate::new(200).set_body_json(json!({"id":"native-coding","object":"chat.completion","created":0,"model":model,"choices":[{"index":0,"message":message,"finish_reason":finish}],"usage":usage}))
    };
    Ok(reply.set_delay(Duration::from_millis(if model == "strong" {
        250
    } else {
        5
    })))
}

fn launch(
    agent: &str,
    adapter: &str,
    worker: &str,
) -> Result<(tokio::runtime::Runtime, NativeService, Cli)> {
    let mock = Environment::new()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    let service = runtime.block_on(setup_with_reply(&mock, agent, adapter, worker, response))?;
    let code = Cli::launch(mock, agent, &runtime)?;
    code.evolution(&["judge-model", "judge"])?;
    code.evolution(&["mode", "automatic"])?;
    Ok((runtime, service, code))
}

fn run(agent: &str, adapter: &str, worker: &str) -> Result<()> {
    let (runtime, service, code) = launch(agent, adapter, worker)?;
    code.submit(PROMPT)?;
    let identity = match runtime.block_on(evaluated(&service, agent)) {
        Ok(identity) => identity,
        Err(error) => {
            for session in runtime.block_on(service.canonical.list("local", agent))? {
                let identity = SessionIdentity {
                    owner: "local".into(),
                    source: agent.into(),
                    native_session_id: session.native_session_id,
                };
                let effective = runtime.block_on(read_effective(&service, &identity))?;
                eprintln!(
                    "Coding checkpoint state: {}",
                    json!({"stale":effective.stale,"reasons":effective.reasons,"revision":effective.current_revision,"resource":effective.resource})
                );
            }
            if let bitrouter::evolution::operator::EvolutionReport::Status(status) = runtime
                .block_on(service.evolution.operate(
                    "local",
                    bitrouter::evolution::operator::EvolutionOperation::Status,
                ))?
            {
                eprintln!(
                    "Coding evaluation jobs: {:?}; worker: {:?}",
                    status.jobs, status.worker
                );
            }
            for request in runtime
                .block_on(service.upstream.received_requests())
                .unwrap_or_default()
            {
                let body: Value = serde_json::from_slice(&request.body)?;
                if body["model"] == "judge" {
                    eprintln!(
                        "Recorded coding fixture judge diagnostic: {:?}",
                        judge(&body).map(|_| "valid fixture response")
                    );
                    break;
                }
            }
            return Err(error);
        }
    };
    let effective = runtime.block_on(read_effective(&service, &identity))?;
    let checkpoint = effective.checkpoint.context("coding checkpoint missing")?;
    let packet = EvidencePacket::from_checkpoint(
        &runtime.block_on(
            service
                .canonical
                .checkpoint_content(&identity, &checkpoint.checkpoint_id),
        )?,
    )?;
    ensure!(
        packet
            .items
            .iter()
            .any(|item| item.kind == EvidenceKind::ToolObservation
                && item.content.to_string().contains("running 2 tests")),
        "canonical checkpoint lost executed tests"
    );
    ensure!(
        std::fs::read_to_string(code.mock._directory.path().join("native_sum.rs"))?
            .contains("a + b"),
        "worker did not write the coding artifact"
    );
    let executions = runtime.block_on(service.evolution.executions(&identity))?;
    ensure!(
        executions.len() >= 2,
        "missing model continuation after the tool result"
    );
    code.stop()?;
    println!(
        "{}",
        json!({"agent":agent,"native_session":identity.native_session_id,"recorded_requests":executions.len(),"tool_tests_recorded":true,"worker_wrote_code":true,"cli_control":true,"scope":"real worker tool execution, captured tests and fixture rubric evaluation"})
    );
    Ok(())
}

#[test]
#[ignore = "requires the maintained Codex ACP adapter and isolated worker executable"]
fn maintained_codex_cli_coding_checkpoint() -> Result<()> {
    run(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
}

#[test]
#[ignore = "requires the maintained Claude ACP adapter and isolated worker executable"]
fn maintained_claude_cli_coding_checkpoint() -> Result<()> {
    run(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
}

//! Controlled upstream tool calls executed by real workers in an isolated cwd.
//! The judge fixture reads recorded test output; it never executes the checks.

use super::*;

#[path = "coding/publication.rs"]
mod publication;

const PROMPT: &str = "NATIVE_CODE_EXERCISE: implement add(a, b) in native_sum.py and run two unit tests, covering positive and negative inputs. Do not request a review or create a PR. Finish with a brief result.";

fn command() -> &'static str {
    r#"python3 - <<'PY'
from pathlib import Path
import unittest
Path('native_sum.py').write_text('def add(a, b):\n    return a + b\n')
from native_sum import add
class SumTests(unittest.TestCase):
    def test_positive(self): self.assertEqual(add(2, 3), 5)
    def test_negative(self): self.assertEqual(add(-4, 1), -3)
unittest.main()
PY"#
}

fn tool_call(body: &Value) -> Result<Value> {
    let command = if body.to_string().contains("NATIVE_FAILING_IMPLEMENTATION") {
        command().replace("return a + b", "return a - b")
    } else {
        command().into()
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
                && item.content.to_string().contains("Ran 2 tests")
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
            && item.content.to_string().contains("FAILED (failures=2)")
    });
    ensure!(
        failed
            || packet
                .items
                .iter()
                .any(|item| item.kind == EvidenceKind::ToolObservation
                    && item.content.to_string().contains("OK")),
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

fn finish_coding_turn(code: &mut CodeFixture, before: &PtyCheckpoint) -> Result<()> {
    for _ in 0..4 {
        let screen = code.pty.wait_for_screen_inner(
            Some(before),
            "coding completion or permission",
            |screen| {
                (screen.contains("Turn completed") && screen.contains("NATIVE_CODE_DONE"))
                    || screen.contains("Permission needed")
            },
        )?;
        if screen.contains("Turn completed") && screen.contains("NATIVE_CODE_DONE") {
            return Ok(());
        }
        code.pty.send(b"\x1b[12~")?;
        let options = code.pty.wait_for_text("Enter confirms selection")?;
        ensure!(
            (options.contains("[1] Yes") || options.contains("[1] Allow"))
                && (options.contains("python3") || options.contains("native_sum.py")),
            "unexpected permission request: {options}"
        );
        let answered = code.pty.checkpoint();
        code.pty.send(b"1\r")?;
        code.pty.wait_for_screen_inner(
            Some(&answered),
            "accepted coding command resumed",
            |screen| !screen.contains("Permission needed") && !screen.contains("┌ Permission"),
        )?;
    }
    bail!("coding worker repeatedly requested permission")
}

fn launch(
    agent: &str,
    adapter: &str,
    worker: &str,
) -> Result<(tokio::runtime::Runtime, NativeService, CodeFixture)> {
    let mock = MockAcp::new(MockScenario::Minimal)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()?;
    let service = runtime.block_on(setup_with_reply(&mock, agent, adapter, worker, response))?;
    let child_pid_path = mock._directory.path().join("code-child.pid");
    let mut command = shell_command(&mock, None)?;
    command.args([
        "code",
        agent,
        "--model",
        selector(agent),
        "--no-start",
        "--turn-timeout",
        "30",
        "--config",
    ]);
    command.arg(&mock.config_path);
    let mut code = CodeFixture {
        mock,
        child_pid_path,
        pty: PtyRunner::spawn(command, 120, 40)?,
    };
    code.pty.wait_for_text("activity: ready")?;
    open_evolution(&mut code)?;
    choose(&mut code, "Judge model", " Judge model ─")?;
    choose(&mut code, "judge", "Checkpoint evaluation and evolution")?;
    choose(&mut code, "Evolution mode", " Evolution mode ─")?;
    choose_at(
        &mut code,
        "Automatic",
        1,
        "Checkpoint evaluation and evolution",
    )?;
    code.close_to_composer()?;
    Ok((runtime, service, code))
}

fn run(agent: &str, adapter: &str, worker: &str) -> Result<()> {
    let (runtime, service, mut code) = launch(agent, adapter, worker)?;
    let before = code.pty.checkpoint();
    code.pty.paste(PROMPT)?;
    code.pty.send(b"\r")?;
    finish_coding_turn(&mut code, &before)?;
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
                && item.content.to_string().contains("Ran 2 tests")),
        "canonical checkpoint lost executed tests"
    );
    ensure!(
        std::fs::read_to_string(code.mock._directory.path().join("native_sum.py"))?
            .contains("return a + b"),
        "worker did not write the coding artifact"
    );
    let executions = runtime.block_on(service.evolution.executions(&identity))?;
    ensure!(
        executions.len() >= 2,
        "missing model continuation after the tool result"
    );
    code.pty.send(b"\x04")?;
    code.assert_terminal_restored()?;
    println!(
        "{}",
        json!({"agent":agent,"native_session":identity.native_session_id,"recorded_requests":executions.len(),"tool_tests_recorded":true,"worker_wrote_code":true,"terminal_restored":true,"scope":"real worker tool execution, captured tests and fixture rubric evaluation"})
    );
    Ok(())
}

#[test]
#[ignore = "requires the maintained Codex ACP adapter and isolated worker executable"]
fn maintained_codex_tui_coding_checkpoint() -> Result<()> {
    run(
        "codex-acp",
        "BITROUTER_TEST_CODEX_ADAPTER",
        "BITROUTER_TEST_CODEX_WORKER",
    )
}

#[test]
#[ignore = "requires the maintained Claude ACP adapter and isolated worker executable"]
fn maintained_claude_tui_coding_checkpoint() -> Result<()> {
    run(
        "claude-acp",
        "BITROUTER_TEST_CLAUDE_ADAPTER",
        "BITROUTER_TEST_CLAUDE_WORKER",
    )
}

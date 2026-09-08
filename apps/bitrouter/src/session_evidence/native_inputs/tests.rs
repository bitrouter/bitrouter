use super::*;
use crate::eval::types::canonical_digest;
use crate::session_evidence::types::RecordInput;
use serde_json::json;

mod executions;
mod rollouts;

const PROCESS: &str = "12345678-1234-4234-8234-123456789abc";

fn source(harness: Harness) -> SourceDescriptor {
    SourceDescriptor {
        namespace: "profile".into(),
        harness,
        node: None,
        format: if harness == Harness::Codex {
            SourceFormat::CodexAppServer
        } else {
            SourceFormat::ClaudeCli
        },
        locator: format!(
            "spool:/owned/{}{PROCESS}.jsonl",
            if harness == Harness::Codex {
                ""
            } else {
                "cli-"
            }
        ),
    }
}

fn record(source: &SourceDescriptor, sequence: u64, mut raw: Value) -> Result<StoredRecord> {
    raw["sequence"] = json!(sequence);
    if source.harness == Harness::ClaudeCode {
        raw["process_id"] = json!(PROCESS);
        raw["namespace"] = json!(source.namespace);
        raw["scope_valid"] = json!(true);
    }
    let input = RecordInput {
        generation: "spool/1".into(),
        sequence,
        byte_start: None,
        byte_end: None,
        producer_version: None,
        raw,
    };
    let source_id = source.id("owner")?;
    Ok(StoredRecord {
        id: input.id(&source_id)?,
        source_id,
        digest: canonical_digest(&input)?,
        input,
    })
}

fn start(id: &str, thread: &str) -> Value {
    json!({"method":"turn/start","direction":"client","phase":"request","operation_id":id,"payload":{"threadId":thread,"input":[]}})
}

fn accepted(id: &str, turn: &str) -> Value {
    json!({"method":"turn/start","direction":"server","phase":"response","operation_id":id,"payload":{"turn":{"id":turn,"status":"inProgress"}}})
}

fn input() -> Value {
    json!({"method":"runtime/input","direction":"client","phase":"request","payload":{"type":"user","uuid":"command","session_id":"acp-attachment"}})
}

fn command(session: &str, state: &str) -> Value {
    json!({"method":"runtime/message","direction":"server","phase":"notification","payload":{"type":"command_lifecycle","command_uuid":"command","state":state,"session_id":session}})
}

fn scan(source: &SourceDescriptor, rows: Vec<Value>) -> Result<(Vec<Receipt>, BTreeSet<String>)> {
    let targets = BTreeSet::from(["turn".into(), "command".into()]);
    let mut scanner = Scanner::new(source, &targets);
    for (sequence, raw) in rows.into_iter().enumerate() {
        scanner.push(&record(source, sequence as u64, raw)?)?;
    }
    Ok(scanner.finish())
}

#[test]
fn codex_pairs_connection_rpc_ids_and_ignores_server_request_id_collisions() -> Result<()> {
    let source = source(Harness::Codex);
    let (receipts, gaps) = scan(
        &source,
        vec![
            start("7", "thread"),
            json!({"method":"item/tool/requestUserInput","direction":"server","phase":"request","operation_id":"7","payload":{}}),
            json!({"method":"item/tool/requestUserInput","direction":"client","phase":"response","operation_id":"7","payload":{}}),
            accepted("7", "turn"),
            start("7", "auxiliary-title"),
            accepted("7", "other-turn"),
        ],
    )?;
    assert!(gaps.is_empty());
    assert_eq!(receipts.len(), 1);
    let receipt = receipts.first().context("receipt")?;
    assert_eq!(receipt.node.native_id, "thread");
    assert_eq!(receipt.input.range.start, 0);
    assert_eq!(receipt.acknowledgements[0].record.range.start, 3);
    assert!(!matches(
        &Event::CodexAccepted {
            thread_id: "wrong-thread".into(),
            turn_id: "turn".into(),
            role: "prompt".into()
        },
        receipt
    ));
    Ok(())
}

#[test]
fn conflicting_rpc_ids_stay_poisoned_after_late_responses_and_reuse() -> Result<()> {
    let (receipts, gaps) = scan(
        &source(Harness::Codex),
        vec![
            start("7", "a"),
            start("7", "b"),
            accepted("7", "turn"),
            start("7", "c"),
            accepted("7", "turn"),
            accepted("7", "turn"),
        ],
    )?;
    assert!(receipts.is_empty());
    assert!(gaps.contains("native_input_rpc_ambiguous"));
    assert!(scan(&source(Harness::Codex), vec![accepted("missing", "turn")]).is_err());
    Ok(())
}

#[test]
fn claude_uses_native_acknowledgement_identity_across_reset() -> Result<()> {
    let (receipts, gaps) = scan(
        &source(Harness::ClaudeCode),
        vec![
            input(),
            command("native-before", "queued"),
            command("native-after", "started"),
            command("native-after", "completed"),
        ],
    )?;
    assert!(gaps.is_empty());
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(
        |receipt| receipt.node.native_id != "acp-attachment" && receipt.input.range.start == 0
    ));
    let continued = receipts
        .iter()
        .find(|receipt| receipt.node.native_id == "native-after")
        .context("continued command")?;
    assert_eq!(
        continued
            .acknowledgements
            .iter()
            .map(|ack| ack.state.as_str())
            .collect::<Vec<_>>(),
        ["started", "completed"]
    );
    Ok(())
}

#[test]
fn repeated_command_input_is_ambiguous_even_after_completion() -> Result<()> {
    let (receipts, gaps) = scan(
        &source(Harness::ClaudeCode),
        vec![
            input(),
            command("native", "completed"),
            input(),
            command("native", "started"),
        ],
    )?;
    assert!(receipts.is_empty());
    assert!(gaps.contains("native_input_command_ambiguous"));
    assert!(
        scan(
            &source(Harness::ClaudeCode),
            vec![command("native", "queued"), input()]
        )
        .is_err()
    );
    let (unacknowledged, _) = scan(&source(Harness::ClaudeCode), vec![input()])?;
    assert!(unacknowledged.is_empty());
    Ok(())
}

#[test]
fn corruption_gaps_and_unknown_native_states_cannot_certify_inputs() -> Result<()> {
    let source = source(Harness::Codex);
    let targets = BTreeSet::from(["turn".into()]);
    let mut scanner = Scanner::new(&source, &targets);
    assert!(
        scanner
            .push(&record(&source, 1, start("7", "thread"))?)
            .is_err()
    );
    let (receipts, gaps) = scan(
        &source,
        vec![
            start("7", "thread"),
            accepted("7", "turn"),
            json!({"method":"runtime/gap","reason":"missing-frame"}),
        ],
    )?;
    assert!(receipts.is_empty());
    assert!(gaps.contains("native_input_capture_gap"));
    let source = super::tests::source(Harness::ClaudeCode);
    assert!(scan(&source, vec![input(), command("native", "unknown")]).is_err());
    let mut invalid = command("native", "started");
    invalid["payload"]["bitrouter_capture_invalid"] = json!(true);
    assert!(scan(&source, vec![input(), invalid]).is_err());
    Ok(())
}

#[test]
fn acknowledgement_limits_apply_across_all_native_conversations() -> Result<()> {
    let source = source(Harness::ClaudeCode);
    let targets = BTreeSet::from(["command".into()]);
    let mut scanner = Scanner::new(&source, &targets);
    scanner.push(&record(&source, 0, input())?)?;
    for n in 0..MAX_GRAPH_ITEMS {
        scanner.push(&record(
            &source,
            n as u64 + 1,
            command(&format!("native-{n}"), "queued"),
        )?)?;
    }
    assert!(
        scanner
            .push(&record(
                &source,
                MAX_GRAPH_ITEMS as u64 + 1,
                command("overflow", "queued")
            )?)
            .is_err()
    );
    Ok(())
}

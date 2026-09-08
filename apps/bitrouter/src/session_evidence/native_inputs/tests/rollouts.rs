use super::*;

fn path(thread: &str, rollout: &str) -> String {
    std::env::temp_dir()
        .join(if thread == rollout {
            format!("rollout-{thread}.jsonl")
        } else {
            format!("rollout-{thread}_{rollout}.jsonl")
        })
        .to_string_lossy()
        .into_owned()
}

fn request(method: &str, id: &str, thread: &str) -> Value {
    json!({"method":method,"direction":"client","phase":"request","operation_id":id,"payload":{"threadId":thread}})
}

fn response(method: &str, id: &str, thread: &str, rollout: &str) -> Value {
    json!({"method":method,"direction":"server","phase":"response","operation_id":id,"payload":{"thread":{"id":thread,"path":path(thread,rollout),"ephemeral":false}}})
}

fn notification(method: &str, thread: &str) -> Value {
    json!({"method":method,"direction":"server","phase":"notification","payload":{"threadId":thread}})
}

fn scan_rows(rows: Vec<Value>) -> Result<Vec<Receipt>> {
    let source = source(Harness::Codex);
    let targets = BTreeSet::from(["before".into(), "after".into(), "turn".into()]);
    let mut scanner = Scanner::new(&source, &targets);
    for (n, raw) in rows.into_iter().enumerate() {
        scanner.push(&record(&source, n as u64, raw)?)?;
    }
    let scanned = scanner.finish();
    let receipts = scanned.receipts;
    let gaps = scanned.gaps;
    assert!(gaps.is_empty(), "{gaps:?}");
    Ok(receipts)
}

fn history(
    receipt: &Receipt,
) -> Result<&crate::session_evidence::native_inputs::rollouts::CodexHistoryEvidence> {
    receipt.codex_history.as_ref().context("Codex history")
}

#[test]
fn native_revert_selects_the_new_rollout_without_rewriting_an_earlier_input() -> Result<()> {
    let receipts = scan_rows(vec![
        request("thread/start", "create", "root"),
        response("thread/start", "create", "root", "root"),
        start("a", "root"),
        accepted("a", "before"),
        request("thread/revert", "revert", "root"),
        response("thread/revert", "revert", "root", "replacement"),
        notification("thread/reverted", "root"),
        start("b", "root"),
        accepted("b", "after"),
        notification("thread/closed", "root"),
    ])?;
    assert_eq!(receipts.len(), 2);
    for (receipt, rollout, request_sequence, response_sequence) in [
        (&receipts[0], "root", 0, 1),
        (&receipts[1], "replacement", 4, 5),
    ] {
        let history = history(receipt)?;
        assert!(history.gaps.is_empty());
        let selected = history.lifecycle.as_ref().context("selection")?;
        assert_eq!(selected.rollout_id, rollout);
        assert_eq!(
            selected.request.as_ref().context("request")?.range.start,
            request_sequence
        );
        assert_eq!(selected.response.range.start, response_sequence);
        assert!(history.source.is_none());
    }
    Ok(())
}

#[test]
fn thread_read_never_selects_a_path_and_a_failed_revert_needs_new_lifecycle_proof() -> Result<()> {
    let receipts = scan_rows(vec![
        request("thread/start", "create", "root"),
        response("thread/start", "create", "root", "root"),
        request("thread/read", "read", "root"),
        response("thread/read", "read", "root", "unrelated"),
        start("a", "root"),
        accepted("a", "before"),
        request("thread/revert", "revert", "root"),
        json!({"method":"thread/revert","direction":"server","phase":"response","operation_id":"revert","payload":{"error_code":-32000}}),
        start("b", "root"),
        accepted("b", "turn"),
        request("thread/resume", "resume", "root"),
        response("thread/resume", "resume", "root", "replacement"),
        start("c", "root"),
        accepted("c", "after"),
    ])?;
    assert_eq!(
        history(&receipts[0])?
            .lifecycle
            .as_ref()
            .context("first")?
            .rollout_id,
        "root"
    );
    assert!(history(&receipts[1])?.lifecycle.is_none());
    assert!(
        history(&receipts[1])?
            .gaps
            .contains("native_input_rollout_unselected")
    );
    assert_eq!(
        history(&receipts[2])?
            .lifecycle
            .as_ref()
            .context("resumed")?
            .rollout_id,
        "replacement"
    );
    Ok(())
}

#[test]
fn overlapping_lifecycle_and_turn_requests_do_not_infer_processing_order() -> Result<()> {
    for responses_reversed in [false, true] {
        let mut rows = vec![
            request("thread/resume", "a", "root"),
            request("thread/revert", "b", "root"),
        ];
        let mut responses = vec![
            response("thread/resume", "a", "root", "root"),
            response("thread/revert", "b", "root", "replacement"),
        ];
        if responses_reversed {
            responses.reverse();
        }
        rows.extend(responses);
        rows.extend([start("c", "root"), accepted("c", "turn")]);
        let receipts = scan_rows(rows)?;
        assert!(history(&receipts[0])?.lifecycle.is_none());
    }
    let receipts = scan_rows(vec![
        request("thread/start", "create", "root"),
        response("thread/start", "create", "root", "root"),
        start("a", "root"),
        request("thread/revert", "revert", "root"),
        accepted("a", "turn"),
        response("thread/revert", "revert", "root", "replacement"),
    ])?;
    assert!(history(&receipts[0])?.lifecycle.is_none());
    assert!(
        history(&receipts[0])?
            .gaps
            .contains("native_input_rollout_transition_overlap")
    );
    Ok(())
}

#[test]
fn start_notifications_on_either_side_of_a_response_keep_its_original_provenance() -> Result<()> {
    for before in [true, false] {
        let mut started = response("thread/started", "unused", "root", "root");
        started["phase"] = json!("notification");
        let response = response("thread/start", "create", "root", "root");
        let pair = if before {
            [started, response]
        } else {
            [response, started]
        };
        let mut rows = vec![request("thread/start", "create", "root")];
        rows.extend(pair);
        rows.extend([start("a", "root"), accepted("a", "turn")]);
        let receipts = scan_rows(rows)?;
        let selected = history(&receipts[0])?
            .lifecycle
            .as_ref()
            .context("selected")?;
        assert_eq!(selected.method, "thread/start");
        assert_eq!(selected.request.as_ref().context("request")?.range.start, 0);
    }
    Ok(())
}

#[test]
fn uncorrelated_reverts_close_and_new_connections_cannot_reuse_an_old_rollout() -> Result<()> {
    for method in [
        "thread/reverted",
        "thread/closed",
        "thread/archived",
        "thread/deleted",
    ] {
        let receipts = scan_rows(vec![
            request("thread/start", "create", "root"),
            response("thread/start", "create", "root", "root"),
            notification(method, "root"),
            start("a", "root"),
            accepted("a", "turn"),
        ])?;
        assert!(history(&receipts[0])?.lifecycle.is_none());
    }
    let receipts = scan_rows(vec![start("a", "root"), accepted("a", "turn")])?;
    assert!(history(&receipts[0])?.lifecycle.is_none());
    Ok(())
}

#[test]
fn delayed_creation_and_revert_notifications_preserve_newer_completed_lifecycles() -> Result<()> {
    for last in ["thread/resume", "thread/revert"] {
        let mut started = response("thread/started", "unused", "root", "root");
        started["phase"] = json!("notification");
        let mut rows = vec![
            request("thread/start", "create", "root"),
            response("thread/start", "create", "root", "root"),
            request("thread/revert", "first", "root"),
            response("thread/revert", "first", "root", "first"),
            request(last, "latest", "root"),
            response(last, "latest", "root", "latest"),
            started,
            notification("thread/reverted", "root"),
        ];
        if last == "thread/revert" {
            rows.push(notification("thread/reverted", "root"));
        }
        rows.extend([start("a", "root"), accepted("a", "turn")]);
        let receipts = scan_rows(rows)?;
        let selected = history(&receipts[0])?
            .lifecycle
            .as_ref()
            .context("latest lifecycle")?;
        assert_eq!(selected.rollout_id, "latest");
        assert_eq!(selected.method, last);
    }
    Ok(())
}

#[test]
fn lifecycle_notification_backlog_is_bounded_across_threads_and_reverts() -> Result<()> {
    let source = source(Harness::Codex);
    let targets = BTreeSet::new();
    let mut scanner = Scanner::new(&source, &targets);
    for n in 0..MAX_GRAPH_ITEMS {
        scanner.push(&record(
            &source,
            n as u64 * 2,
            request("thread/revert", &n.to_string(), "root"),
        )?)?;
        scanner.push(&record(
            &source,
            n as u64 * 2 + 1,
            response("thread/revert", &n.to_string(), "root", "replacement"),
        )?)?;
    }
    scanner.push(&record(
        &source,
        MAX_GRAPH_ITEMS as u64 * 2,
        request("thread/revert", "overflow", "root"),
    )?)?;
    assert!(
        scanner
            .push(&record(
                &source,
                MAX_GRAPH_ITEMS as u64 * 2 + 1,
                response("thread/revert", "overflow", "root", "replacement")
            )?)
            .is_err()
    );
    Ok(())
}

#[test]
fn fork_selects_the_child_without_switching_the_parent_and_ephemeral_stays_unknown() -> Result<()> {
    let mut ephemeral = response("thread/start", "ephemeral", "aux", "aux");
    ephemeral["payload"]["thread"]["ephemeral"] = json!(true);
    let receipts = scan_rows(vec![
        request("thread/start", "create", "root"),
        response("thread/start", "create", "root", "root"),
        request("thread/fork", "fork", "root"),
        response("thread/fork", "fork", "child", "child"),
        start("a", "root"),
        accepted("a", "before"),
        start("b", "child"),
        accepted("b", "after"),
        request("thread/start", "ephemeral", "aux"),
        ephemeral,
        start("c", "aux"),
        accepted("c", "turn"),
    ])?;
    assert_eq!(
        history(&receipts[0])?
            .lifecycle
            .as_ref()
            .context("root")?
            .rollout_id,
        "root"
    );
    assert_eq!(
        history(&receipts[1])?
            .lifecycle
            .as_ref()
            .context("child")?
            .rollout_id,
        "child"
    );
    assert!(history(&receipts[2])?.lifecycle.is_none());
    Ok(())
}

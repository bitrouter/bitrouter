use super::*;
use crate::session_evidence::execution::input_runs::InputOutcome;

fn turn(thread: &str, id: &str, status: &str) -> Value {
    json!({"method":if status == "inProgress" {"turn/started"} else {"turn/completed"},
        "direction":"server","phase":"notification",
        "payload":{"threadId":thread,"turn":{"id":id,"status":status}}})
}

fn result(session: &str, command_id: Option<&str>, status: &str, is_error: bool) -> Value {
    json!({"method":"runtime/message","direction":"server","phase":"notification",
        "payload":{"type":"result","subtype":status,"is_error":is_error,"uuid":"result",
            "session_id":session,"user_message_uuid":command_id}})
}

#[test]
fn codex_selects_addressed_events_across_interleaving_and_late_acceptance() -> Result<()> {
    let (receipts, gaps) = scan(
        &source(Harness::Codex),
        vec![
            start("7", "thread"),
            turn("thread", "turn", "inProgress"),
            start("8", "title-thread"),
            turn("title-thread", "title-turn", "inProgress"),
            json!({"method":"item/completed","direction":"server","phase":"notification",
            "payload":{"threadId":"thread","turnId":"turn","item":{"type":"agentMessage","id":"message","text":"Done"}}}),
            turn("other-thread", "turn", "completed"),
            turn("thread", "turn", "completed"),
            accepted("7", "turn"),
            accepted("8", "title-turn"),
        ],
    )?;
    assert!(gaps.is_empty());
    assert_eq!(receipts.len(), 1);
    let execution = &receipts[0].execution;
    assert!(execution.gaps.is_empty(), "{:?}", execution.gaps);
    assert_eq!(execution.outcome, Some(InputOutcome::Completed));
    assert_eq!(
        execution
            .records
            .iter()
            .map(|row| row.range.start)
            .collect::<Vec<_>>(),
        [1, 4, 6]
    );
    assert_eq!(receipts[0].acknowledgements[0].record.range.start, 7);
    Ok(())
}

#[test]
fn codex_records_a_call_site_without_closing_parent_or_claiming_child_turn() -> Result<()> {
    let call = json!({"method":"item/completed","direction":"server","phase":"notification",
        "payload":{"threadId":"thread","turnId":"turn","item":{"type":"collabAgentToolCall",
            "id":"call","tool":"resumeAgent","status":"completed","senderThreadId":"thread",
            "receiverThreadIds":["child"]}}});
    let prefix = vec![
        start("7", "thread"),
        accepted("7", "turn"),
        turn("thread", "turn", "inProgress"),
    ];
    let mut rows = prefix.clone();
    rows.push(call.clone());
    rows.push(turn("child", "old-child-turn", "completed"));
    let (receipts, gaps) = scan(&source(Harness::Codex), rows)?;
    assert!(gaps.is_empty());
    let execution = &receipts[0].execution;
    assert_eq!(execution.outcome, None);
    assert!(execution.terminations.is_empty());
    assert_eq!(execution.agent_calls.len(), 1);
    assert_eq!(
        execution.agent_calls[0]
            .related_node
            .as_ref()
            .map(|node| node.native_id.as_str()),
        Some("child")
    );

    let mut conflicting = call;
    conflicting["payload"]["item"]["senderThreadId"] = json!("other-thread");
    let mut rows = prefix;
    rows.extend([conflicting, turn("thread", "turn", "completed")]);
    let (receipts, _) = scan(&source(Harness::Codex), rows)?;
    assert!(receipts[0].execution.agent_calls.is_empty());
    assert!(
        receipts[0]
            .execution
            .gaps
            .contains("native_execution_call_identity_conflict")
    );
    assert_eq!(receipts[0].execution.outcome, None);
    Ok(())
}

#[test]
fn codex_cannot_close_with_replayed_reversed_missing_or_foreign_bookends() -> Result<()> {
    let begin = turn("thread", "turn", "inProgress");
    let end = turn("thread", "turn", "completed");
    let cases = [
        (vec![end.clone()], "native_execution_start_unobserved"),
        (
            vec![end.clone(), begin.clone()],
            "native_execution_bookends_reversed",
        ),
        (
            vec![begin.clone(), end.clone(), end.clone()],
            "native_execution_bookends_ambiguous",
        ),
        (
            vec![begin.clone(), begin, end],
            "native_execution_bookends_ambiguous",
        ),
    ];
    for (tail, gap) in cases {
        let mut rows = vec![start("7", "thread"), accepted("7", "turn")];
        rows.extend(tail);
        let (receipts, _) = scan(&source(Harness::Codex), rows)?;
        assert_eq!(receipts[0].execution.outcome, None, "{gap}");
        assert!(receipts[0].execution.gaps.contains(gap));
    }
    let (receipts, _) = scan(
        &source(Harness::Codex),
        vec![
            turn("thread", "turn", "inProgress"),
            start("7", "thread"),
            accepted("7", "turn"),
            turn("thread", "turn", "completed"),
        ],
    )?;
    assert!(
        receipts[0]
            .execution
            .gaps
            .contains("native_execution_precedes_input")
    );
    assert_eq!(receipts[0].execution.outcome, None);

    let (receipts, _) = scan(
        &source(Harness::Codex),
        vec![
            start("7", "thread"),
            accepted("7", "turn"),
            turn("thread", "turn", "inProgress"),
            turn("other-thread", "turn", "completed"),
        ],
    )?;
    assert!(receipts[0].execution.terminations.is_empty());
    assert_eq!(receipts[0].execution.outcome, None);
    Ok(())
}

#[test]
fn claude_result_is_bound_to_command_and_native_conversation_after_reset() -> Result<()> {
    let (receipts, gaps) = scan(
        &source(Harness::ClaudeCode),
        vec![
            input(),
            command("native-before", "queued"),
            command("native-after", "started"),
            result("native-before", Some("command"), "success", false),
            result("native-after", Some("other-command"), "success", false),
            result("native-after", Some("command"), "success", false),
            command("native-after", "completed"),
        ],
    )?;
    assert!(gaps.is_empty());
    assert_eq!(receipts.len(), 2);
    let after = receipts
        .iter()
        .find(|receipt| receipt.node.native_id == "native-after")
        .context("reset receipt")?;
    assert_eq!(after.execution.outcome, Some(InputOutcome::Completed));
    assert_eq!(
        after
            .execution
            .records
            .iter()
            .map(|row| row.range.start)
            .collect::<Vec<_>>(),
        [2, 5, 6]
    );
    let before = receipts
        .iter()
        .find(|receipt| receipt.node.native_id == "native-before")
        .context("old receipt")?;
    assert_eq!(before.execution.outcome, None);
    Ok(())
}

#[test]
fn claude_command_completion_needs_a_consistent_explicit_result() -> Result<()> {
    let cases = [
        (vec![], "native_execution_result_unobserved"),
        (
            vec![result("native", Some("other"), "success", false)],
            "native_execution_result_unobserved",
        ),
        (
            vec![result("native", None, "success", false)],
            "native_execution_identity_missing",
        ),
        (
            vec![result("native", Some("command"), "success", false); 2],
            "native_execution_bookends_ambiguous",
        ),
    ];
    for (results, gap) in cases {
        let mut rows = vec![input(), command("native", "started")];
        rows.extend(results);
        rows.push(command("native", "completed"));
        let (receipts, _) = scan(&source(Harness::ClaudeCode), rows)?;
        assert_eq!(receipts[0].execution.outcome, None, "{gap}");
        assert!(
            receipts[0].execution.gaps.contains(gap),
            "{gap}: {:?}",
            receipts[0].execution.gaps
        );
    }
    for (state, status, error, outcome) in [
        ("completed", "success", false, InputOutcome::Completed),
        ("completed", "success", true, InputOutcome::Failed),
        (
            "completed",
            "error_during_execution",
            false,
            InputOutcome::Failed,
        ),
        (
            "completed",
            "error_during_execution",
            true,
            InputOutcome::Failed,
        ),
        (
            "cancelled",
            "error_during_execution",
            true,
            InputOutcome::Interrupted,
        ),
    ] {
        let (receipts, _) = scan(
            &source(Harness::ClaudeCode),
            vec![
                input(),
                command("native", "started"),
                result("native", Some("command"), status, error),
                command("native", state),
            ],
        )?;
        assert_eq!(receipts[0].execution.outcome, Some(outcome));
    }
    for (state, outcome) in [
        ("completed", None),
        ("cancelled", Some(InputOutcome::Interrupted)),
    ] {
        let (receipts, _) = scan(
            &source(Harness::ClaudeCode),
            vec![
                input(),
                command("native", "started"),
                command("native", state),
                result("native", Some("command"), "success", false),
            ],
        )?;
        assert_eq!(receipts[0].execution.outcome, outcome);
        assert_eq!(
            receipts[0]
                .execution
                .gaps
                .contains("native_execution_result_follows_completion"),
            state == "completed"
        );
    }
    let (receipts, _) = scan(
        &source(Harness::ClaudeCode),
        vec![
            input(),
            command("native", "queued"),
            command("native", "cancelled"),
        ],
    )?;
    assert_eq!(receipts[0].execution.outcome, Some(InputOutcome::Cancelled));
    assert!(receipts[0].execution.starts.is_empty() && receipts[0].execution.results.is_empty());
    Ok(())
}

#[test]
fn claude_terminal_reasons_keep_deferral_abort_and_future_states_distinct() -> Result<()> {
    for (reason, outcome) in [
        ("completed", Some(InputOutcome::Completed)),
        ("api_error", Some(InputOutcome::Failed)),
        ("aborted_tools", Some(InputOutcome::Interrupted)),
        ("aborted_streaming", Some(InputOutcome::Interrupted)),
        ("background_requested", Some(InputOutcome::Deferred)),
        ("tool_deferred", Some(InputOutcome::Deferred)),
        ("hook_stopped", Some(InputOutcome::Stopped)),
        ("future_reason", None),
    ] {
        let mut completion = result("native", Some("command"), "success", false);
        completion["payload"]["terminal_reason"] = json!(reason);
        let (receipts, _) = scan(
            &source(Harness::ClaudeCode),
            vec![
                input(),
                command("native", "started"),
                completion,
                command("native", "completed"),
            ],
        )?;
        assert_eq!(receipts[0].execution.outcome, outcome, "{reason}");
        assert_eq!(
            receipts[0].execution.results[0].terminal_reason.as_deref(),
            Some(reason)
        );
        assert_eq!(
            receipts[0]
                .execution
                .gaps
                .contains("native_execution_terminal_reason_unknown"),
            outcome.is_none()
        );
    }
    let mut completion = result("native", Some("command"), "success", false);
    completion["payload"]["terminal_reason"] = json!("future_reason");
    let (receipts, _) = scan(
        &source(Harness::ClaudeCode),
        vec![
            input(),
            command("native", "started"),
            command("native", "cancelled"),
            completion,
        ],
    )?;
    assert_eq!(receipts[0].execution.outcome, None);
    assert!(
        receipts[0]
            .execution
            .gaps
            .contains("native_execution_terminal_reason_unknown")
    );
    Ok(())
}

#[test]
fn ambiguous_requests_are_removed_before_materializing_a_shared_execution() -> Result<()> {
    let mut rows = Vec::new();
    for id in 0..MAX_GRAPH_ITEMS {
        rows.extend([
            start(&id.to_string(), "thread"),
            accepted(&id.to_string(), "turn"),
        ]);
    }
    rows.push(turn("thread", "turn", "inProgress"));
    for _ in 0..2048 {
        rows.push(
            json!({"method":"item/agentMessage/delta","direction":"server","phase":"notification",
            "payload":{"threadId":"thread","turnId":"turn","itemId":"message","delta":"text"}}),
        );
    }
    rows.push(turn("thread", "turn", "completed"));
    let (receipts, gaps) = scan(&source(Harness::Codex), rows)?;
    assert!(receipts.is_empty());
    assert!(gaps.contains("native_input_turn_ambiguous"));
    Ok(())
}

#[test]
fn claude_rejection_does_not_become_execution_or_override_a_start() -> Result<()> {
    for (state, outcome) in [
        ("discarded", InputOutcome::Discarded),
        ("refused", InputOutcome::Refused),
    ] {
        let (receipts, _) = scan(
            &source(Harness::ClaudeCode),
            vec![input(), command("native", state)],
        )?;
        assert_eq!(receipts[0].execution.outcome, Some(outcome));
        assert!(receipts[0].execution.starts.is_empty());
        let (receipts, _) = scan(
            &source(Harness::ClaudeCode),
            vec![
                input(),
                command("native", "started"),
                command("native", state),
            ],
        )?;
        assert_eq!(receipts[0].execution.outcome, None);
        assert!(
            receipts[0]
                .execution
                .gaps
                .contains("native_execution_rejection_conflict")
        );
    }
    let (receipts, _) = scan(
        &source(Harness::ClaudeCode),
        vec![
            input(),
            result("native", Some("command"), "success", false),
            command("native", "started"),
            command("native", "completed"),
        ],
    )?;
    assert_eq!(receipts[0].execution.outcome, None);
    assert!(
        receipts[0]
            .execution
            .gaps
            .contains("native_execution_result_precedes_start")
    );
    Ok(())
}

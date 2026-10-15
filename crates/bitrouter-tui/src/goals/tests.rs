use super::*;
use crossterm::event::{KeyEvent, KeyEventKind};

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}
fn goal(id: &str) -> Goal {
    Goal {
        id: id.into(),
        revision: 1,
        title: format!("Goal {id}"),
        workspace: "/project".into(),
        execution: "running".into(),
        satisfaction: "incomplete".into(),
        reason: "working".into(),
        tasks: vec![Task {
            id: "branch".into(),
            title: "Independent work".into(),
            status: "running".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn external_editor_targets_only_the_visible_goal_composer() {
    let mut state = State::new("/project".into());
    state.screen = Screen::New;
    state.new_editor.set_text("new draft 中文\nkeep  ");
    let edit = key(KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert_eq!(
        state.event(&edit),
        vec![Effect::ExternalEditor {
            goal_id: None,
            text: "new draft 中文\nkeep  ".into(),
        }]
    );
    state.receive_directory(vec![goal("first"), goal("second")], false, false);
    state.observe("first".into());
    if let Some(book) = state.notebooks.get_mut("first") {
        book.editor.set_text("first draft\nline two  ");
    }
    assert_eq!(
        state.event(&edit),
        vec![Effect::ExternalEditor {
            goal_id: Some("first".into()),
            text: "first draft\nline two  ".into(),
        }]
    );
    state.screen = Screen::Workflow;
    assert!(state.event(&edit).is_empty());
    state.screen = Screen::Detail;
    assert!(state.event(&edit).is_empty());
    assert_eq!(
        state.notebooks.get("first").map(|book| book.editor.text()),
        Some("first draft\nline two  ")
    );
    assert!(
        state
            .notebooks
            .get("second")
            .is_some_and(|book| book.editor.is_empty())
    );
}

#[test]
fn hidden_permission_choices_cannot_survive_a_below_minimum_viewport() {
    let mut state = State::new("/project".into());
    let mut current = goal("first");
    current.decisions.push(permission("actual-request", 1));
    state.receive(current);
    state.observe("first".into());
    state.viewport = Some((40, 12));
    state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    state.viewport = Some((30, 10));
    for code in [
        KeyCode::Down,
        KeyCode::Char('1'),
        KeyCode::Enter,
        KeyCode::Tab,
    ] {
        assert!(state.event(&key(code, KeyModifiers::NONE)).is_empty());
    }
    assert!(
        state
            .decision
            .as_ref()
            .is_some_and(|dialog| dialog.selected.is_none())
    );
    state.viewport = Some((40, 12));
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert!(
        state
            .decision
            .as_ref()
            .is_some_and(|dialog| dialog.selected == Some(0))
    );
}

fn permission(id: &str, revision: u64) -> Decision {
    Decision {
        id: id.into(),
        requirements_revision: revision,
        label: "Review this actual request".into(),
        details: vec!["Assigned workspace: /project".into()],
        request: Artifact {
            id: format!("request-{id}"),
            label: "Actual request source".into(),
        },
        options: vec![
            DecisionOption {
                id: "allow-once".into(),
                label: "Allow once".into(),
                allows: true,
                persistent: false,
            },
            DecisionOption {
                id: "deny".into(),
                label: "Deny".into(),
                allows: false,
                persistent: false,
            },
        ],
    }
}
fn state() -> State {
    let mut state = State::new("/project".into());
    state.receive_directory(vec![goal("a"), goal("b")], false, false);
    state.observe("a".into());
    state
}

#[test]
fn archived_records_stay_read_only_pinned_and_return_to_the_original_task() {
    let mut state = state();
    state.event(&Event::Paste("keep current Goal draft".into()));
    if let Some(goal) = state.goals.get_mut("a") {
        goal.tasks[0].artifacts.push(Artifact {
            id: "archive-2".into(),
            label: "Older record".into(),
        });
    }
    state.event(&key(KeyCode::Right, KeyModifiers::CONTROL));
    state.event(&key(KeyCode::Enter, KeyModifiers::NONE));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    let archived = |id: &str, previous: &str| Task {
        id: "branch".into(),
        title: id.into(),
        details: vec!["Original permissions and evidence".into()],
        artifacts: vec![Artifact {
            id: previous.into(),
            label: "Even older record".into(),
        }],
        ..Default::default()
    };
    assert!(!state.receive_archive("b", "archive-2", archived("wrong Goal", "old")));
    assert!(state.receive_archive(
        "a",
        "archive-2",
        archived("Original execution 2", "archive-1")
    ));
    let mut newer = goal("a");
    newer.revision = 2;
    state.receive(newer);
    assert_eq!(
        state.detail.as_ref().map(|task| task.title.as_str()),
        Some("Original execution 2")
    );
    assert!(state.detail_updated);
    for code in ['p', 'x', 'r', 'a', 'b', 'g'] {
        assert!(
            state
                .event(&key(KeyCode::Char(code), KeyModifiers::CONTROL))
                .is_empty()
        );
    }
    state.event(&Event::Paste("do not edit the hidden draft".into()));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Inspect {
            goal_id: "a".into(),
            artifact_id: "archive-1".into(),
        }]
    );
    assert!(state.receive_archive(
        "a",
        "archive-1",
        archived("Original execution 1", "old-evidence")
    ));
    state.event(&key(KeyCode::Enter, KeyModifiers::NONE));
    state.receive_evidence(
        "a",
        "old-evidence",
        Ok("Original read-only tool evidence".into()),
    );
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.artifact_preview.is_none());
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        state.detail.as_ref().map(|task| task.title.as_str()),
        Some("Independent work")
    );
    assert_eq!(state.notebook().map(|book| book.detail_offset), Some(1));
    assert!(!state.receive_archive("a", "archive-1", archived("late obsolete read", "old")));
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Submit {
            goal_id: "a".into(),
            text: "keep current Goal draft".into(),
        }]
    );
}

#[test]
fn busy_enter_targets_the_current_goal_and_preserves_the_draft_until_acceptance() {
    let mut state = state();
    state.event(&Event::Paste(
        "why is this slow?\nalso fix the regression".into(),
    ));
    let effects = state.event(&key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        effects,
        vec![Effect::Submit {
            goal_id: "a".into(),
            text: "why is this slow?\nalso fix the regression".into()
        }]
    );
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("why is this slow?\nalso fix the regression")
    );
    state.pending(Some("a"), true);
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    state.accepted(Some("a"), "why is this slow?\nalso fix the regression");
    assert!(state.notebook().is_some_and(|book| book.editor.is_empty()));
}

#[test]
fn right_view_keys_and_paste_cannot_mutate_or_retarget_the_middle() {
    let mut state = state();
    state.event(&Event::Paste("draft for a".into()));
    assert!(
        state
            .event(&key(KeyCode::Right, KeyModifiers::CONTROL))
            .is_empty()
    );
    assert_eq!(state.screen, Screen::Workflow);
    for code in ['p', 'x', 'r', 'a', 'b'] {
        assert!(
            state
                .event(&key(KeyCode::Char(code), KeyModifiers::CONTROL))
                .is_empty()
        );
    }
    assert!(
        state
            .event(&Event::Paste("untrusted input".into()))
            .is_empty()
    );
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    assert_eq!(state.screen, Screen::Detail);
    assert_eq!(state.current.as_deref(), Some("a"));
    assert!(
        state
            .event(&key(KeyCode::Esc, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(
        state
            .event(&key(KeyCode::Left, KeyModifiers::NONE))
            .is_empty()
    );
    assert_eq!(state.screen, Screen::Goal);
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Submit {
            goal_id: "a".into(),
            text: "draft for a".into()
        }]
    );
}

#[test]
fn budget_form_pins_goal_and_limits_without_editing_the_goal_draft() {
    let mut state = state();
    state.event(&Event::Paste("keep this objective draft".into()));
    if let Some(goal) = state.goals.get_mut("a") {
        goal.budget_limits.elapsed_ms = Some(10_000);
    }
    state.event(&key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    if let Some(dialog) = &mut state.budget_dialog {
        dialog.fields[0].set_text("20.001");
        dialog.fields[1].set_text("1.000001");
    }
    // A newer poll does not silently change the opened form's expected limits.
    state.receive(goal("a"));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::SetBudget {
            goal_id: "a".into(),
            original: budget::Limits {
                elapsed_ms: Some(10_000),
                ..Default::default()
            },
            limits: budget::Limits {
                elapsed_ms: Some(20_001),
                managed_token_estimates_micro_usd: Some(1_000_001)
            },
        }]
    );
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("keep this objective draft")
    );
}

#[test]
fn initial_budget_is_part_of_create_and_invalid_forms_cannot_submit() {
    let mut state = state();
    state.screen = Screen::New;
    state.new_editor.set_text("delegate this workflow");
    state.event(&key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    if let Some(dialog) = &mut state.budget_dialog {
        dialog.fields[0].set_text("-1");
    }
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(state.budget_dialog.is_some());
    if let Some(dialog) = &mut state.budget_dialog {
        dialog.fields[0].set_text("60");
        dialog.fields[1].set_text("0");
    }
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Create {
            text: "delegate this workflow".into(),
            limits: budget::Limits {
                elapsed_ms: Some(60_000),
                managed_token_estimates_micro_usd: Some(0)
            },
        }]
    );
}

#[test]
fn directory_filters_use_facts_and_never_start_or_retarget_work() {
    let mut state = state();
    if let Some(goal) = state.goals.get_mut("a") {
        goal.title = "complete and running are merely words".into();
    }
    if let Some(goal) = state.goals.get_mut("b") {
        goal.facts.running = true;
        goal.facts.budget_limited = true;
        goal.needs_you = true;
    }
    state.screen = Screen::Goals;
    assert!(
        state
            .event(&key(KeyCode::Char('f'), KeyModifiers::NONE))
            .is_empty()
    );
    assert_eq!(state.filter, Filter::Running);
    assert_eq!(state.directory_ids(), vec!["b"]);
    assert_eq!(state.current.as_deref(), Some("a"));
    state.event(&key(KeyCode::Char('f'), KeyModifiers::NONE));
    assert_eq!(state.filter, Filter::NeedsYou);
    assert_eq!(state.directory_ids(), vec!["b"]);
    state.filter = Filter::BudgetLimited;
    assert_eq!(state.directory_ids(), vec!["b"]);
    state.filter = Filter::Completed;
    assert!(state.directory_ids().is_empty());
}

#[test]
fn expanded_readonly_nodes_preserve_status_scope_draft_and_per_goal_state() {
    let mut state = state();
    state.event(&Event::Paste("do not retarget this draft".into()));
    if let Some(goal) = state.goals.get_mut("a") {
        goal.tasks[0].status = "effects unknown".into();
        goal.tasks[0].dependencies = vec!["upstream".into()];
        goal.tasks[0].resource = "harness: configured".into();
    }
    state.event(&key(KeyCode::Right, KeyModifiers::CONTROL));
    assert!(
        state
            .event(&key(KeyCode::Char(' '), KeyModifiers::NONE))
            .is_empty()
    );
    assert!(
        state
            .notebook()
            .is_some_and(|book| book.expanded_tasks.contains("branch"))
    );
    let (expanded, _) = render::footer(&state, 80, 24);
    assert!(
        expanded
            .iter()
            .any(|row| row.to_string().contains("requires: upstream"))
    );
    state.event(&key(KeyCode::Char(' '), KeyModifiers::NONE));
    let (collapsed, _) = render::footer(&state, 80, 24);
    assert!(
        collapsed
            .iter()
            .any(|row| row.to_string().contains("effects unknown"))
    );
    assert!(
        !collapsed
            .iter()
            .any(|row| row.to_string().contains("requires: upstream"))
    );
    assert_eq!(state.current.as_deref(), Some("a"));
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("do not retarget this draft")
    );
    state.observe("b".into());
    assert!(
        state
            .notebook()
            .is_some_and(|book| book.expanded_tasks.is_empty())
    );
}

#[test]
fn selected_directory_and_workflow_rows_remain_visible_in_short_viewports() {
    let mut state = state();
    state.receive_directory(
        (0..32)
            .map(|index| goal(&format!("goal-{index}")))
            .collect(),
        false,
        false,
    );
    state.screen = Screen::Goals;
    for index in [0, 17, 31] {
        let id = format!("goal-{index}");
        state.selected_goal = Some(id.clone());
        let (rows, _) = render::footer(&state, 80, 12);
        assert!(rows.iter().any(|row| {
            let text = row.to_string();
            text.starts_with('>') && text.contains(&format!("Goal {id} ·"))
        }));
        assert!(rows.len() <= 10);
    }
    state.observe("goal-0".into());
    if let Some(goal) = state.goals.get_mut("goal-0") {
        goal.tasks = (0..32)
            .map(|index| Task {
                id: format!("task-{index}"),
                status: "waiting".into(),
                ..Default::default()
            })
            .collect();
    }
    state.screen = Screen::Workflow;
    for index in [0, 17, 31] {
        if let Some(book) = state.notebooks.get_mut("goal-0") {
            book.selected_task = Some(format!("task-{index}"));
        }
        let (rows, _) = render::footer(&state, 80, 12);
        assert!(rows.iter().any(|row| {
            row.to_string()
                .starts_with(&format!(">+ waiting · task-{index} ·"))
        }));
        assert!(rows.len() <= 10);
    }
}

#[test]
fn fullscreen_browsing_preserves_the_pinned_version_read_position_and_draft() {
    let mut state = state();
    state.event(&Event::Paste("original draft".into()));
    state.event(&key(KeyCode::Right, KeyModifiers::CONTROL));
    state.event(&key(KeyCode::Enter, KeyModifiers::NONE));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert!(
        state
            .event(&key(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .is_empty()
    );
    assert!(state.fullscreen);
    assert_eq!(state.screen, Screen::Detail);
    assert!(
        state
            .event(&key(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .is_empty()
    );
    state.receive(goal("a"));
    assert!(
        state
            .event(&key(KeyCode::Esc, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(!state.fullscreen);
    assert_eq!(state.screen, Screen::Detail);
    assert_eq!(state.detail_revision, 1);
    assert_eq!(state.notebook().map(|book| book.detail_offset), Some(1));
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Submit {
            goal_id: "a".into(),
            text: "original draft".into(),
        }]
    );
}

#[test]
fn drafts_pending_scope_and_read_positions_are_independent_per_goal() {
    let mut state = state();
    state.event(&Event::Paste("original a".into()));
    state.pending(Some("a"), true);
    if let Some(book) = state.notebooks.get_mut("a") {
        book.detail_offset = 9;
    }
    state.observe("b".into());
    state.event(&Event::Paste("different b".into()));
    state.accepted(Some("a"), "original a");
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("different b")
    );
    state.observe("a".into());
    assert!(
        state
            .notebook()
            .is_some_and(|book| book.editor.is_empty() && book.detail_offset == 9)
    );
    assert_eq!(state.current.as_deref(), Some("a"));
}

#[test]
fn empty_arrows_navigate_but_nonempty_arrows_edit() {
    let mut state = state();
    state.event(&Event::Paste("修复登录".into()));
    state.event(&key(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(state.screen, Screen::Goal);
    state.event(&key(KeyCode::Char('!'), KeyModifiers::NONE));
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("修复登!录")
    );
    state.accepted(Some("a"), "修复登!录");
    assert_eq!(
        state.event(&key(KeyCode::Left, KeyModifiers::NONE)),
        vec![Effect::Refresh]
    );
    assert_eq!(state.screen, Screen::Goals);
}

#[test]
fn snapshots_preserve_selection_and_pin_opened_evidence() {
    let mut state = state();
    state.selected_goal = Some("a".into());
    state.receive_directory(vec![goal("b"), goal("a")], true, false);
    assert_eq!(state.directory, vec!["a", "b"]);
    assert_eq!(state.selected_goal.as_deref(), Some("a"));
    state.event(&key(KeyCode::Right, KeyModifiers::NONE));
    state.event(&key(KeyCode::Enter, KeyModifiers::NONE));
    let mut fresh = goal("a");
    fresh.revision = 2;
    fresh.tasks[0].status = "stopped".into();
    state.receive(fresh);
    assert!(state.detail_updated);
    assert_eq!(
        state.detail.as_ref().map(|task| task.status.as_str()),
        Some("running")
    );
    assert_eq!(state.detail_revision, 1);
}

#[test]
fn decision_has_no_default_consent_and_preserves_the_offered_version() {
    let mut state = state();
    let mut first = goal("a");
    first.decisions = vec![permission("permission", 7)];
    state.receive(first.clone());
    assert_eq!(
        state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        vec![Effect::Inspect {
            goal_id: "a".into(),
            artifact_id: "request-permission".into()
        }]
    );
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    first.revision += 1;
    state.receive(first);
    state.receive_evidence("a", "request-permission", Ok("actual request".into()));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Respond {
            goal_id: "a".into(),
            decision_id: "permission".into(),
            requirements_revision: 7,
            option_id: "allow-once".into()
        }]
    );
}

#[test]
fn allow_requires_loaded_request_and_explicit_deny_remains_available() {
    let mut state = state();
    let mut first = goal("a");
    let mut decision = permission("permission", 7);
    // Labels are untrusted display data; they cannot turn a deny into allow.
    decision.options[0].label = "Deny-looking label".into();
    decision.options[1].label = "Allow-looking label".into();
    first.decisions = vec![decision];
    state.receive(first);
    state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    state.receive_evidence("a", "request-permission", Err("hash mismatch".into()));
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Respond {
            goal_id: "a".into(),
            decision_id: "permission".into(),
            requirements_revision: 7,
            option_id: "deny".into()
        }]
    );
}

#[test]
fn decision_browsing_uses_pinned_identities_and_ignores_late_foreign_evidence() {
    let mut state = state();
    state.event(&Event::Paste("preserved Goal draft".into()));
    let mut first = goal("a");
    first.decisions = vec![permission("one", 7), permission("two", 8)];
    state.receive(first.clone());
    state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(
        state.event(&key(KeyCode::Tab, KeyModifiers::NONE)),
        vec![Effect::Inspect {
            goal_id: "a".into(),
            artifact_id: "request-two".into()
        }]
    );
    first.revision += 1;
    state.receive(first);
    state.receive_evidence("a", "request-one", Ok("late first request".into()));
    state.receive_evidence("b", "request-two", Ok("another Goal".into()));
    assert!(
        state
            .decision
            .as_ref()
            .is_some_and(|dialog| dialog.material.is_none() && dialog.selected.is_none())
    );
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    state.receive_evidence("a", "request-two", Ok("actual second request".into()));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Respond {
            goal_id: "a".into(),
            decision_id: "two".into(),
            requirements_revision: 8,
            option_id: "allow-once".into()
        }]
    );
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("preserved Goal draft")
    );
}

#[test]
fn resolved_permission_stays_focused_but_cannot_answer_or_submit_the_draft() {
    let mut state = state();
    state.event(&Event::Paste("unsent Goal draft".into()));
    let mut first = goal("a");
    first.decisions = vec![permission("one", 7), permission("two", 7)];
    state.receive(first.clone());
    state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    state.receive_evidence("a", "request-one", Ok("first original request".into()));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    let mut resolved = first.clone();
    resolved.revision += 1;
    resolved.decisions.remove(0);
    state.receive(resolved);
    state.receive(first); // Late snapshot cannot resurrect the resolved request.
    for code in [
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::Down,
        KeyCode::Char('r'),
    ] {
        assert!(state.event(&key(code, KeyModifiers::NONE)).is_empty());
    }
    state.event(&Event::Paste("do not paste into hidden composer".into()));
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("unsent Goal draft")
    );
    assert!(state.decision.as_ref().is_some_and(|dialog| dialog.stale));
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        vec![Effect::Inspect {
            goal_id: "a".into(),
            artifact_id: "request-two".into()
        }]
    );
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    state.receive_evidence("a", "request-one", Ok("late obsolete material".into()));
    assert!(
        state
            .decision
            .as_ref()
            .is_some_and(|dialog| dialog.material.is_none())
    );
    state.receive_evidence("a", "request-two", Ok("second original request".into()));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Respond {
            goal_id: "a".into(),
            decision_id: "two".into(),
            requirements_revision: 7,
            option_id: "allow-once".into()
        }]
    );
}

#[test]
fn changed_permission_binding_disables_the_pinned_choice() {
    let mut state = state();
    let mut first = goal("a");
    first.decisions = vec![permission("one", 7)];
    state.receive(first.clone());
    state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    state.receive_evidence("a", "request-one", Ok("original request".into()));
    state.event(&key(KeyCode::Down, KeyModifiers::NONE));
    first.revision += 1;
    first.decisions[0].request.id = "replacement-request".into();
    first.decisions[0].options[0].persistent = true;
    state.receive(first);
    state.receive_evidence("a", "request-one", Ok("late old evidence".into()));
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(state.decision.as_ref().is_some_and(|dialog| dialog.stale
        && dialog.material.is_none()
        && dialog.selected.is_none()));
    let (footer, _) = super::render::footer(&state, 100, 24);
    assert!(
        footer
            .iter()
            .any(|line| line.to_string().contains("Permission no longer pending"))
    );
}

#[test]
fn permission_options_stay_visible_when_requests_and_choices_are_long() {
    let mut state = state();
    let mut first = goal("a");
    let mut decision = permission("permission", 7);
    decision.options = (0..16)
        .map(|index| DecisionOption {
            id: format!("option-{index}"),
            label: format!("Actual option {index}"),
            allows: false,
            persistent: false,
        })
        .collect();
    first.decisions = vec![decision];
    state.receive(first);
    state.event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    state.receive_evidence(
        "a",
        "request-permission",
        Ok((0..100)
            .map(|index| format!("Evidence row {index}\n"))
            .collect()),
    );
    for index in [0, 8, 15] {
        if let Some(dialog) = &mut state.decision {
            dialog.selected = Some(index);
        }
        let (rows, _) = render::footer(&state, 40, 12);
        assert!(rows.iter().any(|row| {
            row.to_string()
                .starts_with(&format!("> Actual option {index} ·"))
        }));
        assert!(rows.len() <= 10);
    }
}

#[test]
fn acceptance_requires_explicit_confirmation_of_the_pinned_artifact() {
    let mut state = state();
    let mut first = goal("a");
    first.acceptance = Some(Acceptance {
        attempt_id: "delivery".into(),
        requirements_revision: 3,
        delivery_sha256: "old-artifact".into(),
    });
    state.receive(first);
    assert!(
        state
            .event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .is_empty()
    );
    let mut next = goal("a");
    next.acceptance = Some(Acceptance {
        attempt_id: "new-delivery".into(),
        requirements_revision: 4,
        delivery_sha256: "new-artifact".into(),
    });
    state.receive(next);
    assert_eq!(
        state.event(&key(KeyCode::Enter, KeyModifiers::NONE)),
        vec![Effect::Accept {
            goal_id: "a".into(),
            attempt_id: "delivery".into(),
            requirements_revision: 3,
            delivery_sha256: "old-artifact".into()
        }]
    );
}

#[test]
fn multiline_paste_shift_enter_and_key_release_do_not_submit() {
    let mut state = state();
    assert!(
        state
            .event(&Event::Paste("你好\nworld\n".into()))
            .is_empty()
    );
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::SHIFT))
            .is_empty()
    );
    let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert!(state.event(&Event::Key(release)).is_empty());
    assert_eq!(
        state.notebook().map(|book| book.editor.text()),
        Some("你好\nworld\n\n")
    );
}

#[test]
fn rendering_names_readonly_scope_and_neutralizes_terminal_control_data() {
    let mut state = state();
    if let Some(goal) = state.goals.get_mut("a") {
        goal.title = "attack\u{1b}]52;secret\u{7}".into();
    }
    let (rows, _) = render::footer(&state, 80, 24);
    let text = rows
        .iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\u{7}'));
    state.event(&key(KeyCode::Right, KeyModifiers::NONE));
    let (rows, cursor) = render::footer(&state, 80, 24);
    assert!(rows.iter().any(|row| row.to_string().contains("read only")));
    assert!(cursor.is_none());
    state.screen = Screen::Goal;
    state.viewport = Some((20, 8));
    assert!(
        state
            .event(&key(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .is_empty()
    );
}

#[test]
fn long_history_is_bounded_and_browsing_retains_the_goal_draft()
-> Result<(), Box<dyn std::error::Error>> {
    let mut state = State::new("/project".into());
    state.receive(goal("g"));
    state.observe("g".into());
    if let Some(book) = state.notebooks.get_mut("g") {
        book.editor.set_text("keep this draft 中文");
    }
    for index in 0..10_000 {
        state.event_line(
            "g",
            format!("persisted event {index}: {}", "中".repeat(200)),
        );
    }
    let book = &state.notebooks["g"];
    assert!(book.events.len() <= 128);
    assert!(book.events.iter().map(String::len).sum::<usize>() <= 256 * 1024);
    assert!(book.omitted_events > 0);
    let effects = state.event(&key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let [Effect::ReadHistory { goal_id, epoch, .. }] = effects.as_slice() else {
        return Err("history read effect missing".into());
    };
    let epoch = *epoch;
    assert_eq!(goal_id, "g");
    state.receive_history(
        "g",
        epoch,
        Ok((vec!["old persistent event".into()], 99, Some(16))),
    );
    assert!(
        state
            .event(&key(KeyCode::Enter, KeyModifiers::NONE))
            .is_empty()
    );
    assert!(
        state
            .event(&Event::Paste("must not edit".into()))
            .is_empty()
    );
    assert_eq!(state.notebooks["g"].editor.text(), "keep this draft 中文");
    assert_eq!(state.screen, Screen::History);
    let next = state.event(&key(KeyCode::PageDown, KeyModifiers::NONE));
    assert!(matches!(
        next.as_slice(),
        [Effect::ReadHistory {
            after: 16,
            cutoff: Some(99),
            ..
        }]
    ));
    state.receive_history("g", epoch, Ok((vec!["stale page".into()], 999, None)));
    assert_eq!(
        state.history.as_ref().map(|page| page.cutoff),
        Some(Some(99))
    );
    state.event(&key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(state.screen, Screen::Goal);
    assert_eq!(state.notebooks["g"].editor.text(), "keep this draft 中文");
    Ok(())
}

#[test]
fn directory_pages_release_idle_cache_and_keep_unsubmitted_drafts() {
    let mut state = State::new("/project".into());
    state.receive(goal("draft"));
    state.observe("draft".into());
    if let Some(book) = state.notebooks.get_mut("draft") {
        book.editor.set_text("unfinished");
    }
    for index in 0..1000 {
        let id = format!("page-{index}");
        state.receive_directory(vec![goal(&id)], true, true);
        state.observe(id);
    }
    state.receive_directory(vec![goal("latest")], false, true);
    assert!(state.goals.len() <= 3);
    assert!(state.notebooks.len() <= 3);
    assert_eq!(state.notebooks["draft"].editor.text(), "unfinished");
}

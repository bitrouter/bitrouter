//! Pure draft contracts; PostgreSQL atomicity/fencing is exercised by the caller's fixtures.

use std::collections::BTreeSet;

use super::*;
use crate::gateway::routing::workflow::ir::RouteProjection;
use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
use crate::trajectory::types::RequestStatus;

fn digest(epoch: &str, digit: char) -> Result<KeyedDigest> {
    KeyedDigest::parse(format!(
        "hmac-sha256:{epoch}:{}",
        digit.to_string().repeat(64)
    ))
}

fn input(id: &str) -> Result<CorrelateAndBegin> {
    Ok(CorrelateAndBegin {
        request_id: id.into(),
        new_episode_id: "episode".into(),
        event_id: format!("start-{id}"),
        correlation_key_id: "epoch-1".into(),
        native_parent_id: None,
        native_parent_digest: None,
        full_input_digest: digest("epoch-1", 'a')?,
        ancestor_prefix_digests: Vec::new(),
        ancestor_prefixes_truncated: false,
        starts_with_prior_turns: false,
        canonical_input_bytes: 10,
        protocol: "chat_completions".into(),
        captured_at: "2026-08-01T00:00:00Z".into(),
        guarded_route: Some(GuardedRouteInput {
            route_event_id: format!("route-{id}"),
            guard_event_id: format!("guard-{id}"),
            policy_name: "auto:cost".into(),
            route_projection: "semantic_route/v1|code:generation|implement|normal".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            baseline_tier: Some("reference".into()),
            baseline_effort: None,
            tier_efforts: Default::default(),
            preset: Some("auto:cost".into()),
            projection: RouteProjection::parse_key("agent_trace/v2|edit|normal")
                .context("invalid fixture projection")?,
            candidate_tier: Some("economy".into()),
            policy_digest: format!("sha256:{}", "b".repeat(64)),
            experiment: None,
            route_measurement: None,
            policy: ProgressGuardPolicy {
                escalation_tier: "strong".into(),
                protected_tiers: BTreeSet::from(["strong".into()]),
                max_consecutive_unprotected: None,
                max_same_projection_unprotected: None,
                max_recovery_count: None,
                max_episode_requests: Some(1),
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: None,
                hold_for_requests: 2,
                incomplete_history: IncompleteHistoryAction::Observe,
            },
            carries_tools: false,
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: BTreeSet::from(["strong".into()]),
        }),
    })
}

fn batch(prior: &[TrajectoryEvent], input: &CorrelateAndBegin) -> Result<Vec<TrajectoryEvent>> {
    let sequence = prior.last().map_or(1, |event| event.sequence + 1);
    let start = store::build_start_event(
        "owner",
        input,
        "episode",
        sequence,
        &store::episode_monotonic_captured_at(prior, &input.captured_at)?,
        StartFacts {
            source: if prior.is_empty() {
                CorrelationSource::ExplicitRoot
            } else {
                CorrelationSource::CanonicalPrefix
            },
            completeness: HistoryCompleteness::Complete,
            prefix_conflict: false,
            key_epoch_conflict: false,
        },
    )?;
    let route = store::build_guarded_route_batch(
        prior,
        &start,
        HistoryCompleteness::Complete,
        input.guarded_route.as_ref().context("guard missing")?,
    )?;
    let mut events = vec![start, route.route_event];
    events.extend(route.guard_event);
    Ok(events)
}

fn rows(
    input: &CorrelateAndBegin,
    history: &[TrajectoryEvent],
) -> Result<(episode_entity::Model, request_entity::Model)> {
    let first = history.first().context("fixture start missing")?;
    let head = history.last().context("fixture head missing")?;
    Ok((
        episode_entity::Model {
            episode_id: "episode".into(),
            owner_user_id: "owner".into(),
            correlation_digest: input.full_input_digest.as_str().into(),
            correlation_key_id: "epoch-1".into(),
            correlation_source: "explicit_root".into(),
            history_completeness: "complete".into(),
            next_sequence: i64::try_from(head.sequence + 1)?,
            first_captured_at: first.captured_at.clone(),
            last_captured_at: head.captured_at.clone(),
            closed_at: None,
            latest_request_id: head.request_id.clone(),
        },
        request_entity::Model {
            request_id: input.request_id.clone(),
            owner_user_id: "owner".into(),
            episode_id: "episode".into(),
            start_event_id: input.event_id.clone(),
            settlement_event_id: None,
            settlement_outbox_id: None,
            full_input_digest: input.full_input_digest.as_str().into(),
            native_parent_id: None,
            protocol: input.protocol.clone(),
            status: store::request_status_name(RequestStatus::Started).into(),
        },
    ))
}

#[test]
fn correlation_requires_proven_ancestry_or_genuine_root() -> Result<()> {
    let mut value = input("root")?;
    assert_eq!(
        select_episode(&value, None, None)?,
        (None, CorrelationSource::ExplicitRoot)
    );
    value.starts_with_prior_turns = true;
    assert!(select_episode(&value, None, None).is_err());
    assert_eq!(
        select_episode(&value, None, Some("episode".into()))?,
        (Some("episode".into()), CorrelationSource::CanonicalPrefix)
    );
    value.native_parent_id = Some("parent".into());
    assert!(select_episode(&value, None, None).is_err());
    assert!(select_episode(&value, Some("episode".into()), Some("other".into())).is_err());
    // Native Responses tool continuations can contain only the new tool turn;
    // they rely on the known native parent rather than a fabricated full transcript.
    assert_eq!(
        select_episode(&value, Some("episode".into()), None)?,
        (Some("episode".into()), CorrelationSource::NativeParentId)
    );
    value.ancestor_prefixes_truncated = true;
    assert!(select_episode(&value, Some("episode".into()), None).is_err());
    assert!(select_episode(&value, Some("episode".into()), Some("episode".into())).is_ok());
    let mut prefix = None;
    merge_prefix(&mut prefix, "episode".into())?;
    merge_prefix(&mut prefix, "episode".into())?;
    assert!(merge_prefix(&mut prefix, "other".into()).is_err());
    assert_eq!(prefix.as_deref(), Some("episode"));
    Ok(())
}

#[test]
fn epoch_and_input_bounds_fail_before_storage() -> Result<()> {
    let mut value = input("root")?;
    validate_input("owner", &value)?;
    value.ancestor_prefix_digests = vec![digest("epoch-1", 'b')?; MAX_ANCESTOR_PREFIX_DIGESTS + 1];
    assert!(validate_input("owner", &value).is_err());
    value.ancestor_prefix_digests = vec![digest("epoch-2", 'b')?];
    assert!(validate_input("owner", &value).is_err());
    value.ancestor_prefix_digests.clear();
    value.native_parent_id = Some("parent".into());
    assert!(validate_input("owner", &value).is_err());
    value.native_parent_digest = Some(digest("epoch-2", 'c')?);
    assert!(validate_input("owner", &value).is_err());
    value.native_parent_digest = Some(digest("epoch-1", 'c')?);
    validate_input("owner", &value)?;
    value.guarded_route = None;
    assert!(validate_input("owner", &value).is_err());
    Ok(())
}

#[test]
fn fresh_start_rejects_other_owner_epoch_unresolved_and_closed_episode() -> Result<()> {
    let value = input("root")?;
    let history = batch(&[], &value)?;
    let (mut episode, _) = rows(&value, &history)?;
    validate_open_epoch("owner", &episode, &value)?;
    assert!(validate_open_epoch("other", &episode, &value).is_err());
    episode.correlation_key_id = "epoch-2".into();
    assert!(validate_open_epoch("owner", &episode, &value).is_err());
    episode.correlation_key_id = "epoch-1".into();
    episode.correlation_digest = digest("epoch-2", 'a')?.as_str().into();
    assert!(validate_open_epoch("owner", &episode, &value).is_err());
    episode.correlation_digest = value.full_input_digest.as_str().into();
    episode.correlation_source = "unresolved".into();
    assert!(validate_open_epoch("owner", &episode, &value).is_err());
    episode.correlation_source = "explicit_root".into();
    episode.closed_at = Some(value.captured_at.clone());
    assert!(validate_open_epoch("owner", &episode, &value).is_err());
    Ok(())
}

#[test]
fn exact_retry_reconstructs_original_guard_after_later_head() -> Result<()> {
    let first = input("first")?;
    let mut history = batch(&[], &first)?;
    let (episode, request) = rows(&first, &history)?;
    let original = retry_from_history("owner", &episode, &request, &first, &history)?;
    let mut second = input("second")?;
    second.captured_at = "2026-08-01T00:00:05Z".into();
    second.full_input_digest = digest("epoch-1", 'c')?;
    second.ancestor_prefix_digests = vec![first.full_input_digest.clone()];
    second.starts_with_prior_turns = true;
    let later = batch(&history, &second)?;
    history.extend(later);
    let (episode, request) = rows(&first, &history)?;
    store::validate_episode_head(&episode, &history)?;
    let before = history.clone();
    let retried = retry_from_history("owner", &episode, &request, &first, &history)?;
    assert_eq!(retried.guarded_route, original.guarded_route);
    assert!(retried.prior_events.is_empty());
    assert_eq!(history, before);
    // Calling the same pure helper as local storage yields the identical result.
    let local = store::exact_retry_from_history("owner", &request, &first, &history, &history[0])?;
    assert_eq!(retried.guarded_route, local.guarded_route);
    assert_eq!(retried.source, local.source);
    assert_eq!(retried.completeness, local.completeness);
    let mut changed = input("first")?;
    changed.canonical_input_bytes += 1;
    assert!(retry_from_history("owner", &episode, &request, &changed, &history).is_err());
    changed = input("first")?;
    changed.captured_at = "2026-08-01T00:00:01Z".into();
    assert!(retry_from_history("owner", &episode, &request, &changed, &history).is_err());
    changed = input("first")?;
    changed
        .guarded_route
        .as_mut()
        .context("guard missing")?
        .candidate_tier = Some("strong".into());
    assert!(retry_from_history("owner", &episode, &request, &changed, &history).is_err());
    let incomplete = &history[..2]; // Original route declares a guard; its activation is missing.
    assert!(retry_from_history("owner", &episode, &request, &first, incomplete).is_err());
    Ok(())
}

#[test]
fn new_start_timestamp_uses_current_head_but_retry_uses_its_original_prefix() -> Result<()> {
    let first = input("first")?;
    let mut history = batch(&[], &first)?;
    let mut second = input("second")?;
    second.captured_at = "2026-07-01T00:00:00Z".into();
    second.ancestor_prefix_digests = vec![first.full_input_digest.clone()];
    second.starts_with_prior_turns = true;
    let next = batch(&history, &second)?;
    assert_eq!(next[0].captured_at, first.captured_at);
    history.extend(next);
    let (episode, mut request) = rows(&second, &history)?;
    request.full_input_digest = second.full_input_digest.as_str().into();
    let retry = retry_from_history("owner", &episode, &request, &second, &history)?;
    assert_eq!(retry.prior_events.len(), 3);
    Ok(())
}

#[test]
fn settlement_headroom_accounts_for_existing_and_new_unsettled_work() -> Result<()> {
    reserve_capacity(MAX_HISTORY_EVENTS - 1, 0, 1)?;
    assert!(reserve_capacity(MAX_HISTORY_EVENTS, 0, 1).is_err());
    reserve_capacity(1, MAX_HISTORY_BYTES - MAX_GUARDED_SETTLEMENT_BYTES, 1)?;
    assert!(reserve_capacity(1, MAX_HISTORY_BYTES - MAX_GUARDED_SETTLEMENT_BYTES + 1, 1).is_err());
    reserve_capacity(
        MAX_HISTORY_EVENTS - 2,
        MAX_HISTORY_BYTES - 2 * MAX_GUARDED_SETTLEMENT_BYTES,
        2,
    )?;
    assert!(reserve_capacity(MAX_HISTORY_EVENTS - 1, 0, 2).is_err());
    assert!(reserve_capacity(1, MAX_HISTORY_BYTES - MAX_GUARDED_SETTLEMENT_BYTES, 2).is_err());
    assert!(reserve_capacity(usize::MAX, 0, 1).is_err());
    assert!(reserve_capacity(1, usize::MAX, 1).is_err());
    assert!(reserve_capacity(1, 0, u64::MAX).is_err());
    let first = input("first")?;
    let history = batch(&[], &first)?;
    let second = input("second")?;
    let appended = batch(&history, &second)?;
    reserve_settlements(
        &history,
        &appended,
        second.guarded_route.as_ref().context("guard missing")?,
    )?;
    let snapshot = reduce(
        &history.iter().chain(&appended).cloned().collect::<Vec<_>>(),
        &BTreeSet::new(),
    )?;
    assert_eq!(snapshot.health.unsettled_request_count, 2);
    Ok(())
}

#[test]
fn native_parent_must_match_the_actual_history_start() -> Result<()> {
    let value = input("parent")?;
    let history = batch(&[], &value)?;
    let (_, row) = rows(&value, &history)?;
    let mut parent = store::stored_request(row)?;
    validate_parent_history("owner", &parent, &history)?;
    assert!(validate_parent_history("other", &parent, &history).is_err());
    parent.request_id = "different".into();
    assert!(validate_parent_history("owner", &parent, &history).is_err());
    parent.request_id = value.request_id;
    parent.start_event_id = "missing".into();
    assert!(validate_parent_history("owner", &parent, &history).is_err());
    Ok(())
}

#[test]
fn required_evaluation_cannot_hide_behind_available_history_bytes() -> Result<()> {
    use bitrouter_ai::eval::RouteActionCandidate;
    use bitrouter_ai::routing::ContextStrategy;
    let candidates = (0..64)
        .map(|index| RouteActionCandidate {
            context: ContextStrategy::Preserve,
            tier: if index == 0 {
                "economy".into()
            } else {
                format!("tier-{index:02}")
            },
            model: format!("{}{:02}", "m".repeat(510), index),
            effort: None,
            logging_probability_ppm: if index == 0 { 1_000_000 } else { 0 },
        })
        .collect::<Vec<_>>();
    let measurement = crate::storage::evaluation::route_decision_measurement(
        "economy",
        candidates[0].model.clone(),
        None,
        candidates,
    )?;
    let mut history = Vec::new();
    let mut last = input("unused")?;
    for index in 0..33 {
        let mut next = input(&format!("large-{index}"))?;
        next.guarded_route
            .as_mut()
            .context("guard missing")?
            .route_measurement = Some(measurement.clone());
        history.extend(batch(&history, &next)?);
        last = next;
    }
    let bytes = history
        .iter()
        .try_fold(0usize, |bytes, event| -> Result<usize> {
            Ok(bytes + serde_json::to_vec(event)?.len())
        })?;
    reserve_capacity(history.len(), bytes, 33)?;
    let error = headroom::reserve(&history, 33)
        .err()
        .context("oversized evaluation admitted")?;
    assert!(
        error
            .to_string()
            .contains("required evaluation outbox headroom")
    );
    let settlement = fixture_settlement(&last, &history, empty_evidence())?;
    let outbox = settlement.outbox.context("outbox missing")?;
    assert!(serde_json::to_vec(&outbox.payload)?.len() > super::super::MAX_OUTBOX_BYTES);
    Ok(())
}

#[test]
fn output_reserve_covers_future_receipts_predictions_and_interleaved_settlements() -> Result<()> {
    use bitrouter_ai::eval::EvidenceItem;
    use std::collections::BTreeMap;
    let inputs = [input("one")?, input("two")?, input("three")?];
    let mut history = Vec::new();
    for next in &inputs {
        let appended = batch(&history, next)?;
        reserve_settlements(
            &history,
            &appended,
            next.guarded_route.as_ref().context("guard missing")?,
        )?;
        history.extend(appended);
    }
    let original = history.clone();
    let reserved = headroom::upper_bound(&history, 3)?;
    for index in [1usize, 0, 2] {
        let mut evidence = empty_evidence();
        for key in [
            "predicted_role",
            "predicted_task_family",
            "task_family_reason_codes",
            "predicted_action",
            "predictor_contract_digest",
            "prediction_confidence_kind",
            "observed_action",
            "observation_reason_code",
            "action_match",
            "continuation_proposed_tier",
            "continuation_proposed_model",
            "continuation_adjustment",
        ] {
            evidence
                .categorical
                .insert(format!("routing.{key}"), "\\".repeat(128));
        }
        for key in [
            "prediction_confidence_ppm",
            "task_family_confidence_ppm",
            "observation_confidence_ppm",
        ] {
            evidence
                .structural
                .insert(format!("routing.{key}"), u64::MAX);
        }
        for key in ["total_tokens", "cost_micro_usd", "duration_ms"] {
            evidence.structural.insert(format!("settlement.{key}"), 0);
        }
        for kind in ["routing.assessment", "routing.plan"] {
            evidence.routing_evidence.push(EvidenceItem {
                evidence_id: format!("{index}{}{}", &kind[..1], "\\".repeat(510)),
                kind: kind.into(),
                digest: format!("sha256:{}", "a".repeat(64)),
                redacted: true,
                attributes: BTreeMap::from([("model".into(), "\\".repeat(8192))]),
            });
        }
        // Distinct kinds above must also have distinct immutable identities.
        evidence.routing_evidence[1]
            .evidence_id
            .replace_range(1..2, "p");
        let settlement = fixture_settlement(&inputs[index], &history, evidence)?;
        crate::trajectory::types::validate_event(&settlement.event)?;
        assert!(serde_json::to_vec(&settlement.event)?.len() <= MAX_GUARDED_SETTLEMENT_BYTES);
        let outbox = settlement.outbox.as_ref().context("outbox missing")?;
        crate::trajectory::types::validate_outbox_payload(&outbox.payload)?;
        assert!(serde_json::to_vec(&outbox.payload)?.len() <= reserved);
        history.push(settlement.event);
    }
    assert_eq!(&history[..original.len()], &original);
    assert!(headroom::upper_bound(&original, u64::MAX).is_err());
    Ok(())
}

fn empty_evidence() -> crate::trajectory::types::TrajectoryEvidence {
    crate::trajectory::types::TrajectoryEvidence {
        structural: Default::default(),
        categorical: Default::default(),
        digests: Default::default(),
        route_measurement: None,
        routing_evidence: Vec::new(),
    }
}

fn fixture_settlement(
    input: &CorrelateAndBegin,
    history: &[TrajectoryEvent],
    evidence: crate::trajectory::types::TrajectoryEvidence,
) -> Result<crate::trajectory::types::Settlement> {
    let head = history.last().context("history missing")?;
    let request = StoredRequest {
        request_id: input.request_id.clone(),
        episode_id: "episode".into(),
        start_event_id: input.event_id.clone(),
        settlement_event_id: None,
        full_input_digest: input.full_input_digest.clone(),
        native_parent_id: None,
        protocol: input.protocol.clone(),
        status: RequestStatus::Started,
    };
    crate::trajectory::settlement::build_from_evidence(
        "owner",
        &request,
        history,
        head.sequence + 1,
        evidence,
        head.captured_at.clone(),
        RequestStatus::Failed,
    )
}

#[test]
fn sizing_deduplicates_frozen_receipts_but_rejects_changed_same_identity() -> Result<()> {
    use crate::gateway::model::decision::types::DecisionUsage;
    use crate::gateway::observability::decision::routing_evidence_from;
    use crate::gateway::routing::preparation::Receipt;
    let first = input("receipt-first")?;
    let mut history = batch(&[], &first)?;
    let receipt = Receipt {
        id: "frozen-native-decision".into(),
        assessment: None,
        error: None,
        usage: Some(DecisionUsage {
            input_tokens: 1,
            output_tokens: 2,
        }),
    };
    let mut evidence = empty_evidence();
    evidence.routing_evidence = routing_evidence_from("execution-one", Some(&receipt), None)?;
    let settled = fixture_settlement(&first, &history, evidence.clone())?;
    history.push(settled.event);
    let second = input("receipt-second")?;
    history.extend(batch(&history, &second)?);
    let second_settlement = fixture_settlement(&second, &history, evidence.clone())?;
    let mut same = history.clone();
    same.push(second_settlement.event.clone());
    let without_second_receipt = {
        let mut copy = same.clone();
        let event = copy.last_mut().context("settlement missing")?;
        event.evidence.routing_evidence.clear();
        event.content_digest = event.semantic_digest()?;
        copy
    };
    // The count argument reserves the same hypothetical future request on
    // each side; only retained receipt deduplication is being compared.
    assert_eq!(
        headroom::upper_bound(&same, 1)?,
        headroom::upper_bound(&without_second_receipt, 1)?
    );

    let mut changed = receipt;
    changed.usage = Some(DecisionUsage {
        input_tokens: 3,
        output_tokens: 2,
    });
    let changed_evidence = routing_evidence_from("execution-two", Some(&changed), None)?;
    assert_eq!(
        evidence.routing_evidence[0].evidence_id,
        changed_evidence[0].evidence_id
    );
    assert_ne!(evidence.routing_evidence[0], changed_evidence[0]);
    let event = same.last_mut().context("settlement missing")?;
    event.evidence.routing_evidence = changed_evidence;
    event.content_digest = event.semantic_digest()?;
    let error = headroom::upper_bound(&same, 1)
        .err()
        .context("changed receipt identity accepted")?;
    assert!(
        error
            .to_string()
            .contains("routing receipt identity has conflicting evidence")
    );
    assert!(crate::trajectory::evaluation::build_operational_evaluation(&same).is_err());
    Ok(())
}

use super::*;
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{Attempt, Outcome},
    validation::Validate,
};
use serde_json::json;

fn full() -> Value {
    json!({"input_tokens":10,"output_tokens":4,"total_tokens":14,
        "input_tokens_details":{"cached_tokens":2,"cache_write_tokens":1},
        "output_tokens_details":{"reasoning_tokens":3}})
}

fn attempt(id: &str, usage: Usage) -> Result<Attempt> {
    Ok(Attempt {
        attempt_id: id.try_into()?,
        target: None,
        outcome: Outcome::Failed,
        usage,
        native_usage: Vec::new(),
    })
}

#[test]
fn missing_counters_remain_null_but_explicit_zero_is_provider_evidence() -> Result<()> {
    let mut evidence = Evidence::default();
    evidence.observe(
        Protocol::Responses,
        &json!({"input_tokens":0,"output_tokens":0}),
    );
    let partial = evidence.project();
    ensure!(partial.usage.input_tokens == Some(0) && partial.usage.output_tokens == Some(0));
    ensure!(partial.usage.cache_read_tokens.is_none() && partial.usage.reasoning_tokens.is_none());
    ensure!(partial.usage.provenance == Provenance::Provider && !partial.usage.complete);
    ensure!(partial.native_usage.len() == 2);
    partial.usage.validate()?;
    evidence.observe(
        Protocol::Responses,
        &json!({"input_tokens":0,"output_tokens":0,
        "input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
        "output_tokens_details":{"reasoning_tokens":0}}),
    );
    let complete = evidence.project();
    ensure!(complete.usage.complete && complete.native_usage.len() == 5);
    complete.usage.validate()?;
    Ok(())
}

#[test]
fn existing_canonical_parser_defaults_are_not_provider_presence() -> Result<()> {
    let original = json!({"input_tokens":4,"output_tokens":2,"total_tokens":6});
    let canonical = crate::gateway::model::protocol::responses::parse_usage(&original)
        .ok_or_else(|| anyhow::anyhow!("canonical parser rejected fixture"))?;
    ensure!(canonical.cache_read_tokens == 0 && canonical.reasoning_tokens == 0);
    let mut evidence = Evidence::default();
    evidence.observe(Protocol::Responses, &original);
    let projected = evidence.project();
    ensure!(projected.usage.input_tokens == Some(4) && projected.usage.output_tokens == Some(2));
    ensure!(
        projected.usage.cache_read_tokens.is_none() && projected.usage.reasoning_tokens.is_none()
    );
    ensure!(!projected.usage.complete && projected.native_usage.len() == 2);
    Ok(())
}

#[test]
fn repeated_snapshots_do_not_double_count_and_conflicts_do_not_heal() -> Result<()> {
    let mut evidence = Evidence::default();
    for _ in 0..4 {
        evidence.observe(Protocol::Responses, &full());
    }
    ensure!(evidence.project().usage.input_tokens == Some(10));
    ensure!(evidence.project().usage.complete);
    evidence.observe(Protocol::Responses, &json!({"output_tokens":5}));
    evidence.observe(Protocol::Responses, &full());
    let projection = evidence.project();
    ensure!(projection.usage.output_tokens.is_none());
    ensure!(projection.usage.input_tokens == Some(10) && !projection.usage.complete);
    ensure!(
        !projection
            .native_usage
            .iter()
            .any(|counter| counter.name == UsageDimension::OutputTokens)
    );
    projection.usage.validate()?;
    Ok(())
}

#[test]
fn messages_merge_original_envelopes_without_defaulting_absent_cache_buckets() -> Result<()> {
    let mut evidence = Evidence::default();
    evidence.observe(
        Protocol::Messages,
        &json!({"input_tokens":10,"output_tokens":0}),
    );
    ensure!(evidence.project().usage.input_tokens.is_none());
    ensure!(
        evidence
            .project()
            .native_usage
            .iter()
            .any(|counter| counter.name == UsageDimension::InputTokens && counter.value == 10)
    );
    evidence.observe(
        Protocol::Messages,
        &json!({"cache_read_input_tokens":2,"cache_creation_input_tokens":3}),
    );
    evidence.observe(Protocol::Messages, &json!({"output_tokens":7}));
    evidence.observe(Protocol::Messages, &json!({"output_tokens":7}));
    let projected = evidence.project();
    ensure!(projected.usage.input_tokens == Some(15) && projected.usage.output_tokens == Some(7));
    ensure!(projected.usage.reasoning_tokens.is_none() && !projected.usage.complete);
    ensure!(projected.native_usage.len() == 4);
    evidence.observe(Protocol::Messages, &json!({"output_tokens":6}));
    evidence.observe(Protocol::Messages, &json!({"output_tokens":8}));
    ensure!(evidence.project().usage.output_tokens.is_none());
    Ok(())
}

#[test]
fn unsupported_payload_is_never_retained_or_recursively_projected() -> Result<()> {
    let mut raw = full();
    raw["billing_metadata"] = json!({"secret":"private".repeat(100_000),"arbitrary_tokens":999});
    raw["output_tokens_details"]["audio_tokens"] = 42.into();
    let mut evidence = Evidence::default();
    evidence.observe(Protocol::Responses, &raw);
    drop(raw);
    let projected = evidence.project();
    ensure!(projected.usage.input_tokens == Some(10) && !projected.usage.complete);
    ensure!(projected.native_usage.len() == 5);
    let encoded = serde_json::to_vec(&projected.native_usage)?;
    ensure!(encoded.len() <= 8192);
    let text = std::str::from_utf8(&encoded)?;
    ensure!(!text.contains("private") && !text.contains("audio") && !text.contains("metadata"));
    // Unsupported nested objects are inspected only by key; no recursive walk.
    let mut nested = Value::Null;
    for _ in 0..128 {
        nested = json!({"nested":nested});
    }
    evidence.observe(
        Protocol::Responses,
        &json!({"input_tokens":10,"extension":nested}),
    );
    ensure!(evidence.project().usage.input_tokens == Some(10));
    Ok(())
}

#[test]
fn malformed_range_and_inconsistent_totals_are_conservative() -> Result<()> {
    for invalid in [
        json!(-1),
        json!(1.5),
        json!("7"),
        json!(null),
        json!(AuthorityRevision::MAX + 1),
    ] {
        let mut evidence = Evidence::default();
        evidence.observe(
            Protocol::Responses,
            &json!({"input_tokens":invalid,"output_tokens":2}),
        );
        let projected = evidence.project();
        ensure!(projected.usage.input_tokens.is_none() && projected.usage.output_tokens == Some(2));
        ensure!(!projected.usage.complete);
        projected.usage.validate()?;
    }
    let mut evidence = Evidence::default();
    evidence.observe(
        Protocol::Decisions,
        &json!({"input_tokens":2,"output_tokens":3,"total_tokens":4}),
    );
    let projected = evidence.project();
    ensure!(projected.usage.provenance == Provenance::Provider);
    ensure!(projected.usage.input_tokens.is_none() && projected.usage.output_tokens.is_none());
    ensure!(!projected.usage.complete && projected.native_usage.len() == 2);
    ensure!(projected.native_usage[0].name == UsageDimension::InputTokens);
    ensure!(projected.native_usage[0].value == 2);
    ensure!(projected.native_usage[1].name == UsageDimension::OutputTokens);
    ensure!(projected.native_usage[1].value == 3);
    projected.usage.validate()?;
    let mut overflowing = Evidence::default();
    overflowing.observe(
        Protocol::Messages,
        &json!({"input_tokens":AuthorityRevision::MAX,
        "output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":0}),
    );
    ensure!(overflowing.project().usage.input_tokens.is_none());
    ensure!(overflowing.project().usage.output_tokens == Some(2));
    Ok(())
}

#[test]
fn redundant_total_and_wrappers_do_not_add_dimensions_but_subsets_are_checked() -> Result<()> {
    for protocol in [Protocol::Responses, Protocol::Decisions] {
        let mut evidence = Evidence::default();
        evidence.observe(protocol, &full());
        let projected = evidence.project();
        ensure!(projected.usage.complete && projected.native_usage.len() == 5);
        projected.usage.validate()?;
    }
    let mut chat = Evidence::default();
    chat.observe(
        Protocol::ChatCompletions,
        &json!({"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,
        "prompt_tokens_details":{"cached_tokens":2,"cache_write_tokens":1},
        "completion_tokens_details":{"reasoning_tokens":3}}),
    );
    ensure!(chat.project().usage.complete);
    let mut raw = full();
    raw["input_tokens_details"]["cached_tokens"] = 10.into();
    raw["output_tokens_details"]["reasoning_tokens"] = 5.into();
    let mut invalid = Evidence::default();
    invalid.observe(Protocol::Responses, &raw);
    let projected = invalid.project();
    ensure!(projected.usage.input_tokens.is_none() && projected.usage.output_tokens.is_none());
    ensure!(!projected.usage.complete && projected.native_usage.len() == 5);
    projected.usage.validate()?;
    Ok(())
}

#[test]
fn observation_bound_and_protocol_or_provenance_conflict_cannot_be_erased() -> Result<()> {
    let mut evidence = Evidence::default();
    for _ in 0..MAX_OBSERVATIONS {
        evidence.observe(Protocol::Responses, &full());
    }
    ensure!(evidence.project().usage.complete);
    evidence.observe(Protocol::Responses, &full());
    ensure!(
        !evidence.project().usage.complete && evidence.project().usage.input_tokens == Some(10)
    );
    let mut mixed = Evidence::default();
    mixed.observe(Protocol::Responses, &full());
    mixed.observe(Protocol::Decisions, &full());
    ensure!(mixed.project().usage.provenance == Provenance::Unknown);
    ensure!(mixed.project().native_usage.is_empty());
    let mut estimate = Evidence::estimated([Some(1), Some(1), Some(0), Some(0), Some(0)]);
    ensure!(estimate.project().usage.provenance == Provenance::Estimated);
    ensure!(!estimate.project().usage.complete && estimate.project().native_usage.is_empty());
    estimate.observe(Protocol::Responses, &full());
    ensure!(estimate.project().usage.provenance == Provenance::Unknown);
    Ok(())
}

#[test]
fn aggregate_unknown_missing_estimated_and_safe_integer_overflow() -> Result<()> {
    let mut evidence = Evidence::default();
    evidence.observe(Protocol::Responses, &full());
    let mut attempts = vec![
        attempt("a", evidence.project().usage)?,
        attempt("b", evidence.project().usage)?,
    ];
    let total = aggregate(&attempts);
    ensure!(total.input_tokens == Some(20) && total.complete);
    attempts[1].usage = Evidence::estimated([Some(1), Some(1), Some(0), Some(0), Some(0)])
        .project()
        .usage;
    let total = aggregate(&attempts);
    ensure!(
        total.input_tokens == Some(11)
            && total.provenance == Provenance::Estimated
            && !total.complete
    );
    attempts[1].usage = Evidence::default().project().usage;
    let total = aggregate(&attempts);
    ensure!(
        total.provenance == Provenance::Unknown
            && total.input_tokens.is_none()
            && total.output_tokens.is_none()
    );
    ensure!(
        total.cache_read_tokens.is_none()
            && total.cache_write_tokens.is_none()
            && total.reasoning_tokens.is_none()
    );
    attempts[1].usage = evidence.project().usage;
    attempts[1].usage.output_tokens = None;
    attempts[1].usage.complete = false;
    ensure!(
        aggregate(&attempts).input_tokens == Some(20)
            && aggregate(&attempts).output_tokens.is_none()
    );
    attempts[0].usage.input_tokens = Some(AuthorityRevision::MAX);
    let overflow = aggregate(&attempts);
    ensure!(
        overflow.input_tokens.is_none()
            && overflow.provenance == Provenance::Provider
            && !overflow.complete
    );
    overflow.validate()?;
    ensure!(aggregate(&[]).provenance == Provenance::Unknown);
    let oversized = vec![attempt("aggregate-only", evidence.project().usage)?; 9];
    let rejected_total = aggregate(&oversized);
    ensure!(rejected_total.provenance == Provenance::Unknown);
    ensure!(rejected_total.input_tokens.is_none() && !rejected_total.complete);
    Ok(())
}

fn total_protocols() -> [(Protocol, Value); 3] {
    [
        (Protocol::Responses, full()),
        (Protocol::Decisions, full()),
        (
            Protocol::ChatCompletions,
            json!({"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,
                "prompt_tokens_details":{"cached_tokens":2,"cache_write_tokens":1},
                "completion_tokens_details":{"reasoning_tokens":3}}),
        ),
    ]
}

fn without_total(mut raw: Value) -> Result<Value> {
    let object = raw
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("usage fixture must be an object"))?;
    ensure!(object.remove("total_tokens").is_some());
    Ok(raw)
}

fn assert_total_conflict(projected: &Projection) -> Result<()> {
    ensure!(projected.usage.provenance == Provenance::Provider);
    ensure!(!projected.usage.complete);
    ensure!(projected.usage.input_tokens.is_none() && projected.usage.output_tokens.is_none());
    ensure!(projected.usage.cache_read_tokens == Some(2));
    ensure!(projected.usage.cache_write_tokens == Some(1));
    ensure!(projected.usage.reasoning_tokens == Some(3));
    // The redundant total never joins the whitelist, while the independently
    // observed five raw facts survive a relationship failure unchanged.
    ensure!(
        serde_json::to_value(&projected.native_usage)?
            == json!([
                {"name":"input_tokens","value":10},
                {"name":"output_tokens","value":4},
                {"name":"cache_read_tokens","value":2},
                {"name":"cache_write_tokens","value":1},
                {"name":"reasoning_tokens","value":3},
            ])
    );
    ensure!(serde_json::to_vec(&projected.native_usage)?.len() <= 8192);
    projected.usage.validate()?;
    for counter in &projected.native_usage {
        counter.validate()?;
    }
    Ok(())
}

#[test]
fn contradictory_partial_totals_fail_in_both_observation_orders() -> Result<()> {
    for (protocol, complete) in total_protocols() {
        let mut partial = json!({"total_tokens":999});
        let input = if protocol == Protocol::ChatCompletions {
            "prompt_tokens"
        } else {
            "input_tokens"
        };
        partial[input] = 10.into();
        for observations in [[&partial, &complete], [&complete, &partial]] {
            let mut evidence = Evidence::default();
            for raw in observations {
                evidence.observe(protocol, raw);
            }
            assert_total_conflict(&evidence.project())?;
            // An apparently matching later snapshot cannot erase either order
            // of the earlier contradictory evidence.
            evidence.observe(protocol, &complete);
            assert_total_conflict(&evidence.project())?;
        }
    }
    Ok(())
}

#[test]
fn pending_total_is_checked_when_later_counters_omit_it() -> Result<()> {
    for (protocol, complete) in total_protocols() {
        let mut evidence = Evidence::default();
        evidence.observe(protocol, &json!({"total_tokens":999}));
        let pending = evidence.project();
        ensure!(pending.usage.provenance == Provenance::Unknown);
        ensure!(!pending.usage.complete && pending.native_usage.is_empty());
        evidence.observe(protocol, &without_total(complete.clone())?);
        assert_total_conflict(&evidence.project())?;
        evidence.observe(protocol, &complete);
        assert_total_conflict(&evidence.project())?;
    }
    Ok(())
}

#[test]
fn repeated_total_conflicts_latch_before_either_primary_counter_exists() -> Result<()> {
    for (protocol, complete) in total_protocols() {
        for totals in [[999, 14], [14, 999]] {
            let mut evidence = Evidence::default();
            for total in totals {
                evidence.observe(protocol, &json!({"total_tokens":total}));
            }
            let pending = evidence.project();
            ensure!(pending.usage.provenance == Provenance::Unknown);
            ensure!(!pending.usage.complete && pending.native_usage.is_empty());
            evidence.observe(protocol, &without_total(complete.clone())?);
            assert_total_conflict(&evidence.project())?;
            evidence.observe(protocol, &complete);
            assert_total_conflict(&evidence.project())?;
        }
    }
    Ok(())
}

#[test]
fn consistent_split_usage_grows_only_explicitly_observed_dimensions() -> Result<()> {
    for (protocol, complete) in total_protocols() {
        let (input, output, input_details, output_details) =
            if protocol == Protocol::ChatCompletions {
                (
                    "prompt_tokens",
                    "completion_tokens",
                    "prompt_tokens_details",
                    "completion_tokens_details",
                )
            } else {
                (
                    "input_tokens",
                    "output_tokens",
                    "input_tokens_details",
                    "output_tokens_details",
                )
            };
        let mut evidence = Evidence::default();
        evidence.observe(protocol, &json!({"total_tokens":14}));
        ensure!(evidence.project().native_usage.is_empty());
        evidence.observe(protocol, &json!({input:10}));
        let first = evidence.project();
        ensure!(first.usage.input_tokens == Some(10) && first.usage.output_tokens.is_none());
        ensure!(!first.usage.complete && first.native_usage.len() == 1);
        evidence.observe(protocol, &json!({output:4}));
        let second = evidence.project();
        ensure!(second.usage.input_tokens == Some(10) && second.usage.output_tokens == Some(4));
        ensure!(
            second.usage.cache_read_tokens.is_none() && second.usage.reasoning_tokens.is_none()
        );
        ensure!(!second.usage.complete && second.native_usage.len() == 2);
        evidence.observe(
            protocol,
            &json!({input_details:{"cached_tokens":2,"cache_write_tokens":1}}),
        );
        let third = evidence.project();
        ensure!(third.usage.input_tokens == Some(10) && third.usage.output_tokens == Some(4));
        ensure!(
            third.usage.cache_read_tokens == Some(2) && third.usage.cache_write_tokens == Some(1)
        );
        ensure!(third.usage.reasoning_tokens.is_none());
        ensure!(!third.usage.complete && third.native_usage.len() == 4);
        evidence.observe(protocol, &json!({output_details:{"reasoning_tokens":3}}));
        let final_projection = evidence.project();
        ensure!(final_projection.usage.complete && final_projection.native_usage.len() == 5);
        final_projection.usage.validate()?;
        evidence.observe(protocol, &complete);
        let repeated = evidence.project();
        ensure!(repeated.usage == final_projection.usage);
        ensure!(
            serde_json::to_value(&repeated.native_usage)?
                == serde_json::to_value(&final_projection.native_usage)?
        );
    }
    Ok(())
}

#[test]
fn malformed_total_never_heals_after_matching_provider_completion() -> Result<()> {
    for (protocol, complete) in total_protocols() {
        for invalid in [
            json!(null),
            json!(-1),
            json!(1.5),
            json!("14"),
            json!(AuthorityRevision::MAX + 1),
            json!({"private":"not numeric"}),
            json!([14]),
        ] {
            let partial = json!({"total_tokens":invalid});
            for observations in [[&partial, &complete], [&complete, &partial]] {
                let mut evidence = Evidence::default();
                for raw in observations {
                    evidence.observe(protocol, raw);
                }
                assert_total_conflict(&evidence.project())?;
                evidence.observe(protocol, &complete);
                assert_total_conflict(&evidence.project())?;
            }
        }
    }
    Ok(())
}

#[test]
fn total_relationship_failure_survives_later_primary_counter_conflict() -> Result<()> {
    for (protocol, complete) in total_protocols() {
        let mut bad_total = complete.clone();
        bad_total["total_tokens"] = 999.into();
        let mut evidence = Evidence::default();
        evidence.observe(protocol, &bad_total);
        assert_total_conflict(&evidence.project())?;
        let output = if protocol == Protocol::ChatCompletions {
            "completion_tokens"
        } else {
            "output_tokens"
        };
        evidence.observe(protocol, &json!({output:5}));
        evidence.observe(protocol, &complete);
        let projected = evidence.project();
        // Invalidating the output counter must not hide the earlier total
        // contradiction and make normalized input reappear.
        ensure!(projected.usage.input_tokens.is_none() && projected.usage.output_tokens.is_none());
        ensure!(!projected.usage.complete && projected.usage.provenance == Provenance::Provider);
        ensure!(projected.native_usage.len() == 4);
        ensure!(
            projected
                .native_usage
                .iter()
                .any(|counter| counter.name == UsageDimension::InputTokens && counter.value == 10)
        );
        ensure!(
            !projected
                .native_usage
                .iter()
                .any(|counter| counter.name == UsageDimension::OutputTokens)
        );
        projected.usage.validate()?;
    }
    Ok(())
}

#[test]
fn excessive_total_sum_keeps_only_five_bounded_canonical_raw_dimensions() -> Result<()> {
    let maximum = AuthorityRevision::MAX;
    let mut evidence = Evidence::default();
    evidence.observe(Protocol::Responses, &json!({"total_tokens":maximum}));
    let mut raw = json!({
        "input_tokens":maximum,"output_tokens":maximum,
        "input_tokens_details":{"cached_tokens":maximum,"cache_write_tokens":0},
        "output_tokens_details":{"reasoning_tokens":maximum},
        "secret_metadata":"private".repeat(100_000),
    });
    evidence.observe(Protocol::Responses, &raw);
    raw["secret_metadata"] = Value::Null;
    drop(raw);
    let projected = evidence.project();
    ensure!(projected.usage.provenance == Provenance::Provider && !projected.usage.complete);
    ensure!(projected.usage.input_tokens.is_none() && projected.usage.output_tokens.is_none());
    ensure!(projected.usage.cache_read_tokens == Some(maximum));
    ensure!(projected.usage.cache_write_tokens == Some(0));
    ensure!(projected.usage.reasoning_tokens == Some(maximum));
    ensure!(projected.native_usage.len() == 5);
    ensure!(
        serde_json::to_value(&projected.native_usage)?
            == json!([
                {"name":"input_tokens","value":maximum},
                {"name":"output_tokens","value":maximum},
                {"name":"cache_read_tokens","value":maximum},
                {"name":"cache_write_tokens","value":0},
                {"name":"reasoning_tokens","value":maximum},
            ])
    );
    let bytes = serde_json::to_vec(&projected.native_usage)?;
    ensure!(bytes.len() <= 8192);
    let text = std::str::from_utf8(&bytes)?;
    ensure!(
        !text.contains("private") && !text.contains("metadata") && !text.contains("total_tokens")
    );
    projected.usage.validate()?;
    for counter in &projected.native_usage {
        counter.validate()?;
    }
    Ok(())
}

#[test]
fn aggregation_cannot_fill_totals_withheld_by_one_attempt() -> Result<()> {
    let mut valid = Evidence::default();
    valid.observe(Protocol::Responses, &full());
    let mut invalid = Evidence::default();
    invalid.observe(Protocol::Responses, &json!({"total_tokens":999}));
    invalid.observe(Protocol::Responses, &full());
    assert_total_conflict(&invalid.project())?;
    let attempts = [
        attempt("valid", valid.project().usage)?,
        attempt("invalid", invalid.project().usage)?,
    ];
    let total = aggregate(&attempts);
    ensure!(total.provenance == Provenance::Provider && !total.complete);
    ensure!(total.input_tokens.is_none() && total.output_tokens.is_none());
    ensure!(total.cache_read_tokens == Some(4) && total.cache_write_tokens == Some(2));
    ensure!(total.reasoning_tokens == Some(6));
    total.validate()?;
    Ok(())
}

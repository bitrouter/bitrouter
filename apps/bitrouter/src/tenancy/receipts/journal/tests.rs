use super::*;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Provenance, ReceiptState},
    identity::AuthorityRevision,
};
use serde_json::json;

// Unit-only construction bypasses admission to exercise the pure state machine.
// Production constructors require &TenantExecution and an exact retained lookup.
fn owner(execution: &str) -> Result<Owner> {
    Ok(Owner {
        installation: "installation".try_into()?,
        execution: execution.try_into()?,
    })
}

fn reference(kind: ResourceKind) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant".try_into()?,
        resource_kind: kind,
        resource_id: "slot".try_into()?,
        revision_id: "r1".try_into()?,
    })
}

fn stamp(owner: &Owner, model: &str) -> Result<TargetStamp> {
    let route = reference(ResourceKind::Route)?;
    let credential = reference(ResourceKind::Credential)?;
    Ok(TargetStamp {
        owner: owner.clone(),
        target: project_target(
            &owner.installation,
            &route,
            &credential,
            "provider",
            model,
            None,
        )?,
        route,
        credential,
        candidate: format!("exact-private-projection:{model}")
            .into_bytes()
            .into(),
    })
}

fn known_usage() -> serde_json::Value {
    json!({"input_tokens":10,"output_tokens":4,"total_tokens":14,
        "input_tokens_details":{"cached_tokens":2,"cache_write_tokens":1},
        "output_tokens_details":{"reasoning_tokens":3}})
}

fn receipt(frozen: Frozen, outcome: Outcome) -> Result<ExecutionReceipt> {
    Ok(ExecutionReceipt {
        installation_id: frozen.owner.installation,
        tenant_id: "tenant".try_into()?,
        principal_id: "principal".try_into()?,
        execution_id: frozen.owner.execution,
        correlation_id: "correlation".try_into()?,
        execution_revision: "r1".try_into()?,
        state: ReceiptState::Incomplete,
        outcome,
        attempts: frozen.attempts,
        usage: frozen.usage,
        recorded_at: "2026-10-25T00:00:00Z".try_into()?,
    })
}

#[test]
fn reserved_drop_releases_capacity_without_manufacturing_a_send() -> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    for _ in 0..32 {
        drop(journal.reserve(stamp(&owner, "model")?)?);
    }
    let frozen = journal.freeze()?;
    ensure!(frozen.attempts.is_empty() && frozen.dispatch == DispatchState::NotStarted);
    ensure!(frozen.usage.provenance == Provenance::Unknown);
    receipt(frozen, Outcome::Failed)?.validate()?;
    Ok(())
}

#[test]
fn all_eight_slots_bound_reservations_and_actual_sends_with_unique_ids() -> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "model")?;
    let mut permits = Vec::new();
    for _ in 0..SLOTS {
        permits.push(journal.reserve(stamp.clone())?);
    }
    ensure!(matches!(
        journal.reserve(stamp.clone()),
        Err(Error::Capacity)
    ));
    let mut ids = std::collections::HashSet::new();
    for permit in &mut permits {
        ensure!(ids.insert(permit.arm(&stamp)?));
    }
    drop(permits);
    ensure!(
        matches!(journal.reserve(stamp), Err(Error::Capacity)),
        "armed slots cannot be recycled"
    );
    let frozen = journal.freeze()?;
    ensure!(frozen.attempts.len() == SLOTS && frozen.dispatch == DispatchState::Unknown);
    ensure!(
        frozen
            .attempts
            .iter()
            .all(|attempt| attempt.outcome == Outcome::Unknown)
    );
    receipt(frozen, Outcome::Unknown)?.validate()?;
    Ok(())
}

#[test]
fn duplicate_generated_identity_is_rejected_without_overwriting_the_original_attempt() -> Result<()>
{
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "model")?;
    let id: AttemptId = "injected-duplicate".try_into()?;
    let mut original = journal.reserve_identity(stamp.clone(), id.clone())?;
    original.arm(&stamp)?;
    original.http_status(403)?;
    drop(original);
    ensure!(matches!(
        journal.reserve_identity(stamp, id.clone()),
        Err(Error::Conflict)
    ));
    let frozen = journal.freeze()?;
    ensure!(frozen.attempts.len() == 1 && frozen.attempts[0].attempt_id == id);
    ensure!(frozen.attempts[0].outcome == Outcome::Failed);
    Ok(())
}

#[test]
fn armed_drop_is_synchronous_unknown_without_a_runtime() -> Result<()> {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<AttemptPermit>();
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "model")?;
    let mut permit = journal.reserve(stamp.clone())?;
    let id = permit.arm(&stamp)?;
    ensure!(matches!(permit.arm(&stamp), Err(Error::Phase)));
    drop(permit);
    let frozen = journal.freeze()?;
    let attempt = frozen.attempts.first().context("attempt")?;
    ensure!(attempt.attempt_id == id && attempt.outcome == Outcome::Unknown);
    ensure!(attempt.target.as_ref().context("actual target")?.model_id == "model");
    ensure!(frozen.http_statuses == vec![None] && frozen.dispatch == DispatchState::Unknown);
    Ok(())
}

#[tokio::test]
async fn cancelling_an_owned_future_drops_its_permit_at_the_actual_phase() -> Result<()> {
    for arm in [false, true] {
        let owner = owner("execution")?;
        let journal = Journal::owned(owner.clone());
        let stamp = stamp(&owner, "model")?;
        let mut permit = journal.reserve(stamp.clone())?;
        let (entered, wait) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if arm {
                permit.arm(&stamp)?;
            }
            if entered.send(()).is_err() {
                return Err(Error::Closed);
            }
            std::future::pending::<()>().await;
            drop(permit);
            Ok::<_, Error>(())
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), wait).await??;
        task.abort();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(2), task).await?;
        ensure!(joined.is_err_and(|error| error.is_cancelled()));
        let frozen = journal.freeze()?;
        ensure!(frozen.attempts.len() == usize::from(arm));
        ensure!(
            frozen.dispatch
                == if arm {
                    DispatchState::Unknown
                } else {
                    DispatchState::NotStarted
                }
        );
    }
    Ok(())
}

#[test]
fn observed_rejection_survives_dropped_truncated_or_stalled_diagnostics() -> Result<()> {
    for status in [301, 400, 401, 403, 429, 503] {
        let owner = owner(&format!("execution-{status}"))?;
        let journal = Journal::owned(owner.clone());
        let stamp = stamp(&owner, "rejected")?;
        let mut permit = journal.reserve(stamp.clone())?;
        permit.arm(&stamp)?;
        permit.http_status(status)?;
        ensure!(matches!(journal.fallback_evidence(), Err(Error::Busy)));
        // The body read may now error or be cancelled; no terminal decoder ran.
        drop(permit);
        let fact = journal.fallback_evidence()?.context("settled rejection")?;
        ensure!(fact.outcome == Outcome::Failed && fact.http_status == Some(status));
        let frozen = journal.freeze()?;
        ensure!(frozen.attempts[0].outcome == Outcome::Failed);
        ensure!(
            frozen.dispatch == DispatchState::Started && frozen.http_statuses == vec![Some(status)]
        );
        ensure!(frozen.usage.provenance == Provenance::Unknown);
        receipt(frozen, Outcome::Failed)?.validate()?;
    }
    Ok(())
}

#[test]
fn pre_send_observations_are_rejected_and_conflicting_status_cannot_restore_known_outcome()
-> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "actual")?;
    let mut permit = journal.reserve(stamp.clone())?;
    ensure!(matches!(permit.http_status(200), Err(Error::Phase)));
    ensure!(matches!(
        permit.validated_completion(true),
        Err(Error::Phase)
    ));
    ensure!(matches!(
        permit.observe_usage(Protocol::Responses, &known_usage()),
        Err(Error::Phase)
    ));
    permit.arm(&stamp)?;
    ensure!(matches!(permit.http_status(199), Err(Error::Phase)));
    ensure!(matches!(permit.http_status(600), Err(Error::Phase)));
    permit.http_status(403)?;
    permit.http_status(403)?;
    ensure!(matches!(permit.http_status(200), Err(Error::Conflict)));
    permit.http_status(403)?;
    ensure!(matches!(
        permit.validated_completion(false),
        Err(Error::Conflict)
    ));
    drop(permit);
    let fact = journal
        .fallback_evidence()?
        .context("conflicting attempt")?;
    ensure!(fact.outcome == Outcome::Unknown && fact.http_status == Some(403));
    let frozen = journal.freeze()?;
    ensure!(frozen.attempts[0].outcome == Outcome::Unknown);
    ensure!(frozen.dispatch == DispatchState::Unknown && !frozen.usage.complete);
    Ok(())
}

#[test]
fn success_headers_are_not_completion_and_validated_work_survives_local_failure() -> Result<()> {
    for completed in [false, true] {
        let owner = owner("execution")?;
        let journal = Journal::owned(owner.clone());
        let stamp = stamp(&owner, "actual")?;
        let mut permit = journal.reserve(stamp.clone())?;
        permit.arm(&stamp)?;
        permit.http_status(200)?;
        permit.observe_usage(Protocol::Decisions, &known_usage())?;
        if completed {
            permit.validated_completion(true)?;
        }
        // Rendering/disconnect errors do not call validated_completion(false).
        drop(permit);
        let frozen = journal.freeze()?;
        ensure!(
            frozen.attempts[0].outcome
                == if completed {
                    Outcome::Completed
                } else {
                    Outcome::Unknown
                }
        );
        ensure!(frozen.usage.input_tokens == Some(10) && frozen.usage.complete == completed);
        receipt(frozen, Outcome::Failed)?.validate()?;
    }
    Ok(())
}

#[test]
fn validated_failure_retains_partial_usage_and_conflicting_provider_terminals_fail_closed()
-> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "actual")?;
    let mut failed = journal.reserve(stamp.clone())?;
    failed.arm(&stamp)?;
    failed.http_status(200)?;
    failed.observe_usage(
        Protocol::Responses,
        &json!({"input_tokens":4,"output_tokens":2}),
    )?;
    failed.validated_completion(false)?;
    failed.incomplete_usage()?;
    drop(failed);
    let mut conflict = journal.reserve(stamp.clone())?;
    conflict.arm(&stamp)?;
    conflict.http_status(200)?;
    conflict.validated_completion(true)?;
    ensure!(matches!(
        conflict.validated_completion(false),
        Err(Error::Conflict)
    ));
    ensure!(matches!(
        conflict.validated_completion(true),
        Err(Error::Conflict)
    ));
    drop(conflict);
    let frozen = journal.freeze()?;
    ensure!(
        frozen.attempts[0].outcome == Outcome::Failed
            && frozen.attempts[0].usage.input_tokens == Some(4)
    );
    ensure!(frozen.attempts[1].outcome == Outcome::Unknown);
    ensure!(frozen.usage.provenance == Provenance::Unknown && frozen.usage.input_tokens.is_none());
    receipt(frozen, Outcome::Failed)?.validate()?;
    Ok(())
}

#[test]
fn freeze_waits_for_producers_and_prevents_late_reserved_send() -> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "actual")?;
    let mut reserved = journal.reserve(stamp.clone())?;
    let mut armed = journal.reserve(stamp.clone())?;
    armed.arm(&stamp)?;
    ensure!(matches!(journal.freeze(), Err(Error::Busy)));
    ensure!(matches!(reserved.arm(&stamp), Err(Error::Closed)));
    ensure!(matches!(journal.reserve(stamp), Err(Error::Closed)));
    // Closing future sends must not suppress already-started evidence.
    armed.http_status(200)?;
    armed.validated_completion(true)?;
    ensure!(matches!(journal.freeze(), Err(Error::Busy)));
    drop((reserved, armed));
    let frozen = journal.freeze()?;
    ensure!(frozen.attempts.len() == 1 && frozen.attempts[0].outcome == Outcome::Completed);
    ensure!(
        serde_json::to_value(&frozen.attempts)?
            == serde_json::to_value(journal.freeze()?.attempts)?
    );
    Ok(())
}

#[test]
fn exact_binding_rejects_foreign_owner_revision_and_private_header_changes_before_arming()
-> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "actual")?;
    let mut foreign = stamp.clone();
    foreign.owner.execution = "foreign".try_into()?;
    ensure!(matches!(journal.reserve(foreign), Err(Error::Binding)));
    let mut permit = journal.reserve(stamp.clone())?;
    let mut changed = stamp.clone();
    changed.credential.revision_id = "rotated-provider-key".try_into()?;
    ensure!(matches!(permit.arm(&changed), Err(Error::Binding)));
    changed = stamp.clone();
    changed.route.revision_id = "different-route".try_into()?;
    ensure!(matches!(permit.arm(&changed), Err(Error::Binding)));
    changed = stamp.clone();
    changed.candidate = Arc::from(&b"changed-private-fixed-header"[..]);
    ensure!(changed.target.account_id == stamp.target.account_id);
    ensure!(matches!(permit.arm(&changed), Err(Error::Binding)));
    drop(permit);
    ensure!(journal.freeze()?.attempts.is_empty());
    Ok(())
}

#[test]
fn arm_order_and_concurrent_drop_preserve_every_actual_attempt() -> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let first_stamp = stamp(&owner, "reserved-first")?;
    let second_stamp = stamp(&owner, "sent-first")?;
    let mut first = journal.reserve(first_stamp.clone())?;
    let mut second = journal.reserve(second_stamp.clone())?;
    second.arm(&second_stamp)?;
    first.arm(&first_stamp)?;
    let a = std::thread::spawn(move || -> Result<()> {
        first.http_status(403)?;
        Ok(())
    });
    let b = std::thread::spawn(move || -> Result<()> {
        second.http_status(200)?;
        second.validated_completion(true)?;
        Ok(())
    });
    a.join()
        .map_err(|_| anyhow::anyhow!("first worker failed"))??;
    b.join()
        .map_err(|_| anyhow::anyhow!("second worker failed"))??;
    let frozen = journal.freeze()?;
    ensure!(frozen.attempts.len() == 2);
    ensure!(
        frozen.attempts[0]
            .target
            .as_ref()
            .context("first target")?
            .model_id
            == "sent-first"
    );
    ensure!(
        frozen.attempts[0].outcome == Outcome::Completed
            && frozen.attempts[1].outcome == Outcome::Failed
    );
    ensure!(frozen.http_statuses == vec![Some(200), Some(403)]);
    Ok(())
}

#[test]
fn later_known_rejection_never_hides_earlier_uncertain_or_completed_work() -> Result<()> {
    for completed in [false, true] {
        let owner = owner("execution")?;
        let journal = Journal::owned(owner.clone());
        let stamp = stamp(&owner, "actual")?;
        let mut earlier = journal.reserve(stamp.clone())?;
        earlier.arm(&stamp)?;
        let mut later = journal.reserve(stamp.clone())?;
        later.arm(&stamp)?;
        later.http_status(503)?;
        drop(later);
        ensure!(matches!(journal.fallback_evidence(), Err(Error::Busy)));
        if completed {
            earlier.http_status(200)?;
            earlier.validated_completion(true)?;
        }
        drop(earlier);
        let evidence = journal.fallback_evidence()?.context("settled attempts")?;
        ensure!(
            evidence.outcome
                == if completed {
                    Outcome::Completed
                } else {
                    Outcome::Unknown
                }
        );
        ensure!(evidence.http_status == if completed { Some(200) } else { None });
        let frozen = journal.freeze()?;
        ensure!(frozen.attempts.len() == 2 && frozen.http_statuses[1] == Some(503));
        ensure!(frozen.attempts[1].outcome == Outcome::Failed);
    }
    Ok(())
}

#[test]
fn opaque_slot_ids_preserve_versions_and_separate_all_owner_namespaces() -> Result<()> {
    let installation = "installation".try_into()?;
    let mut route = reference(ResourceKind::Route)?;
    let mut credential = reference(ResourceKind::Credential)?;
    let default = project_target(
        &installation,
        &route,
        &credential,
        "provider",
        "model",
        None,
    )?;
    let named_default = project_target(
        &installation,
        &route,
        &credential,
        "provider",
        "model",
        Some("default"),
    )?;
    ensure!(default.account_id == named_default.account_id);
    let private = project_target(
        &installation,
        &route,
        &credential,
        "provider",
        "model",
        Some("private account / 租户"),
    )?;
    ensure!(
        private.account_id.as_str().starts_with("acct1-")
            && private.credential_id.as_str().starts_with("cred1-")
    );
    ensure!(!serde_json::to_string(&private)?.contains("private account"));
    let other_installation = project_target(
        &"other-installation".try_into()?,
        &route,
        &credential,
        "provider",
        "model",
        None,
    )?;
    ensure!(
        other_installation.account_id != default.account_id
            && other_installation.credential_id != default.credential_id
    );
    route.revision_id = "r2".try_into()?;
    credential.revision_id = "r3".try_into()?;
    let rotated = project_target(
        &installation,
        &route,
        &credential,
        "provider",
        "model",
        None,
    )?;
    ensure!(
        rotated.account_id == default.account_id && rotated.credential_id == default.credential_id
    );
    ensure!(rotated.credential_version.as_str() == "r3");
    credential.owner_kind = OwnerKind::OperatorPool;
    let pooled = project_target(
        &installation,
        &route,
        &credential,
        "provider",
        "model",
        None,
    )?;
    ensure!(
        pooled.credential_id != default.credential_id && pooled.account_id == default.account_id
    );
    route.owner_kind = OwnerKind::OperatorPool;
    let foreign = project_target(
        &installation,
        &route,
        &credential,
        "provider",
        "model",
        None,
    )?;
    ensure!(foreign.account_id != default.account_id);
    ensure!(
        project_target(
            &installation,
            &route,
            &credential,
            "invalid provider/id",
            "model",
            None
        )
        .is_err()
    );
    ensure!(
        project_target(
            &installation,
            &route,
            &credential,
            "provider",
            &"m".repeat(257),
            None
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn bounded_projection_satisfies_frozen_receipt_validation_even_when_sum_overflows() -> Result<()> {
    let owner = owner("execution")?;
    let journal = Journal::owned(owner.clone());
    let stamp = stamp(&owner, "model")?;
    for _ in 0..SLOTS {
        let mut permit = journal.reserve(stamp.clone())?;
        permit.arm(&stamp)?;
        permit.http_status(200)?;
        permit.observe_usage(
            Protocol::Responses,
            &json!({"input_tokens":AuthorityRevision::MAX,
            "output_tokens":1,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":0}}),
        )?;
        permit.validated_completion(true)?;
    }
    let frozen = journal.freeze()?;
    ensure!(frozen.usage.input_tokens.is_none() && frozen.usage.output_tokens == Some(8));
    ensure!(frozen.usage.provenance == Provenance::Provider && !frozen.usage.complete);
    for attempt in &frozen.attempts {
        ensure!(
            attempt.native_usage.len() <= 5
                && serde_json::to_vec(&attempt.native_usage)?.len() <= 8192
        );
    }
    let receipt = receipt(frozen, Outcome::Completed)?;
    receipt.validate()?;
    ensure!(serde_json::to_vec(&receipt)?.len() <= 1024 * 1024);
    Ok(())
}

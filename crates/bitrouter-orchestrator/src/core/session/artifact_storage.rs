//! Retained artifact bodies and outstanding tool-evidence allowances share one
//! quota. Core-generated replacement roots coexist with the acknowledged root
//! until the checkpoint commits; the host owns safe reclamation thereafter.

use super::*;

pub(super) fn check(
    state: &SessionSnapshot,
    prepared: &SessionSnapshot,
    host: &Limits,
) -> Result<(), CoreError> {
    let calls = || {
        state
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.invocations)
    };
    let references = recovery::artifact_map(
        recovery::artifacts(state)?
            .into_values()
            .chain(prepared.recovery_archive.iter().cloned())
            // Hydrated recovery evidence must count even when wire inventory
            // contains only its archive root and omits transitive dependencies.
            .chain(
                calls()
                    .flat_map(|call| {
                        call.recovery_observation
                            .iter()
                            .chain(&call.prior_recovery_observations)
                    })
                    .flat_map(|observation| observation.evidence.iter().cloned()),
            )
            .chain(
                state
                    .signals
                    .materials
                    .values()
                    .filter_map(|material| material.artifact.clone()),
            ),
    )?;
    let roots = recovery::artifact_map(
        state
            .recovery_archive
            .iter()
            .cloned()
            .chain(prepared.recovery_archive.iter().cloned()),
    )?;
    // Keep space for the current archive representation even before wire
    // compaction is needed, and for its replacement to coexist before ACK.
    // First essential restoration evidence consumes its frozen reservation;
    // optional/repeated evidence still needs fresh admission. Physical host
    // leases and historical-checkpoint retention are separate obligations.
    let archive = archive::prepare(state, true, host)?.state;
    let mut archive_bytes = archive
        .recovery_archive
        .as_ref()
        .map_or(0, |root| root.bytes);
    let mut tool_evidence_bytes = 0;
    for agent in state.agents.values() {
        let Some(turn) = &agent.turn else { continue };
        for call in &turn.invocations {
            let limits =
                tool_payloads::limits(call, host, state.run.as_ref(), turn.input.limits.as_ref())?;
            if let Some(allowance) = call.recovery_archive_allowance {
                let growth = allowance
                    .checked_mul(archive::remaining_observation_slots(call))
                    .ok_or_else(exhausted)?;
                archive_bytes = add(archive_bytes, growth)?;
            }
            let reserved = limits
                .artifact_bytes
                .unwrap_or(0)
                .checked_mul(tool_payloads::remaining_artifact_slots(call))
                .ok_or_else(exhausted)?;
            tool_evidence_bytes = add(tool_evidence_bytes, reserved)?;
        }
    }
    if archive_bytes > host.unacknowledged_bytes {
        return Err(exhausted());
    }
    let mut bytes = archive_bytes.checked_mul(2).ok_or_else(exhausted)?;
    let mut retained_roots = 0u64;
    for reference in roots.values() {
        retained_roots = add(retained_roots, reference.bytes)?;
    }
    bytes = bytes.max(retained_roots);
    for reference in references.values() {
        if !roots.contains_key(&reference.artifact_id) {
            bytes = add(bytes, reference.bytes)?;
        }
    }
    bytes = add(bytes, tool_evidence_bytes)?;
    if bytes > state.manifest.artifact_quota_bytes {
        return Err(exhausted());
    }
    Ok(())
}

fn add(left: u64, right: u64) -> Result<u64, CoreError> {
    left.checked_add(right).ok_or_else(exhausted)
}

fn exhausted() -> CoreError {
    reject(
        ErrorCode::LimitExceeded,
        "artifact quota cannot retain current objects and admitted tool evidence",
    )
}

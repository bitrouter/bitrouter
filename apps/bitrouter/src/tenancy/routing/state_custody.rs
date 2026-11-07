//! Numeric observations of real routing admissions, used by the tenant HTTP
//! schedules. Only the already-open circuit's time bound can be advanced.
use super::*;
use crate::{gateway::pipeline::types::RoutingTarget, tenancy::execution::TenantExecution};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) active: usize,
    pub(crate) target_active: usize,
    pub(crate) pending: usize,
    pub(crate) sent: u64,
    pub(crate) last_admission: u64,
    pub(crate) failures: u32,
    pub(crate) generation: u64,
    pub(crate) probe: Option<u64>,
    pub(crate) open: bool,
    pub(crate) samples: Vec<Option<u64>>,
}

pub(crate) fn key(
    execution: &TenantExecution,
    target: &RoutingTarget,
) -> crate::gateway::error::Result<Key> {
    let credential = execution
        .route()
        .match_candidate(target)
        .map_err(|code| execution.reject(code))?;
    super::super::key(execution.installation_id(), &credential, target)
}

pub(crate) fn snapshot(runtime: &Runtime, key: &Key) -> Result<Option<Snapshot>, Refusal> {
    let state = runtime.state.lock().map_err(|_| Refusal::Unavailable)?;
    Ok(state.targets.get(key).map(|entry| Snapshot {
        active: state.active,
        target_active: entry.rate.active,
        pending: entry.rate.pending,
        sent: entry.rate.seconds.iter().map(|(_, count)| count).sum(),
        last_admission: state.next,
        failures: entry.failures,
        generation: entry.generation,
        probe: entry.probe,
        open: entry.open_until.is_some(),
        samples: entry.samples.iter().map(|sample| sample.output).collect(),
    }))
}

/// The failures and open generation must come from actual HTTP responses.
/// Expire only the 30s open interval; rates and probe identity are untouched.
pub(crate) fn elapse_open_period(runtime: &Runtime, key: &Key) -> Result<(), Refusal> {
    let mut state = runtime.state.lock().map_err(|_| Refusal::Unavailable)?;
    let entry = state.targets.get_mut(key).ok_or(Refusal::Unavailable)?;
    if entry.open_until.is_none() || entry.probe.is_some() || entry.failures < FAILURES {
        return Err(Refusal::Unavailable);
    }
    entry.open_until = Some(Instant::now());
    Ok(())
}

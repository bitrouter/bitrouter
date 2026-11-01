//! Bounded in-memory attempt facts. This is neither send authorization nor durability.
//!
//! Integration must reserve before effectful work, arm at the existing final
//! send gate, and retain the permit through protocol validation/stream finish.
//! Dropping a permit records unresolved armed work synchronously, without tasks.

use std::sync::{Arc, Mutex, MutexGuard};

use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution::{Attempt, Outcome, Target, Usage},
    identity::{AttemptId, ExecutionId, InstallationId},
    operation::DispatchState,
    validation::Validate,
};
use sha2::{Digest, Sha256};

use super::evidence::{self, Evidence, Protocol};
use crate::{gateway::pipeline::types::RoutingTarget, tenancy::execution::TenantExecution};

#[cfg(test)]
mod tests;

const SLOTS: usize = 8;
const MAX_CANDIDATE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("receipt attempt capacity exhausted")]
    Capacity,
    #[error("receipt journal is closed")]
    Closed,
    #[error("receipt producers remain live")]
    Busy,
    #[error("receipt owner or actual target mismatch")]
    Binding,
    #[error("receipt target is not representable")]
    Target,
    #[error("receipt attempt phase conflict")]
    Phase,
    #[error("receipt provider observations conflict")]
    Conflict,
    #[error("receipt journal unavailable")]
    Poisoned,
}

#[derive(Clone, PartialEq, Eq)]
struct Owner {
    installation: InstallationId,
    execution: ExecutionId,
}

impl Owner {
    fn execution(execution: &TenantExecution) -> Self {
        Self {
            installation: execution.installation_id().clone(),
            execution: execution.id().clone(),
        }
    }
}

/// Created only from an admitted execution and exact retained candidate lookup.
/// Not a public Target DTO, caller context, grant, or provider-verified account.
/// Exact private bytes live only through the permit/auth handoff, never in facts.
#[derive(Clone)]
pub(crate) struct TargetStamp {
    owner: Owner,
    route: ResourceRef,
    credential: ResourceRef,
    candidate: Arc<[u8]>,
    target: Target,
}

impl TargetStamp {
    /// Prepare before authentication. Current authority and authenticated-request
    /// identity still have to be checked by the real send gate after every await.
    pub(crate) fn for_execution(
        execution: &TenantExecution,
        target: &RoutingTarget,
    ) -> Result<Self, Error> {
        let retained = execution.route();
        let credential = retained
            .match_candidate(target)
            .map_err(|_| Error::Binding)?;
        let candidate = retained.candidate_key(target).map_err(|_| Error::Binding)?;
        if candidate.len() > MAX_CANDIDATE_BYTES {
            return Err(Error::Target);
        }
        let owner = Owner::execution(execution);
        let route = retained.reference().clone();
        let projected = project_target(
            &owner.installation,
            &route,
            &credential,
            &target.provider_name,
            &target.service_id,
            target.account_label.as_deref(),
        )?;
        Ok(Self {
            owner,
            route,
            credential,
            candidate: candidate.into(),
            target: projected,
        })
    }

    pub(crate) fn candidate(&self) -> &[u8] {
        &self.candidate
    }
    pub(crate) fn credential(&self) -> &ResourceRef {
        &self.credential
    }
    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.route == other.route
            && self.credential == other.credential
            && self.candidate == other.candidate
    }
}

/// Pure mapping over private exact retained references. It supplies opaque
/// configured-slot IDs, not new authentication or provider account assertions.
pub(in crate::tenancy) fn project_target(
    installation: &InstallationId,
    route: &ResourceRef,
    credential: &ResourceRef,
    provider: &str,
    model: &str,
    account: Option<&str>,
) -> Result<Target, Error> {
    if route.resource_kind != ResourceKind::Route
        || credential.resource_kind != ResourceKind::Credential
    {
        return Err(Error::Target);
    }
    let account = account.unwrap_or("default");
    if account.is_empty() || account.len() > 1024 * 1024 {
        return Err(Error::Target);
    }
    let account_id = opaque(
        "acct1",
        b"bitrouter.receipt.configured-account.v1",
        &[
            installation.as_str(),
            owner_name(route.owner_kind),
            route.owner_id.as_str(),
            "route",
            route.resource_id.as_str(),
            provider,
            account,
        ],
    );
    let credential_id = opaque(
        "cred1",
        b"bitrouter.receipt.credential-slot.v1",
        &[
            installation.as_str(),
            owner_name(credential.owner_kind),
            credential.owner_id.as_str(),
            "credential",
            credential.resource_id.as_str(),
        ],
    );
    let target = Target {
        provider_id: provider.try_into().map_err(|_| Error::Target)?,
        model_id: model.to_owned(),
        account_id: account_id.try_into().map_err(|_| Error::Target)?,
        credential_id: credential_id.try_into().map_err(|_| Error::Target)?,
        credential_version: credential.revision_id.clone(),
    };
    target.validate().map_err(|_| Error::Target)?;
    Ok(target)
}

fn owner_name(kind: OwnerKind) -> &'static str {
    match kind {
        OwnerKind::Tenant => "tenant",
        OwnerKind::OperatorPool => "operator_pool",
    }
}

fn opaque(prefix: &str, domain: &[u8], parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain);
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{prefix}-{}", hex::encode(hash.finalize()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Reserved,
    Armed,
    Completed,
    Failed,
    Unknown,
}

struct Entry {
    id: AttemptId,
    target: Target,
    order: u8,
    phase: Phase,
    status: Option<u16>,
    evidence: Evidence,
    terminal_observed: bool,
    live: bool,
}

struct State {
    slots: [Option<Entry>; SLOTS],
    armed: u8,
    closed: bool,
    usage_evidence: super::usage_evidence::Snapshot,
}

/// Contains no Service/transport handle and cannot cause provider work.
pub(crate) struct Journal {
    owner: Owner,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

/// Consumed by the future receipt owner only after producers have relinquished
/// their permits. The DTO outcome of the invocation is a separate decision.
pub(crate) struct Frozen {
    owner: Owner,
    pub(crate) attempts: Vec<Attempt>,
    pub(crate) usage: Usage,
    pub(crate) dispatch: DispatchState,
    /// Same order as attempts; status is classification evidence, not diagnostic text.
    pub(crate) http_statuses: Vec<Option<u16>>,
    pub(crate) usage_evidence: super::usage_evidence::Snapshot,
}

/// Evidence for the existing fallback classifier, never permission to retry.
pub(crate) struct SettledAttempt {
    pub(crate) outcome: Outcome,
    pub(crate) http_status: Option<u16>,
}

impl Frozen {
    pub(crate) fn belongs_to_identity(
        &self,
        installation: &InstallationId,
        execution: &ExecutionId,
    ) -> bool {
        &self.owner.installation == installation && &self.owner.execution == execution
    }
}

impl Journal {
    /// One journal per admission must be retained by the execution's future
    /// receipt owner. Public headers/CallerContext cannot construct this owner.
    pub(crate) fn new(execution: &TenantExecution) -> Arc<Self> {
        Self::owned(Owner::execution(execution))
    }

    fn owned(owner: Owner) -> Arc<Self> {
        Arc::new(Self {
            owner,
            changed: tokio::sync::Notify::new(),
            state: Mutex::new(State {
                slots: std::array::from_fn(|_| None),
                armed: 0,
                closed: false,
                usage_evidence: super::usage_evidence::Snapshot::default(),
            }),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state.lock().map_err(|_| Error::Poisoned)
    }

    /// A reservation is not a send. Its drop frees its slot and proves no work
    /// only because arming must precede the first poll of the actual send future.
    pub(crate) fn reserve(self: &Arc<Self>, stamp: TargetStamp) -> Result<AttemptPermit, Error> {
        if self.owner != stamp.owner {
            return Err(Error::Binding);
        }
        let id = uuid::Uuid::new_v4()
            .to_string()
            .try_into()
            .map_err(|_| Error::Target)?;
        self.reserve_identity(stamp, id)
    }

    fn reserve_identity(
        self: &Arc<Self>,
        stamp: TargetStamp,
        id: AttemptId,
    ) -> Result<AttemptPermit, Error> {
        if self.owner != stamp.owner {
            return Err(Error::Binding);
        }
        let mut state = self.lock()?;
        if state.closed {
            return Err(Error::Closed);
        }
        if state.slots.iter().flatten().any(|entry| entry.id == id) {
            return Err(Error::Conflict);
        }
        let index = state
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Capacity)?;
        state.slots[index] = Some(Entry {
            id,
            target: stamp.target.clone(),
            order: 0,
            phase: Phase::Reserved,
            status: None,
            evidence: Evidence::default(),
            terminal_observed: false,
            live: true,
        });
        Ok(AttemptPermit {
            journal: self.clone(),
            index,
            stamp,
        })
    }

    /// Inspect only after all transport custody is relinquished. Any unknown
    /// attempt, then any completed attempt, dominates the latest rejection so
    /// concurrent producers cannot conceal uncertain or completed work. These
    /// facts never authorize retry; known rejection still requires policy/grants.
    pub(crate) fn fallback_evidence(&self) -> Result<Option<SettledAttempt>, Error> {
        let state = self.lock()?;
        if state.slots.iter().flatten().any(|entry| entry.live) {
            return Err(Error::Busy);
        }
        let entries = || state.slots.iter().flatten();
        entries()
            .find(|entry| entry.phase == Phase::Unknown)
            .or_else(|| entries().find(|entry| entry.phase == Phase::Completed))
            .or_else(|| entries().max_by_key(|entry| entry.order))
            .map(|entry| {
                Ok(SettledAttempt {
                    outcome: outcome(entry.phase)?,
                    http_status: entry.status,
                })
            })
            .transpose()
    }

    pub(crate) async fn settled(&self) -> Result<Frozen, Error> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.freeze() {
                Err(Error::Busy) => notified.await,
                result => return result,
            }
        }
    }

    pub(crate) fn dispatch(&self) -> Result<DispatchState, Error> {
        let state = self.lock()?;
        let mut result = DispatchState::NotStarted;
        for entry in state
            .slots
            .iter()
            .flatten()
            .filter(|entry| entry.phase != Phase::Reserved)
        {
            if matches!(entry.phase, Phase::Armed | Phase::Unknown) {
                return Ok(DispatchState::Unknown);
            }
            result = DispatchState::Started;
        }
        Ok(result)
    }

    /// Close admission first, then obtain facts only after every permit is dropped.
    /// Busy does not cancel a producer or manufacture an Unknown terminal.
    pub(crate) fn freeze(&self) -> Result<Frozen, Error> {
        let mut state = self.lock()?;
        state.closed = true;
        if state.slots.iter().flatten().any(|entry| entry.live) {
            return Err(Error::Busy);
        }
        let mut entries: Vec<_> = state.slots.iter().flatten().collect();
        entries.sort_by_key(|entry| entry.order);
        let mut attempts = Vec::with_capacity(entries.len());
        let mut statuses = Vec::with_capacity(entries.len());
        let mut dispatch = DispatchState::NotStarted;
        for entry in entries {
            let outcome = outcome(entry.phase)?;
            dispatch = if outcome == Outcome::Unknown || dispatch == DispatchState::Unknown {
                DispatchState::Unknown
            } else {
                DispatchState::Started
            };
            let evidence = entry.evidence.project();
            attempts.push(Attempt {
                attempt_id: entry.id.clone(),
                target: Some(entry.target.clone()),
                outcome,
                usage: evidence.usage,
                native_usage: evidence.native_usage,
            });
            statuses.push(entry.status);
        }
        let usage = evidence::aggregate(&attempts);
        let mut usage_evidence = state.usage_evidence.clone();
        usage_evidence.terminals = state
            .slots
            .iter()
            .flatten()
            .filter(|entry| entry.terminal_observed)
            .map(|entry| entry.id.clone())
            .collect();
        Ok(Frozen {
            owner: self.owner.clone(),
            attempts,
            usage,
            dispatch,
            http_statuses: statuses,
            usage_evidence,
        })
    }
}

fn outcome(phase: Phase) -> Result<Outcome, Error> {
    match phase {
        Phase::Completed => Ok(Outcome::Completed),
        Phase::Failed => Ok(Outcome::Failed),
        Phase::Unknown => Ok(Outcome::Unknown),
        Phase::Reserved | Phase::Armed => Err(Error::Phase),
    }
}

/// Non-Clone, Send+Sync custody of exactly one slot. No writable fields or public constructor.
pub(crate) struct AttemptPermit {
    journal: Arc<Journal>,
    index: usize,
    stamp: TargetStamp,
}

impl AttemptPermit {
    pub(crate) fn stamp(&self) -> &TargetStamp {
        &self.stamp
    }

    /// Call inside the final authority/send linearization, with the stamp carried
    /// by the exact authenticated request. This comparison is not a grant check.
    /// Arming precedes polling send; even connect failure thereafter is unknown.
    pub(crate) fn arm(&mut self, authenticated: &TargetStamp) -> Result<AttemptId, Error> {
        if !self.stamp.matches(authenticated) {
            return Err(Error::Binding);
        }
        let mut state = self.journal.lock()?;
        if state.closed {
            return Err(Error::Closed);
        }
        let order = state.armed;
        if usize::from(order) >= SLOTS {
            return Err(Error::Capacity);
        }
        let entry = state.slots[self.index].as_mut().ok_or(Error::Phase)?;
        if entry.phase != Phase::Reserved {
            return Err(Error::Phase);
        }
        entry.phase = Phase::Armed;
        entry.order = order;
        let id = entry.id.clone();
        state.armed += 1;
        Ok(id)
    }

    /// Record immediately at headers, before any diagnostic-body read. A known
    /// non-2xx rejection remains Failed when its body truncates, stalls, or drops.
    pub(crate) fn http_status(&mut self, status: u16) -> Result<(), Error> {
        if !(200..=599).contains(&status) {
            return Err(Error::Phase);
        }
        self.edit(|entry| {
            if let Some(previous) = entry.status {
                if previous == status {
                    return Ok(());
                }
                entry.phase = Phase::Unknown;
                entry.evidence.mark_incomplete();
                return Err(Error::Conflict);
            }
            entry.status = Some(status);
            if status >= 300 {
                entry.phase = Phase::Failed;
            }
            Ok(())
        })
    }

    pub(crate) fn observe_usage(
        &mut self,
        protocol: Protocol,
        original: &serde_json::Value,
    ) -> Result<(), Error> {
        self.observe_usage_frame(
            protocol,
            bitrouter_ai::tenancy::usage_evidence::Source::Json,
            original,
        )
    }

    pub(crate) fn observe_usage_frame(
        &mut self,
        protocol: Protocol,
        source: bitrouter_ai::tenancy::usage_evidence::Source,
        original: &serde_json::Value,
    ) -> Result<(), Error> {
        self.edit(|entry| {
            entry.evidence.observe(protocol, original);
            Ok(())
        })?;
        let mut state = self.journal.lock()?;
        let id = state.slots[self.index]
            .as_ref()
            .ok_or(Error::Phase)?
            .id
            .clone();
        use bitrouter_ai::tenancy::usage_evidence::Protocol as Wire;
        let protocol = match protocol {
            Protocol::ChatCompletions => Wire::ChatCompletions,
            Protocol::Responses => Wire::Responses,
            Protocol::Messages => Wire::Messages,
            Protocol::Decisions => Wire::Decisions,
        };
        state
            .usage_evidence
            .observe(&id, protocol, source, original);
        Ok(())
    }

    /// The adapter must call this if its available raw payload was filtered or
    /// evidence could not be retained; default-filled canonical counts are forbidden.
    pub(crate) fn incomplete_usage(&mut self) -> Result<(), Error> {
        self.edit(|entry| {
            entry.evidence.mark_incomplete();
            Ok(())
        })
    }

    /// Only after the existing protocol validator accepts its terminal (and SSE
    /// EOF/trailing-event checks). A later local failure/drop cannot erase it.
    pub(crate) fn validated_completion(&mut self, successful: bool) -> Result<(), Error> {
        self.edit(|entry| {
            let desired = if successful {
                Phase::Completed
            } else {
                Phase::Failed
            };
            if entry.phase == desired {
                entry.terminal_observed = true;
                return Ok(());
            }
            if entry.phase != Phase::Armed || !entry.status.is_some_and(|status| status < 300) {
                entry.phase = Phase::Unknown;
                entry.evidence.mark_incomplete();
                return Err(Error::Conflict);
            }
            entry.phase = desired;
            entry.terminal_observed = true;
            Ok(())
        })
    }

    fn edit(&mut self, f: impl FnOnce(&mut Entry) -> Result<(), Error>) -> Result<(), Error> {
        let mut state = self.journal.lock()?;
        let entry = state.slots[self.index].as_mut().ok_or(Error::Phase)?;
        if entry.phase == Phase::Reserved || !entry.live {
            return Err(Error::Phase);
        }
        f(entry)
    }
}

impl Drop for AttemptPermit {
    fn drop(&mut self) {
        // Poison is never ignored: recover only to record bounded terminal facts.
        // The poisoned mutex remains poisoned, so subsequent freeze/admission
        // fail closed rather than asserting healthy completion.
        let mut state = match self.journal.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = state.slots[self.index].as_mut() {
            if entry.phase == Phase::Reserved {
                state.slots[self.index] = None;
            } else {
                if entry.phase == Phase::Armed {
                    entry.phase = Phase::Unknown;
                    // Counts already observed remain useful, but an unresolved
                    // response cannot certify that its usage stream was final.
                    entry.evidence.mark_incomplete();
                }
                entry.live = false;
            }
        }
        drop(state);
        self.journal.changed.notify_waiters();
    }
}

impl Journal {
    /// Called by the installed stream finalizer before receipt preparation.
    pub(crate) fn observe_estimator(
        &self,
        estimate: Option<bitrouter_ai::tenancy::usage_evidence::Estimate>,
    ) -> Result<(), Error> {
        if let Some(value) = &estimate {
            value.validate().map_err(|_| Error::Conflict)?;
        }
        let mut state = self.lock()?;
        if state.closed {
            return Err(Error::Closed);
        }
        if state.usage_evidence.estimator_seen && state.usage_evidence.estimate != estimate {
            return Err(Error::Conflict);
        }
        state.usage_evidence.estimator_seen = true;
        state.usage_evidence.estimate = estimate;
        Ok(())
    }

    #[cfg(any(test, all(feature = "core", feature = "harness")))]
    fn link_child(&self, child: &Self) -> Result<(), Error> {
        if self.owner.installation != child.owner.installation
            || self.owner.execution == child.owner.execution
        {
            return Err(Error::Binding);
        }
        // Stable lock order prevents inverse-parent calls from deadlocking.
        let (mut parent_state, mut child_state) = if self.owner.execution < child.owner.execution {
            (self.lock()?, child.lock()?)
        } else {
            let child_state = child.lock()?;
            (self.lock()?, child_state)
        };
        if parent_state.closed
            || child_state.closed
            || child_state.armed != 0
            || child_state.slots.iter().any(Option::is_some)
            || child_state.usage_evidence.parent.is_some()
            || !child_state.usage_evidence.children.is_empty()
        {
            return Err(Error::Phase);
        }
        if parent_state.usage_evidence.children.len() >= 64 {
            return Err(Error::Capacity);
        }
        if parent_state.usage_evidence.parent.as_ref() == Some(&child.owner.execution)
            || parent_state
                .usage_evidence
                .children
                .contains(&child.owner.execution)
        {
            return Err(Error::Conflict);
        }
        parent_state
            .usage_evidence
            .children
            .push(child.owner.execution.clone());
        child_state.usage_evidence.parent = Some(self.owner.execution.clone());
        Ok(())
    }
}

/// Link only the actual admitted parent and child, before the child starts work.
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) fn link_owned_child(
    parent: &TenantExecution,
    child: &TenantExecution,
) -> Result<(), bitrouter_ai::tenancy::operation::ErrorCode> {
    if parent.storage_identity() != child.storage_identity() {
        return Err(bitrouter_ai::tenancy::operation::ErrorCode::Forbidden);
    }
    parent
        .journal()?
        .link_child(child.journal()?)
        .map_err(super::journal_error)
}

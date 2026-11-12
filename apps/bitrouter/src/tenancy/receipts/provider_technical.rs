//! Noncritical provider measurements. This journal never grants a send, waits
//! for a producer, changes a receipt, or participates in its durable commitment.
#[cfg(test)]
use bitrouter_ai::tenancy::provider_technical_facts::ProviderTechnicalFactsV1;
use bitrouter_ai::tenancy::{
    identity::AttemptId,
    provider_technical_facts::{
        FirstSemantic, FirstSemanticKind, MAX_OPERATIONS, ProviderOperation,
    },
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

pub(in crate::tenancy) mod http;
pub(crate) mod scope;
pub(in crate::tenancy) mod storage;
#[cfg(test)]
mod tests;

#[derive(Default)]
struct State {
    operations: Vec<Arc<Operation>>,
    selected: Option<Weak<Operation>>,
    frozen: Option<Vec<ProviderOperation>>,
    attempted_write: bool,
}

pub(crate) struct Capture(Mutex<State>, std::sync::atomic::AtomicBool, scope::Scope);

struct Observed {
    attempts: Vec<AttemptId>,
    upstream: Option<u64>,
    first: Option<crate::gateway::pipeline::timing::FirstTokenTiming>,
    first_at: Option<Instant>,
    generation: Option<u64>,
    partial: bool,
    source_complete: bool,
    ended: Option<Instant>,
}

pub(crate) struct Operation {
    capture: Weak<Capture>,
    started: Instant,
    streamed: bool,
    observed: Mutex<Observed>,
}

fn stream_millis(duration: Duration) -> u64 {
    crate::gateway::pipeline::timing::duration_millis(duration)
}
fn json_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Default for Capture {
    fn default() -> Self {
        Self(
            Mutex::new(State::default()),
            std::sync::atomic::AtomicBool::new(true),
            scope::Scope::default(),
        )
    }
}
impl Capture {
    pub(crate) fn healthy(&self) -> bool {
        self.1.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn invalidate(&self) {
        self.1.store(false, std::sync::atomic::Ordering::Release);
        self.2.notify_lost();
    }
    pub(crate) fn finish_selected(&self) {
        let selected = match self.0.try_lock() {
            Ok(state) => state.selected.as_ref().and_then(Weak::upgrade),
            Err(_) => {
                self.invalidate();
                None
            }
        };
        if let Some(operation) = selected {
            operation.finish(false);
        }
    }
    pub(crate) fn clear_selected(&self) {
        match self.0.try_lock() {
            Ok(mut state) if state.frozen.is_none() => state.selected = None,
            _ => self.invalidate(),
        }
    }

    pub(crate) fn begin(self: &Arc<Self>, started: Instant, streamed: bool) -> Arc<Operation> {
        Arc::new(Operation {
            capture: Arc::downgrade(self),
            started,
            streamed,
            observed: Mutex::new(Observed {
                attempts: Vec::new(),
                upstream: None,
                first: None,
                first_at: None,
                generation: None,
                partial: true,
                source_complete: false,
                ended: None,
            }),
        })
    }

    /// Freeze an already observed prefix at the original receipt owner's freeze.
    /// A busy/poisoned diagnostic lock loses facts, never receipt authority.
    pub(crate) fn freeze(&self) {
        if !self.healthy() {
            return;
        }
        let Ok(mut state) = self.0.try_lock() else {
            self.invalidate();
            return;
        };
        if state.frozen.is_some() {
            return;
        }
        let selected = state.selected.as_ref().and_then(Weak::upgrade);
        let mut operations = Vec::new();
        for operation in &state.operations {
            let Ok(observed) = operation.observed.try_lock() else {
                self.invalidate();
                return;
            };
            if observed.attempts.is_empty() {
                continue;
            }
            let chosen = selected
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, operation));
            let upstream = observed.upstream.or_else(|| {
                if operation.streamed && !chosen {
                    observed
                        .ended
                        .map(|at| stream_millis(at.saturating_duration_since(operation.started)))
                } else {
                    None
                }
            });
            let partial = if observed.upstream.is_some() {
                observed.partial
            } else {
                chosen || !observed.source_complete
            };
            operations.push(ProviderOperation {
                attempt_ids: observed.attempts.clone(),
                selected_attempt_id: if chosen {
                    observed.attempts.last().cloned()
                } else {
                    None
                },
                streamed: operation.streamed,
                upstream_duration_ms: upstream,
                first_semantic: observed.first.map(|first| FirstSemantic {
                    ttft_ms: first.ttft_ms,
                    kind: match first.kind {
                        crate::gateway::pipeline::timing::FirstTokenKind::Reasoning => {
                            FirstSemanticKind::Reasoning
                        }
                        crate::gateway::pipeline::timing::FirstTokenKind::Text => {
                            FirstSemanticKind::Text
                        }
                        crate::gateway::pipeline::timing::FirstTokenKind::Tool => {
                            FirstSemanticKind::Tool
                        }
                    },
                }),
                generation_duration_ms: observed.generation,
                partial,
            });
        }
        state.frozen = Some(operations);
    }

    #[cfg(test)]
    pub(in crate::tenancy) fn take(
        &self,
        terminal: &super::store::format::Terminal,
    ) -> Option<ProviderTechnicalFactsV1> {
        if !self.healthy() {
            return None;
        }
        let mut state = self.0.try_lock().ok()?;
        if state.attempted_write {
            return None;
        }
        state.attempted_write = true;
        let operations = state.frozen.as_ref()?.clone();
        if operations.is_empty() {
            return None;
        }
        let facts = terminal.facts();
        let revision = match terminal {
            super::store::format::Terminal::V1(value) => value.receipt.execution_revision.clone(),
            super::store::format::Terminal::V2(value) => value.receipt.execution_revision.clone(),
        };
        let result = ProviderTechnicalFactsV1 {
            schema_version: 1,
            receipt_format: match terminal.format() {
                super::store::format::Format::V1 => 1,
                super::store::format::Format::V2 => 2,
            },
            installation_id: facts.installation.clone(),
            tenant_id: facts.tenant.clone(),
            principal_id: facts.principal.clone(),
            execution_id: facts.execution.clone(),
            execution_revision: revision,
            operations,
        };
        let valid = match terminal {
            super::store::format::Terminal::V1(value) => result.validate_against(&value.receipt),
            super::store::format::Terminal::V2(value) => result.validate_against_v2(&value.receipt),
        };
        valid.ok().map(|()| result)
    }
}

/// The one existing optional task may observe a racing local finalization;
/// neither the receipt owner nor its ACK waits for this diagnostic descriptor.
pub(in crate::tenancy) struct Pending {
    capture: Arc<Capture>,
    operations: Vec<ProviderOperation>,
}
impl Capture {
    pub(in crate::tenancy) fn pending(self: &Arc<Self>) -> Option<Pending> {
        if !self.healthy() {
            return None;
        }
        let mut state = self.0.try_lock().ok()?;
        if state.attempted_write {
            return None;
        }
        state.attempted_write = true;
        Some(Pending {
            capture: self.clone(),
            operations: state.frozen.as_ref()?.clone(),
        })
    }
}
impl Pending {
    #[cfg(test)]
    pub(in crate::tenancy) fn operation_view(
        &self,
        terminal: &super::store::format::Terminal,
    ) -> Option<ProviderTechnicalFactsV1> {
        self.facts(terminal, None, Vec::new())?.operation_view()
    }
    pub(in crate::tenancy) async fn finish(
        self,
        terminal: &super::store::format::Terminal,
        deadline: tokio::time::Instant,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<bitrouter_ai::tenancy::provider_technical_facts_v2::ProviderTechnicalFactsV2> {
        tokio::select! {biased;
            ()=cancel.cancelled()=>return None,
            _=tokio::time::sleep_until(deadline)=>{},
            ()=self.capture.scope_ready()=>{},
        }
        if cancel.is_cancelled() {
            return None;
        }
        let (scope, hops) = self.capture.scope_snapshot()?;
        self.facts(terminal, scope, hops)
    }
    fn facts(
        &self,
        terminal: &super::store::format::Terminal,
        scope: Option<bitrouter_ai::tenancy::provider_technical_facts_v2::PipelineScope>,
        hops: Vec<bitrouter_ai::tenancy::provider_technical_facts_v2::HopObservation>,
    ) -> Option<bitrouter_ai::tenancy::provider_technical_facts_v2::ProviderTechnicalFactsV2> {
        if !self.capture.healthy() {
            return None;
        }
        let facts = terminal.facts();
        let revision = match terminal {
            super::store::format::Terminal::V1(value) => value.receipt.execution_revision.clone(),
            super::store::format::Terminal::V2(value) => value.receipt.execution_revision.clone(),
        };
        let result = bitrouter_ai::tenancy::provider_technical_facts_v2::ProviderTechnicalFactsV2 {
            schema_version: 2,
            receipt_format: match terminal.format() {
                super::store::format::Format::V1 => 1,
                super::store::format::Format::V2 => 2,
            },
            installation_id: facts.installation.clone(),
            tenant_id: facts.tenant.clone(),
            principal_id: facts.principal.clone(),
            execution_id: facts.execution.clone(),
            execution_revision: revision,
            operations: self.operations.clone(),
            pipeline_scope: scope,
            hop_observations: hops,
        };
        let valid = match terminal {
            super::store::format::Terminal::V1(value) => result.validate_against(&value.receipt),
            super::store::format::Terminal::V2(value) => result.validate_against_v2(&value.receipt),
        };
        valid.ok().map(|()| result)
    }
}

impl Operation {
    pub(crate) fn belongs_to(&self, capture: &Arc<Capture>) -> bool {
        self.capture
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, capture))
    }

    pub(crate) fn healthy(&self) -> bool {
        self.capture
            .upgrade()
            .is_some_and(|capture| capture.healthy())
    }
    pub(crate) fn invalidate(&self) {
        if let Some(capture) = self.capture.upgrade() {
            capture.invalidate();
        }
    }

    /// Called only after the existing actual-send arm has returned its real ID.
    pub(crate) fn armed(self: &Arc<Self>, id: &AttemptId) {
        let copied = id.clone();
        // Same lock order as freeze; neither lock is held across any await.
        let Some(capture) = self.capture.upgrade() else {
            return;
        };
        if !capture.healthy() {
            return;
        }
        let Ok(mut state) = capture.0.try_lock() else {
            capture.invalidate();
            return;
        };
        if state.frozen.is_some() {
            capture.invalidate();
            return;
        }
        let Ok(mut observed) = self.observed.try_lock() else {
            self.invalidate();
            return;
        };
        if observed.attempts.len() >= MAX_OPERATIONS || observed.attempts.contains(&copied) {
            capture.invalidate();
            return;
        }
        if observed.attempts.is_empty() {
            if state.operations.len() >= MAX_OPERATIONS {
                capture.invalidate();
                return;
            }
            state.operations.push(self.clone());
        }
        observed.attempts.push(copied);
    }

    pub(crate) fn selected(self: &Arc<Self>) {
        let Some(capture) = self.capture.upgrade() else {
            return;
        };
        match capture.0.try_lock() {
            Ok(mut state) if state.frozen.is_none() => state.selected = Some(Arc::downgrade(self)),
            _ => capture.invalidate(),
        };
    }

    pub(crate) fn semantic(
        &self,
        at: Instant,
        kind: crate::gateway::pipeline::timing::FirstTokenKind,
    ) {
        if !self.streamed {
            return;
        }
        let Ok(mut observed) = self.observed.try_lock() else {
            self.invalidate();
            return;
        };
        let Observed {
            first_at,
            first,
            generation,
            ..
        } = &mut *observed;
        crate::gateway::pipeline::timing::observe_semantic(
            self.started,
            at,
            kind,
            first_at,
            first,
            generation,
        );
    }

    pub(crate) fn source_end(&self, complete: bool) {
        match self.observed.try_lock() {
            Ok(mut observed) => {
                observed.source_complete = complete;
                observed.ended = Some(Instant::now());
            }
            Err(_) => self.invalidate(),
        }
    }

    pub(crate) fn finish(&self, partial: bool) {
        let elapsed = self.started.elapsed();
        if let Ok(mut observed) = self.observed.try_lock() {
            if observed.upstream.is_some() {
                return;
            }
            observed.upstream = Some(if self.streamed {
                stream_millis(elapsed)
            } else {
                json_millis(elapsed)
            });
            observed.partial = if self.streamed {
                partial || !observed.source_complete
            } else {
                partial
            };
        } else {
            self.invalidate();
        }
    }
}

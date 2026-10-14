//! Operation applicability for host-declared pipeline registrations.

use std::any::TypeId;
use std::ops::Deref;
use std::sync::Arc;

use bitrouter_ai::types::{ModelOperation, OperationScope};

/// Stage where a host requires a concrete protection to be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStage {
    /// Local identity and authorization before router resolution.
    PreResolution,
    /// Preparation after the ingress router binding is frozen.
    RouterPreparation,
    /// Local request policy.
    PreRequest,
    /// Final route preparation.
    Route,
    /// Bound effective-model selection.
    ModelSelection,
    /// Upstream execution callbacks.
    Execution,
    /// Settlement recording.
    Settlement,
    /// Success-critical finalization.
    Finalization,
    /// Request and hop observations.
    Observation,
}

pub(crate) struct HookRegistration<T: ?Sized> {
    pub(crate) hook: Arc<T>,
    pub(crate) supported_operations: OperationScope,
    pub(crate) type_id: TypeId,
}

impl<T: ?Sized> HookRegistration<T> {
    pub(crate) fn new(hook: Arc<T>, supported_operations: OperationScope, type_id: TypeId) -> Self {
        Self {
            hook,
            supported_operations,
            type_id,
        }
    }

    pub(crate) fn supports(&self, operation: ModelOperation) -> bool {
        self.supported_operations.contains(operation)
    }
}

impl<T: ?Sized> Clone for HookRegistration<T> {
    fn clone(&self) -> Self {
        Self {
            hook: self.hook.clone(),
            supported_operations: self.supported_operations,
            type_id: self.type_id,
        }
    }
}

impl<T: ?Sized> Deref for HookRegistration<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.hook.as_ref()
    }
}

pub(crate) fn applicable<T: ?Sized>(
    hooks: &[HookRegistration<T>],
    operation: ModelOperation,
) -> Vec<Arc<T>> {
    hooks
        .iter()
        .filter(|hook| hook.supports(operation))
        .map(|hook| hook.hook.clone())
        .collect()
}

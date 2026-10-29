//! Concrete callback registrations owned by application assembly.

use std::sync::Arc;

use bitrouter_ai::extension::request_check::Callback;
use bitrouter_ai::types::OperationScope;

/// One statically linked implementation consumed by a host at startup.
#[derive(Clone)]
pub struct Registration {
    /// Operations whose projection the callback explicitly supports.
    pub supported_operations: OperationScope,
    /// Code/rules revision that must match the configured binding.
    pub revision: String,
    /// Trusted business implementation.
    pub callback: Arc<Callback>,
}

impl Registration {
    /// Construct an implementation. Hosts must still validate the revision and
    /// its configuration match before executing it.
    pub fn new(revision: impl Into<String>, callback: Arc<Callback>) -> Self {
        Self {
            supported_operations: OperationScope::Generation,
            revision: revision.into(),
            callback,
        }
    }
    /// Opt into a migrated projection contract for these operations.
    pub fn with_supported_operations(mut self, operations: OperationScope) -> Self {
        self.supported_operations = operations;
        self
    }
}

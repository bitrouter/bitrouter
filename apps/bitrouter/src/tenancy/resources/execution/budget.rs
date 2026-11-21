//! Cumulative source decoding and target allocation bounds for retained wire routes.

use serde_json::Value;

use super::super::{ExecutionCapacityExceeded, compiler, manifest::ByteBudget};
use crate::config::Config;

const MAX_WORK_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct Budget<'a> {
    operation: Option<&'a crate::storage::operation_budget::OperationBudget<'a>>,
    work: ByteBudget,
}

impl<'a> Budget<'a> {
    pub(super) fn with_operation(
        operation: Option<&'a crate::storage::operation_budget::OperationBudget<'a>>,
    ) -> Self {
        Self {
            operation,
            work: ByteBudget::new(MAX_WORK_BYTES),
        }
    }

    pub(super) fn routing_config(&mut self, source: &Value) -> anyhow::Result<Config> {
        compiler::routing_config_observed(source, Some(&mut self.work), self.operation)
            .map(|(config, _)| config)
    }

    pub(super) fn native_route_config(&mut self, source: &Value) -> anyhow::Result<Config> {
        compiler::native_routing_config_observed(source, Some(&mut self.work), self.operation)
            .map(|(config, _)| config)
    }

    pub(super) fn check_operation(&self) -> anyhow::Result<()> {
        if let Some(operation) = self.operation {
            operation.check()?;
        }
        Ok(())
    }

    /// Target decoding has a separate 8 MiB content quota. Charge its exact
    /// serialized content before the typed clone/projection is allocated.
    pub(super) fn target(&self, value: &Value) -> anyhow::Result<()> {
        if let Some(operation) = self.operation {
            let mut measured = ByteBudget::new(super::super::MAX_ARTIFACT_BYTES);
            measured.charge(value)?;
            let bytes = super::super::MAX_ARTIFACT_BYTES - measured.remaining();
            operation.charge(0, bytes.checked_mul(2).ok_or(ExecutionCapacityExceeded)?)?;
        }
        Ok(())
    }
}

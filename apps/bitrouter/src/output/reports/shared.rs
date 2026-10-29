//! Secret-free human projection of an explicit shared bootstrap receipt.

use crate::output::{CliReport, human::Human};
use bitrouter_ai::gateway::operator::BootstrapReceipt;
use bitrouter_ai::gateway::{execution::GatewayExecution, operator};

/// Only secret-free operator results may reach CLI output. JSON retains the
/// original service shape; unresolved lookup is a successful observation.
#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum OperatorReport {
    Receipt(Box<operator::Receipt>),
    Operation(operator::OperationLookup),
    Revision(operator::Revision),
    Tenant(operator::TenantPolicy),
    Resource(operator::ResourcePolicy),
    Principal(operator::PrincipalMetadata),
    Credential(operator::CredentialMetadata),
    Execution(Box<GatewayExecution>),
}

impl CliReport for OperatorReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        match self {
            Self::Receipt(receipt) => render_receipt(receipt, human),
            Self::Operation(operator::OperationLookup::Committed { receipt }) => {
                human.field("status", "committed")?;
                render_receipt(receipt, human)
            }
            Self::Operation(operator::OperationLookup::Unresolved { operation_id }) => {
                human.field("status", "unresolved")?;
                human.field("operation", operation_id)?;
                human.note("Receipt absence does not prove rollback. Keep the original operation ID and mutation.")
            }
            Self::Revision(revision) => {
                human.field("configuration generation", revision.config_generation)?;
                human.field("key generation", revision.key_generation)
            }
            Self::Tenant(value) => metadata(value, human),
            Self::Resource(value) => metadata(value, human),
            Self::Principal(value) => metadata(value, human),
            Self::Credential(value) => metadata(value, human),
            Self::Execution(value) => metadata(value, human),
        }
    }
}

fn render_receipt(receipt: &operator::Receipt, human: &mut Human<'_>) -> std::io::Result<()> {
    human.field("namespace", &receipt.namespace)?;
    human.field("operator", &receipt.operator_id)?;
    human.field("operation", &receipt.operation_id)?;
    human.field("committed at (Unix ms)", receipt.committed_at_ms)?;
    metadata(&receipt.outcome, human)
}

fn metadata(value: &impl serde::Serialize, human: &mut Human<'_>) -> std::io::Result<()> {
    human.embedded_json(&serde_json::to_value(value).map_err(std::io::Error::other)?)
}

impl CliReport for BootstrapReceipt {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.field("namespace", &self.namespace)?;
        human.field("operator", &self.operator_id)?;
        human.field("operation", &self.operation_id)?;
        human.field("configuration generation", self.revision.config_generation)?;
        human.field("key generation", self.revision.key_generation)?;
        human.field("committed operations", self.operations.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    #[test]
    fn bootstrap_report_has_stable_json_and_human_identity() -> anyhow::Result<()> {
        let receipt = BootstrapReceipt {
            namespace: "fixture".into(),
            operator_id: "operator".into(),
            operation_id: "root".into(),
            committed_at_ms: 42,
            revision: bitrouter_ai::gateway::operator::Revision {
                config_generation: 1,
                key_generation: 1,
            },
            operations: vec![],
        };
        let encoded = Output::new(Format::Json).render_to_vec(&receipt);
        anyhow::ensure!(serde_json::from_slice::<BootstrapReceipt>(&encoded)? == receipt);
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&receipt))?;
        for field in [
            "fixture",
            "operator",
            "root",
            "configuration generation",
            "key generation",
        ] {
            anyhow::ensure!(human.contains(field));
        }
        Ok(())
    }
}

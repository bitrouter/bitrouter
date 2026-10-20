//! Secret-free human projection of an explicit shared bootstrap receipt.

use crate::output::{CliReport, human::Human};
use bitrouter_ai::gateway::operator::BootstrapReceipt;

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

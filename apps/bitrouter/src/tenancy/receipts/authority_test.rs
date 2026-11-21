//! One-shot observation at a real credential-read await boundary. Test builds only.
use crate::tenancy::Service;
use bitrouter_ai::tenancy::identity::ExecutionId;
use std::sync::{Arc, OnceLock};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(in crate::tenancy) struct CredentialGate {
    pub execution: OnceLock<ExecutionId>,
    pub entered: Notify,
    pub release: CancellationToken,
}

impl Service {
    pub(in crate::tenancy) fn hold_receipt_credential_read_for_test(
        &self,
    ) -> anyhow::Result<Arc<CredentialGate>> {
        let mut slot = self
            .receipt_credential_gate
            .lock()
            .map_err(|_| anyhow::anyhow!("credential test gate poisoned"))?;
        anyhow::ensure!(slot.is_none(), "credential test gate already installed");
        let gate = Arc::new(CredentialGate::default());
        *slot = Some(gate.clone());
        Ok(gate)
    }

    pub(in crate::tenancy) async fn receipt_credential_read_for_test(&self, id: &ExecutionId) {
        let gate = match self.receipt_credential_gate.lock() {
            Ok(mut slot) => slot.take(),
            Err(poison) => poison.into_inner().take(),
        };
        if let Some(gate) = gate {
            // The slot is consumed once. This ID is observed from admission,
            // never supplied by the test as an authority/proof constructor.
            if gate.execution.set(id.clone()).is_err() {
                tracing::error!("credential test gate entered more than once");
            }
            gate.entered.notify_one();
            gate.release.cancelled().await;
        }
    }
}

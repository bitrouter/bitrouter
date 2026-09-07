//! Launch preparation and exact prompt provenance for producer observations.

use super::*;
use crate::session_evidence::adapter_bridge;

impl ControllerEvidence {
    pub async fn adapter_arguments(&self, command: &str, args: &[String]) -> Result<Vec<String>> {
        let prepared = adapter_bridge::prepare_launch(
            &self.spool,
            self.collector.root().harness,
            command,
            args,
        )
        .await?;
        // Installation is not runtime support proof. The loaded adapter must
        // advertise the matching capability in its initialize response.
        Ok(prepared.unwrap_or_else(|| args.to_vec()))
    }

    pub(super) async fn prepare_prompt_origin(
        &self,
        operation_id: &str,
        mut params: Value,
    ) -> Result<Value> {
        // A manager-provided old claim is never authority for this operation,
        // even when the new prompt has an unresolved scope or no bridge.
        if let Some(metadata) = params.get_mut("_meta").and_then(Value::as_object_mut) {
            metadata.remove(adapter_bridge::META_KEY);
        }
        if !self.state.lock().await.bridge_capable {
            return Ok(params);
        }
        let Some(origin) = self
            .store
            .prompt_origin(&self.controller_id, operation_id)
            .await?
        else {
            return Ok(params);
        };
        let object = params
            .as_object_mut()
            .context("prompt request must be an object")?;
        let state = match object.get("_meta") {
            None => "absent",
            Some(Value::Null) => "null",
            Some(Value::Object(_)) => "object",
            _ => return Ok(params),
        };
        let mut metadata = object
            .get("_meta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert(
            adapter_bridge::META_KEY.into(),
            json!({"origin":origin, "metaState":state}),
        );
        object.insert("_meta".into(), Value::Object(metadata));
        Ok(params)
    }
}

#[cfg(test)]
mod tests;

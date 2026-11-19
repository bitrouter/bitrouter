//! M0 persisted a permission prompt, its resolution and the accepted answer key
//! before it could record a running mutation. None of these facts alone grants it.
use super::*;

#[derive(Default)]
pub(super) struct Approvals {
    seen: BTreeSet<String>,
    pending: Option<Value>,
    resolved: bool,
    keyed: bool,
}

impl Approvals {
    pub(super) fn request(&mut self, prompt: &Value) -> Result<()> {
        ensure!(
            self.pending.is_none()
                && self.seen.insert(field(prompt, "request_id")?.to_owned())
                && matches!(field(prompt, "tool_name")?, "write" | "edit" | "shell")
                && !field(prompt, "tool_id")?.is_empty(),
            "duplicate or unsupported approval"
        );
        self.pending = Some(prompt.clone());
        Ok(())
    }

    pub(super) fn resolve(&mut self, answer: &Value) -> Result<()> {
        let prompt = self.pending.as_ref().context("answer without prompt")?;
        ensure!(
            !self.resolved
                && answer
                    == &json!({"kind":"input_resolved","request_id":prompt["request_id"],"approved":true}),
            "unconfirmed M0 approval"
        );
        self.resolved = true;
        Ok(())
    }

    pub(super) fn accepted(&mut self, turn: &str, entry: &Value) -> Result<()> {
        let prompt = self
            .pending
            .as_ref()
            .context("approval key without prompt")?;
        ensure!(
            self.resolved
                && !self.keyed
                && entry["turn_id"] == turn
                && !field(entry, "key")?.is_empty()
                && entry["fingerprint"]
                    == crate::storage::checkpoint::sha256(&serde_json::to_vec(&(
                        turn,
                        field(prompt, "request_id")?,
                        true
                    ))?),
            "approval key differs from decision"
        );
        self.keyed = true;
        Ok(())
    }

    pub(super) fn consume(&mut self, call: &Value) -> Result<()> {
        if !matches!(field(call, "name")?, "write" | "edit" | "shell") {
            return Ok(());
        }
        let prompt = self.pending.take().context("mutation without approval")?;
        ensure!(
            self.keyed
                && self.resolved
                && prompt["tool_id"] == call["item_id"]
                && prompt["tool_name"] == call["name"]
                && prompt["arguments"] == call["arguments"],
            "mutation differs from approved call"
        );
        self.keyed = false;
        self.resolved = false;
        Ok(())
    }

    pub(super) fn finished(&self) -> bool {
        self.pending.is_none()
    }
}

/// M0's workspace tools returned these JSON shapes after a completed effect.
/// The native outcome and dispatch ownership are checked independently.
pub(super) fn confirmed_output(tool: &str, args: &Value, output: &ToolResultOutput) -> bool {
    let ToolResultOutput::Json { value } = output else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    match tool {
        "write" => {
            object.len() == 2
                && args["path"].is_string()
                && value["path"] == args["path"]
                && args["content"]
                    .as_str()
                    .is_some_and(|text| value["bytes"].as_u64() == Some(text.len() as u64))
        }
        "edit" => {
            object.len() == 3
                && args["path"].is_string()
                && value["path"] == args["path"]
                && args["edits"].as_array().is_some_and(|v| !v.is_empty())
                && value["bytes"].as_u64().is_some()
                && value["diff"].is_string()
        }
        "shell" => {
            object.len() == 7
                && args["command"].is_string()
                && value["interpreter"]["executable"].is_string()
                && value["interpreter"]["dialect"].is_string()
                && object
                    .get("exit_status")
                    .is_some_and(|v| v.is_null() || v.is_i64())
                && value["stdout"].is_string()
                && value["stderr"].is_string()
                && value["stdout_truncated"].is_boolean()
                && value["stderr_truncated"].is_boolean()
                && value["timed_out"] == false
        }
        _ => false,
    }
}

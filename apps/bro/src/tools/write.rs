//! Whole-file writes with explicit effect evidence.

use bitrouter_ai::types::ToolResultOutput;

use super::{MAX_FILE_BYTES, WorkspaceTools, WriteArgs};
use bitrouter_ai::harness::runtime::EffectStatus;

impl WorkspaceTools {
    pub(super) fn write(
        &self,
        args: WriteArgs,
        effect: &mut EffectStatus,
    ) -> Result<ToolResultOutput, String> {
        if args.content.len() as u64 > MAX_FILE_BYTES {
            return Err("file exceeds the 2 MiB write limit".into());
        }
        let mut target = self
            .destination(&args.path, true, effect)
            .map_err(|e| e.to_string())?;
        let original = target.original().map_err(|e| e.to_string())?;
        target
            .publish(args.content.as_bytes(), original.as_deref(), effect)
            .map_err(|e| e.to_string())?;
        *effect = EffectStatus::Completed;
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({"path": args.path, "bytes": args.content.len()}),
        })
    }
}

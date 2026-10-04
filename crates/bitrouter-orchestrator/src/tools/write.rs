//! Whole-file writes with explicit effect evidence.

use std::fs;

use bitrouter_sdk::language_model::ToolResultOutput;

use super::{MAX_FILE_BYTES, WorkspaceTools, WriteArgs, persist_text};
use crate::store::EffectStatus;

impl WorkspaceTools {
    pub(super) fn write(
        &self,
        args: WriteArgs,
        effect: &mut EffectStatus,
    ) -> Result<ToolResultOutput, String> {
        if args.content.len() as u64 > MAX_FILE_BYTES {
            return Err("file exceeds the 2 MiB write limit".into());
        }
        let path = self.writable_path(&args.path, true, effect)?;
        let original = if path.exists() {
            if fs::metadata(&path)
                .map_err(|error| error.to_string())?
                .len()
                > MAX_FILE_BYTES
            {
                return Err("existing file exceeds the 2 MiB write limit".into());
            }
            Some(fs::read(&path).map_err(|error| error.to_string())?)
        } else {
            None
        };
        persist_text(&path, args.content.as_bytes(), original.as_deref(), effect)?;
        *effect = EffectStatus::Completed;
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({"path": args.path, "bytes": args.content.len()}),
        })
    }
}

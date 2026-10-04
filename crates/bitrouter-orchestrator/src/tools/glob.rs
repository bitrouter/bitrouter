//! Workspace filename and path matching.

use std::path::Path;

use bitrouter_sdk::language_model::ToolResultOutput;
use globset::Glob;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_GLOB_RESULTS, GlobArgs, WorkspaceTools, append_search_line, search_limit,
    workspace_walk,
};

impl WorkspaceTools {
    pub(super) fn glob(
        &self,
        args: GlobArgs,
        cancel: &CancellationToken,
    ) -> Result<ToolResultOutput, String> {
        if args.pattern.is_empty() {
            return Err("pattern must not be empty".into());
        }
        let path = self.search_path(args.path.as_deref())?;
        if !path.is_dir() {
            return Err("path is not a directory".into());
        }
        let limit = search_limit(args.limit, DEFAULT_GLOB_RESULTS)?;
        let glob = Glob::new(&args.pattern)
            .map_err(|error| format!("invalid glob: {error}"))?
            .compile_matcher();
        let match_path = args.pattern.contains('/');
        let mut output = String::new();
        let mut matches = 0;
        let mut truncated = false;
        for entry in workspace_walk(&path).build() {
            if cancel.is_cancelled() {
                return Err("glob cancelled".into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            if entry.depth() == 0 || entry.file_type().is_some_and(|kind| kind.is_symlink()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&path)
                .map_err(|error| error.to_string())?;
            let candidate = if match_path {
                relative
            } else {
                Path::new(entry.file_name())
            };
            if !glob.is_match(candidate) {
                continue;
            }
            if matches == limit {
                truncated = true;
                break;
            }
            let mut display = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                display.push('/');
            }
            if !append_search_line(&mut output, &display) {
                truncated = true;
                break;
            }
            matches += 1;
        }
        if truncated {
            output.push_str("[results truncated; refine pattern or raise limit]\n");
        }
        if output.is_empty() {
            output.push_str("No files found matching pattern");
        }
        Ok(ToolResultOutput::Text { value: output })
    }
}

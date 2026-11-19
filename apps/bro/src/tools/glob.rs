//! Workspace filename and path matching.

use std::path::Path;

use bitrouter_ai::types::ToolResultOutput;
use globset::Glob;
use tokio_util::sync::CancellationToken;

use super::{DEFAULT_GLOB_RESULTS, GlobArgs, WorkspaceTools, append_search_line, search_limit};

impl WorkspaceTools {
    pub(super) fn glob(
        &self,
        args: GlobArgs,
        cancel: &CancellationToken,
    ) -> Result<ToolResultOutput, String> {
        if args.pattern.is_empty() {
            return Err("pattern must not be empty".into());
        }
        let (path, node) = self
            .lookup(args.path.as_deref())
            .map_err(|e| e.to_string())?;
        let super::files::Node::Directory(dir) = node else {
            return Err("path is not a directory".into());
        };
        let limit = search_limit(args.limit, DEFAULT_GLOB_RESULTS)?;
        let glob = Glob::new(&args.pattern)
            .map_err(|error| format!("invalid glob: {error}"))?
            .compile_matcher();
        let match_path = args.pattern.contains('/');
        let mut output = String::new();
        let mut matches = 0;
        let mut truncated = false;
        super::walk::walk(dir, &path, cancel, |_, name, workspace_path, kind| {
            let candidate = if match_path {
                workspace_path.strip_prefix(&path)?
            } else {
                Path::new(name)
            };
            if !glob.is_match(candidate) {
                return Ok(true);
            }
            if matches == limit {
                truncated = true;
                return Ok(false);
            }
            let mut display = workspace_path.to_string_lossy().replace('\\', "/");
            if kind == crate::workspace::children::Kind::Directory {
                display.push('/');
            }
            if !append_search_line(&mut output, &display) {
                truncated = true;
                return Ok(false);
            }
            matches += 1;
            Ok(true)
        })
        .map_err(|e| e.to_string())?;
        if truncated {
            output.push_str("[results truncated; refine pattern or raise limit]\n");
        }
        if output.is_empty() {
            output.push_str("No files found matching pattern");
        }
        Ok(ToolResultOutput::Text { value: output })
    }
}

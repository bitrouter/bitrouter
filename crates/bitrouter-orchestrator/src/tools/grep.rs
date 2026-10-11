//! Bounded workspace content search.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

use bitrouter_ai::types::ToolResultOutput;
use globset::Glob;
use regex::RegexBuilder;
use tokio_util::sync::CancellationToken;

use super::{
    GrepArgs, MAX_FILE_BYTES, MAX_GREP_LINE_CHARS, MAX_GREP_MATCHES, WorkspaceTools,
    append_search_line, search_limit, workspace_walk,
};

impl WorkspaceTools {
    pub(super) fn grep(
        &self,
        args: GrepArgs,
        cancel: &CancellationToken,
    ) -> Result<ToolResultOutput, String> {
        if args.pattern.is_empty() {
            return Err("pattern must not be empty".into());
        }
        let path = self.search_path(args.path.as_deref())?;
        let limit = search_limit(args.limit, MAX_GREP_MATCHES)?;
        let pattern = if args.literal {
            regex::escape(&args.pattern)
        } else {
            args.pattern
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(args.ignore_case)
            .size_limit(1 << 20)
            .build()
            .map_err(|error| format!("invalid regex: {error}"))?;
        let glob = args
            .glob
            .as_deref()
            .map(Glob::new)
            .transpose()
            .map_err(|error| format!("invalid glob: {error}"))?
            .map(|glob| glob.compile_matcher());
        let mut output = String::new();
        let mut matches = 0;
        let mut truncated = false;
        if !path.is_file() && !path.is_dir() {
            return Err("path is not a file or directory".into());
        }
        for entry in workspace_walk(&path).build() {
            if cancel.is_cancelled() {
                return Err("grep cancelled".into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|error| error.to_string())?;
            if let Some(glob) = &glob {
                let file = entry.file_name();
                if !glob.is_match(relative) && !glob.is_match(Path::new(file)) {
                    continue;
                }
            }
            let canonical = entry
                .path()
                .canonicalize()
                .map_err(|error| error.to_string())?;
            if !canonical.starts_with(&self.root) {
                return Err("path escapes the workspace".into());
            }
            if fs::metadata(&canonical)
                .map_err(|error| error.to_string())?
                .len()
                > MAX_FILE_BYTES
            {
                continue;
            }
            let file = File::open(canonical).map_err(|error| error.to_string())?;
            for (index, line) in BufReader::new(file).lines().enumerate() {
                if cancel.is_cancelled() {
                    return Err("grep cancelled".into());
                }
                let line = match line {
                    Ok(line) => line,
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => break,
                    Err(error) => return Err(error.to_string()),
                };
                if !regex.is_match(&line) {
                    continue;
                }
                if matches == limit {
                    truncated = true;
                    break;
                }
                let excerpt: String = line.chars().take(MAX_GREP_LINE_CHARS).collect();
                let display = format!(
                    "{}:{}: {}",
                    relative.to_string_lossy().replace('\\', "/"),
                    index + 1,
                    excerpt
                );
                if !append_search_line(&mut output, &display) {
                    truncated = true;
                    break;
                }
                matches += 1;
            }
            if truncated {
                break;
            }
        }
        if truncated {
            output.push_str("[matches truncated; refine pattern or raise limit]\n");
        }
        if output.is_empty() {
            output.push_str("No matches found");
        }
        Ok(ToolResultOutput::Text { value: output })
    }
}

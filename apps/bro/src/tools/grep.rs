//! Bounded workspace content search.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use bitrouter_ai::types::ToolResultOutput;
use globset::Glob;
use regex::RegexBuilder;
use tokio_util::sync::CancellationToken;

use super::{
    GrepArgs, MAX_FILE_BYTES, MAX_GREP_LINE_CHARS, MAX_GREP_MATCHES, WorkspaceTools,
    append_search_line, search_limit,
};
use crate::workspace::children::{Access, Kind, regular};

impl WorkspaceTools {
    pub(super) fn grep(
        &self,
        args: GrepArgs,
        cancel: &CancellationToken,
    ) -> Result<ToolResultOutput, String> {
        if args.pattern.is_empty() {
            return Err("pattern must not be empty".into());
        }
        let (path, node) = self
            .lookup(args.path.as_deref())
            .map_err(|e| e.to_string())?;
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
        let mut search = |relative: &Path, file: File| -> anyhow::Result<bool> {
            regular(&file)?;
            if file.metadata()?.len() > MAX_FILE_BYTES {
                return Ok(true);
            }
            for (index, line) in BufReader::new(file.take(MAX_FILE_BYTES + 1))
                .lines()
                .enumerate()
            {
                if cancel.is_cancelled() {
                    anyhow::bail!("grep cancelled");
                }
                let line = match line {
                    Ok(line) => line,
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => break,
                    Err(error) => return Err(error.into()),
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
            Ok(!truncated)
        };
        let matches_glob = |relative: &Path| {
            glob.as_ref().is_none_or(|g| {
                g.is_match(relative)
                    || relative
                        .file_name()
                        .is_some_and(|n| g.is_match(Path::new(n)))
            })
        };
        match node {
            super::files::Node::File(file) => {
                if matches_glob(&path) {
                    search(&path, file).map_err(|e| e.to_string())?;
                }
            }
            super::files::Node::Directory(dir) => {
                super::walk::walk(dir, &path, cancel, |parent, name, relative, kind| {
                    if kind == Kind::File && matches_glob(relative) {
                        match parent.open(name, Access::Read) {
                            Ok(file) => return search(relative, file),
                            Err(error) if error.kind() == io::ErrorKind::InvalidData => (),
                            Err(error) => return Err(error.into()),
                        }
                    }
                    Ok(true)
                })
                .map_err(|e| e.to_string())?
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

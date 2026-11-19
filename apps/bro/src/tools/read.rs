//! Bounded file and directory reads.

use super::files::{Node, contents};
use crate::workspace::children::Kind;

use bitrouter_ai::types::ToolResultOutput;

use super::{
    DEFAULT_DIRECTORY_ENTRIES, MAX_READ_BYTES, MAX_READ_LINES, ReadArgs, WorkspaceTools,
    search_limit,
};

impl WorkspaceTools {
    pub(super) fn read(&self, args: ReadArgs) -> Result<ToolResultOutput, String> {
        let (_, node) = self.lookup(Some(&args.path)).map_err(|e| e.to_string())?;
        let quoted = serde_json::to_string(&args.path).map_err(|error| error.to_string())?;
        let offset = args.offset.unwrap_or(1);
        if offset == 0 {
            return Err("offset must be positive".into());
        }
        match node {
            Node::File(mut file) => {
                let original = String::from_utf8(contents(&mut file).map_err(|e| e.to_string())?)
                    .map_err(|_| "file is not UTF-8")?;
                let limit = search_limit(args.limit, MAX_READ_LINES)?;
                read_page(
                    format!("File {quoted}\n"),
                    original
                        .lines()
                        .enumerate()
                        .map(|(index, line)| format!("L{}: {line}\n", index + 1)),
                    offset,
                    limit,
                    "(empty file)",
                )
            }
            Node::Directory(dir) => {
                let limit = search_limit(args.limit, DEFAULT_DIRECTORY_ENTRIES)?;
                let mut entries = dir
                    .entries()
                    .map_err(|error| error.to_string())?
                    .map(|entry| {
                        let (name, kind) = entry.map_err(|error| error.to_string())?;
                        let mut name = directory_name(name)?;
                        let kind = match kind {
                            Kind::Directory => {
                                name.push('/');
                                "directory"
                            }
                            Kind::File => "file",
                            Kind::Link => "symlink",
                            Kind::Special => "special",
                        };
                        Ok((name, kind))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                sort_directory_entries(&mut entries);
                let rendered = entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, kind))| {
                        serde_json::to_string(&name)
                            .map(|name| format!("E{}: {kind} {name}\n", index + 1))
                            .map_err(|error| error.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                read_page(
                    format!("Directory {quoted}\n"),
                    rendered.into_iter(),
                    offset,
                    limit,
                    "(empty directory)",
                )
            }
        }
    }
}

pub(super) fn sort_directory_entries(entries: &mut [(String, &str)]) {
    entries.sort_by_cached_key(|(name, _)| (name.to_lowercase(), name.clone()));
}

pub(super) fn directory_name(name: std::ffi::OsString) -> Result<String, String> {
    name.into_string()
        .map_err(|_| "directory contains a non-UTF-8 name".into())
}

pub(super) fn read_page(
    mut output: String,
    records: impl Iterator<Item = String>,
    offset: usize,
    limit: usize,
    empty: &str,
) -> Result<ToolResultOutput, String> {
    const FOOTER_RESERVE: usize = 96;
    if output.len() + FOOTER_RESERVE >= MAX_READ_BYTES {
        return Err("read header exceeds the output limit".into());
    }
    let content_start = output.len();
    let mut emitted = 0;
    let mut any = false;
    for (index, record) in records.enumerate() {
        any = true;
        if index < offset - 1 {
            continue;
        }
        if record.len() + content_start + FOOTER_RESERVE > MAX_READ_BYTES {
            return Err(format!(
                "record {} cannot fit in the 50 KiB output limit",
                index + 1
            ));
        }
        if emitted == limit || output.len() + record.len() + FOOTER_RESERVE > MAX_READ_BYTES {
            output.push_str(&format!(
                "[output truncated; continue with offset={}]\n",
                index + 1
            ));
            return Ok(ToolResultOutput::Text { value: output });
        }
        output.push_str(&record);
        emitted += 1;
    }
    if emitted == 0 {
        output.push_str(if any { "(end of input)" } else { empty });
        output.push('\n');
    }
    Ok(ToolResultOutput::Text { value: output })
}

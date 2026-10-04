//! Bounded file and directory reads.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use bitrouter_sdk::language_model::ToolResultOutput;

use super::{
    DEFAULT_DIRECTORY_ENTRIES, MAX_FILE_BYTES, MAX_READ_BYTES, MAX_READ_LINES, ReadArgs,
    WorkspaceTools, search_limit,
};

impl WorkspaceTools {
    pub(super) fn read(&self, args: ReadArgs) -> Result<ToolResultOutput, String> {
        let path = self.search_path(Some(&args.path))?;
        let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
        let quoted = serde_json::to_string(&args.path).map_err(|error| error.to_string())?;
        let offset = args.offset.unwrap_or(1);
        if offset == 0 {
            return Err("offset must be positive".into());
        }
        if metadata.is_file() {
            let original = read_text(&path)?;
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
        } else if metadata.is_dir() {
            let limit = search_limit(args.limit, DEFAULT_DIRECTORY_ENTRIES)?;
            let mut entries = fs::read_dir(path)
                .map_err(|error| error.to_string())?
                .map(|entry| {
                    let entry = entry.map_err(|error| error.to_string())?;
                    let mut name = directory_name(entry.file_name())?;
                    let kind = entry.file_type().map_err(|error| error.to_string())?;
                    let kind = if kind.is_dir() {
                        name.push('/');
                        "directory"
                    } else if kind.is_symlink() {
                        "symlink"
                    } else if kind.is_file() {
                        "file"
                    } else {
                        "special"
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
        } else {
            Err("path is not a regular file or directory".into())
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

fn read_text(path: &Path) -> Result<String, String> {
    if !path
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("read requires a regular file".into());
    }
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("file exceeds the 2 MiB read limit".into());
    }
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8".into())
}

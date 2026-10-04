//! Original-text edits with explicit effect evidence.

use std::fs;

use bitrouter_sdk::language_model::ToolResultOutput;

use super::{EditArgs, MAX_FILE_BYTES, WorkspaceTools, persist_text};
use crate::store::EffectStatus;

impl WorkspaceTools {
    pub(super) fn edit(
        &self,
        args: EditArgs,
        effect: &mut EffectStatus,
    ) -> Result<ToolResultOutput, String> {
        if args.edits.is_empty() {
            return Err("edits must contain at least one replacement".into());
        }
        let path = self.writable_path(&args.path, false, effect)?;
        let original_bytes = fs::read(&path).map_err(|error| error.to_string())?;
        if original_bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("file exceeds the 2 MiB edit limit".into());
        }
        let original =
            std::str::from_utf8(&original_bytes).map_err(|_| "file is not UTF-8".to_string())?;
        let (bom, searchable) = match original.strip_prefix('\u{feff}') {
            Some(text) => ("\u{feff}", text),
            None => ("", original),
        };
        let crlf = searchable.matches("\r\n").count() > searchable.matches('\n').count() / 2;
        let mut replacements = Vec::with_capacity(args.edits.len());
        for edit in &args.edits {
            if edit.old_text.is_empty() {
                return Err("oldText must not be empty".into());
            }
            let exact = unique_match(searchable, &edit.old_text)?;
            let (start, old_len) = match exact {
                Some(start) => (start, edit.old_text.len()),
                None => {
                    let normalized = normalize_line_endings(&edit.old_text, crlf);
                    match unique_match(searchable, &normalized)? {
                        Some(start) => (start, normalized.len()),
                        None => return Err("oldText was not found in the original file".into()),
                    }
                }
            };
            replacements.push((
                start,
                start + old_len,
                normalize_line_endings(&edit.new_text, crlf),
            ));
        }
        replacements.sort_by_key(|(start, _, _)| *start);
        for pair in replacements.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err("edits overlap in the original file".into());
            }
        }
        let mut updated = String::with_capacity(original.len());
        updated.push_str(bom);
        let mut position = 0;
        for (start, end, replacement) in replacements {
            updated.push_str(&searchable[position..start]);
            updated.push_str(&replacement);
            position = end;
        }
        updated.push_str(&searchable[position..]);
        if updated == original {
            return Err("edits would not change the file".into());
        }
        if updated.len() as u64 > MAX_FILE_BYTES {
            return Err("edited file exceeds the 2 MiB limit".into());
        }
        persist_text(&path, updated.as_bytes(), Some(&original_bytes), effect)?;
        *effect = EffectStatus::Completed;
        let diff = similar::TextDiff::from_lines(original, &updated)
            .unified_diff()
            .header(&format!("a/{}", args.path), &format!("b/{}", args.path))
            .to_string();
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({
                "path": args.path,
                "bytes": updated.len(),
                "diff": diff.chars().take(16_384).collect::<String>()
            }),
        })
    }
}

fn unique_match(content: &str, needle: &str) -> Result<Option<usize>, String> {
    let Some(first) = content.find(needle) else {
        return Ok(None);
    };
    let next = first + content[first..].chars().next().map_or(0, char::len_utf8);
    if content[next..].contains(needle) {
        return Err("oldText must occur exactly once in the original file".into());
    }
    Ok(Some(first))
}

fn normalize_line_endings(text: &str, crlf: bool) -> String {
    let lf = text.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bitrouter_sdk::language_model::{Tool, ToolResultOutput};
use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{RunEvent, ToolMode};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_READ_LINES: usize = 2_000;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_LIVE_CHUNKS: usize = 8_192;
const MAX_BASH_SECONDS: u64 = 120;
const MAX_SEARCH_LIMIT: usize = 2_000;
const MAX_LS_ENTRIES: usize = 500;
const MAX_FIND_RESULTS: usize = 1_000;
const MAX_GREP_MATCHES: usize = 100;
const MAX_GREP_LINE_CHARS: usize = 500;

#[derive(Clone)]
pub(crate) struct WorkspaceTools {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditReplacement {
    old_text: String,
    new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    edits: Vec<EditReplacement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LsArgs {
    path: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    limit: Option<usize>,
}

impl WorkspaceTools {
    pub(crate) fn new(root: &Path) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "workspace is not a directory",
            ));
        }
        Ok(Self { root })
    }

    pub(crate) fn allowed(mode: ToolMode, name: &str) -> bool {
        match name {
            "read" | "ls" | "find" | "grep" => true,
            "write" | "edit" => mode == ToolMode::Coding,
            "bash" => mode == ToolMode::Coding && !cfg!(windows),
            "powershell" => mode == ToolMode::Coding && cfg!(windows),
            _ => false,
        }
    }

    pub(crate) fn read_only(name: &str) -> bool {
        matches!(name, "read" | "ls" | "find" | "grep")
    }

    pub(crate) fn declarations(mode: ToolMode) -> Vec<Tool> {
        [
            (
                "read",
                "Read a UTF-8 workspace file. Output is limited to 2000 lines or 50 KiB; use offset and limit to read more.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer", "minimum": 1},
                        "limit": {"type": "integer", "minimum": 1}
                    },
                    "required": ["path"], "additionalProperties": false
                }),
            ),
            (
                "ls",
                "List one workspace directory, including dotfiles. Directory names end with '/'. Returns at most 500 entries or 50 KiB.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                    },
                    "additionalProperties": false
                }),
            ),
            (
                "find",
                "Find workspace paths by glob, respecting .gitignore. Returns at most 1000 paths or 50 KiB.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                    },
                    "required": ["pattern"], "additionalProperties": false
                }),
            ),
            (
                "grep",
                "Search workspace text files by regex, respecting .gitignore. Returns path:line matches, at most 100 matches or 50 KiB. Set literal for exact text.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string"},
                        "glob": {"type": "string"},
                        "ignoreCase": {"type": "boolean"},
                        "literal": {"type": "boolean"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                    },
                    "required": ["pattern"], "additionalProperties": false
                }),
            ),
            (
                "write",
                "Create or overwrite a UTF-8 workspace file, creating parent directories as needed.",
                serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                    "required": ["path", "content"], "additionalProperties": false
                }),
            ),
            (
                "edit",
                "Edit one existing workspace file. Each nonempty oldText must match exactly one non-overlapping region of the original file; all edits apply together.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "edits": {"type": "array", "minItems": 1, "items": {
                            "type": "object",
                            "properties": {"oldText": {"type": "string"}, "newText": {"type": "string"}},
                            "required": ["oldText", "newText"], "additionalProperties": false
                        }}
                    },
                    "required": ["path", "edits"], "additionalProperties": false
                }),
            ),
            (
                "bash",
                "Run a shell command in the server workspace. Commands can affect paths outside the workspace. Returns exit status and bounded stdout/stderr.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "integer", "minimum": 1, "maximum": 120}
                    },
                    "required": ["command"], "additionalProperties": false
                }),
            ),
            (
                "powershell",
                "Run a PowerShell command in the server workspace on Windows. Commands can affect paths outside the workspace. Returns exit status and bounded stdout/stderr.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "integer", "minimum": 1, "maximum": 120}
                    },
                    "required": ["command"], "additionalProperties": false
                }),
            ),
        ]
        .into_iter()
        .filter(|(name, _, _)| Self::allowed(mode, name))
        .map(|(name, description, parameters)| Tool::Function {
            name: name.into(),
            description: Some(description.into()),
            parameters,
            strict: Some(true),
            provider_metadata: Default::default(),
        })
        .collect()
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        arguments: &str,
        cancel: &CancellationToken,
        tool_id: &str,
        live: Option<&mpsc::Sender<RunEvent>>,
    ) -> ToolResultOutput {
        let result = match name {
            "read" => serde_json::from_str::<ReadArgs>(arguments)
                .map_err(|error| format!("invalid read arguments: {error}"))
                .and_then(|args| self.read(args)),
            "ls" => serde_json::from_str::<LsArgs>(arguments)
                .map_err(|error| format!("invalid ls arguments: {error}"))
                .and_then(|args| self.ls(args)),
            "find" => match serde_json::from_str::<FindArgs>(arguments) {
                Ok(args) => {
                    let tools = self.clone();
                    let cancel = cancel.clone();
                    match tokio::task::spawn_blocking(move || tools.find(args, &cancel)).await {
                        Ok(result) => result,
                        Err(error) => Err(error.to_string()),
                    }
                }
                Err(error) => Err(format!("invalid find arguments: {error}")),
            },
            "grep" => match serde_json::from_str::<GrepArgs>(arguments) {
                Ok(args) => {
                    let tools = self.clone();
                    let cancel = cancel.clone();
                    match tokio::task::spawn_blocking(move || tools.grep(args, &cancel)).await {
                        Ok(result) => result,
                        Err(error) => Err(error.to_string()),
                    }
                }
                Err(error) => Err(format!("invalid grep arguments: {error}")),
            },
            "write" => serde_json::from_str::<WriteArgs>(arguments)
                .map_err(|error| format!("invalid write arguments: {error}"))
                .and_then(|args| self.write(args)),
            "edit" => serde_json::from_str::<EditArgs>(arguments)
                .map_err(|error| format!("invalid edit arguments: {error}"))
                .and_then(|args| self.edit(args)),
            "bash" => match serde_json::from_str::<BashArgs>(arguments) {
                Ok(args) if !cfg!(windows) => self.shell(args, "bash", cancel, tool_id, live).await,
                Ok(_) => Err("bash requires a Bash shell; use powershell on Windows".into()),
                Err(error) => Err(format!("invalid bash arguments: {error}")),
            },
            "powershell" => match serde_json::from_str::<BashArgs>(arguments) {
                Ok(args) if cfg!(windows) => {
                    self.shell(args, "powershell", cancel, tool_id, live).await
                }
                Ok(_) => Err("powershell is available only on Windows".into()),
                Err(error) => Err(format!("invalid powershell arguments: {error}")),
            },
            _ => Err(format!("unknown tool: {name}")),
        };
        match result {
            Ok(value) => value,
            Err(error) => ToolResultOutput::ErrorJson {
                value: serde_json::json!({"error": error}),
            },
        }
    }

    fn read(&self, args: ReadArgs) -> Result<ToolResultOutput, String> {
        let path = self.existing_path(&args.path)?;
        let original = read_text(&path)?;
        let offset = args.offset.unwrap_or(1);
        let limit = args.limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);
        if offset == 0 || limit == 0 {
            return Err("offset and limit must be positive".into());
        }
        let mut output = String::new();
        let mut truncated = false;
        for (read_lines, (index, line)) in original.lines().enumerate().skip(offset - 1).enumerate()
        {
            if read_lines >= limit {
                truncated = true;
                break;
            }
            let rendered = format!("L{}: {line}\n", index + 1);
            if output.len() + rendered.len() > MAX_READ_BYTES {
                truncated = true;
                break;
            }
            output.push_str(&rendered);
        }
        if truncated {
            output.push_str("[output truncated; use offset to continue]\n");
        }
        Ok(ToolResultOutput::Text { value: output })
    }

    fn search_path(&self, path: Option<&str>) -> Result<PathBuf, String> {
        match path {
            None | Some(".") => Ok(self.root.clone()),
            Some(path) => self.existing_path(path),
        }
    }

    fn ls(&self, args: LsArgs) -> Result<ToolResultOutput, String> {
        let path = self.search_path(args.path.as_deref())?;
        if !path.is_dir() {
            return Err("path is not a directory".into());
        }
        let limit = search_limit(args.limit, MAX_LS_ENTRIES)?;
        let mut entries = fs::read_dir(path)
            .map_err(|error| error.to_string())?
            .map(|entry| {
                let entry = entry.map_err(|error| error.to_string())?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_dir()
                {
                    name.push('/');
                }
                Ok(name)
            })
            .collect::<Result<Vec<_>, String>>()?;
        entries.sort_by_key(|name| name.to_lowercase());
        let mut output = String::new();
        let mut truncated = false;
        for (index, entry) in entries.iter().enumerate() {
            if index >= limit || !append_search_line(&mut output, entry) {
                truncated = true;
                break;
            }
        }
        if truncated {
            output.push_str("[results truncated; narrow path or raise limit]\n");
        }
        if output.is_empty() {
            output.push_str("(empty directory)");
        }
        Ok(ToolResultOutput::Text { value: output })
    }

    fn find(&self, args: FindArgs, cancel: &CancellationToken) -> Result<ToolResultOutput, String> {
        if args.pattern.is_empty() {
            return Err("pattern must not be empty".into());
        }
        let path = self.search_path(args.path.as_deref())?;
        if !path.is_dir() {
            return Err("path is not a directory".into());
        }
        let limit = search_limit(args.limit, MAX_FIND_RESULTS)?;
        let glob = Glob::new(&args.pattern)
            .map_err(|error| format!("invalid glob: {error}"))?
            .compile_matcher();
        let match_path = args.pattern.contains('/');
        let mut output = String::new();
        let mut matches = 0;
        let mut truncated = false;
        for entry in workspace_walk(&path).build() {
            if cancel.is_cancelled() {
                return Err("find cancelled".into());
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

    fn grep(&self, args: GrepArgs, cancel: &CancellationToken) -> Result<ToolResultOutput, String> {
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

    fn write(&self, args: WriteArgs) -> Result<ToolResultOutput, String> {
        if args.content.len() as u64 > MAX_FILE_BYTES {
            return Err("file exceeds the 2 MiB write limit".into());
        }
        let path = self.writable_path(&args.path, true)?;
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
        persist_text(&path, args.content.as_bytes(), original.as_deref())?;
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({"path": args.path, "bytes": args.content.len()}),
        })
    }

    fn edit(&self, args: EditArgs) -> Result<ToolResultOutput, String> {
        if args.edits.is_empty() {
            return Err("edits must contain at least one replacement".into());
        }
        let path = self.writable_path(&args.path, false)?;
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
        persist_text(&path, updated.as_bytes(), Some(&original_bytes))?;
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

    async fn shell(
        &self,
        args: BashArgs,
        kind: &str,
        cancel: &CancellationToken,
        tool_id: &str,
        live: Option<&mpsc::Sender<RunEvent>>,
    ) -> Result<ToolResultOutput, String> {
        if args.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        let timeout = args.timeout.unwrap_or(30);
        if timeout == 0 || timeout > MAX_BASH_SECONDS {
            return Err("timeout must be in 1..=120 seconds".into());
        }
        let mut command = if kind == "powershell" {
            let mut command = Command::new("pwsh");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
            ]);
            command
        } else {
            let mut command = Command::new("bash");
            command.arg("-c");
            command
        };
        let command_text = if kind == "powershell" {
            format!(
                "try {{ [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 }} catch {{}}\n{}",
                args.command
            )
        } else {
            args.command
        };
        command
            .arg(&command_text)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if kind == "powershell" && error.kind() == std::io::ErrorKind::NotFound => {
                let mut fallback = Command::new("powershell.exe");
                fallback
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-Command",
                    ])
                    .arg(&command_text)
                    .current_dir(&self.root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                fallback.spawn().map_err(|error| error.to_string())?
            }
            Err(error) if kind == "bash" && error.kind() == std::io::ErrorKind::NotFound => {
                let mut fallback = Command::new("sh");
                fallback
                    .arg("-c")
                    .arg(&command_text)
                    .current_dir(&self.root)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                #[cfg(unix)]
                fallback.process_group(0);
                fallback.spawn().map_err(|error| error.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        let child_id = child.id();
        let stdout = child.stdout.take().ok_or("stdout pipe unavailable")?;
        let stderr = child.stderr.take().ok_or("stderr pipe unavailable")?;
        let stdout_task = tokio::spawn(read_bounded(
            stdout,
            tool_id.to_owned(),
            "stdout",
            live.cloned(),
        ));
        let stderr_task = tokio::spawn(read_bounded(
            stderr,
            tool_id.to_owned(),
            "stderr",
            live.cloned(),
        ));
        let mut timed_out = false;
        let status = tokio::select! {
            result = child.wait() => result.map_err(|error| error.to_string())?,
            _ = cancel.cancelled() => {
                kill_process_group(child_id);
                let _ = child.kill().await;
                stdout_task.abort();
                stderr_task.abort();
                return Err("command cancelled; effects may have occurred".into());
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                timed_out = true;
                kill_process_group(child_id);
                let _ = child.kill().await;
                child.wait().await.map_err(|error| error.to_string())?
            }
        };
        // A shell may exit while a background child still holds a pipe open.
        kill_process_group(child_id);
        let (stdout, stdout_truncated) = stdout_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let (stderr, stderr_truncated) = stderr_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({
                "exit_status": status.code(), "stdout": stdout, "stderr": stderr,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated, "timed_out": timed_out
            }),
        })
    }

    fn existing_path(&self, path: &str) -> Result<PathBuf, String> {
        let relative = validate_relative(path)?;
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !path.starts_with(&self.root) {
            return Err("path escapes the workspace".into());
        }
        Ok(path)
    }

    fn writable_path(&self, path: &str, create_parents: bool) -> Result<PathBuf, String> {
        let relative = validate_relative(path)?;
        let candidate = self.root.join(relative);
        if let Ok(metadata) = fs::symlink_metadata(&candidate)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err("target must be a regular file, not a symlink".into());
        }
        let parent = candidate.parent().ok_or("path has no parent")?;
        if create_parents {
            let mut existing = parent;
            while !existing.exists() {
                existing = existing.parent().ok_or("path has no existing ancestor")?;
            }
            if !existing
                .canonicalize()
                .map_err(|error| error.to_string())?
                .starts_with(&self.root)
            {
                return Err("path escapes the workspace".into());
            }
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let parent = parent.canonicalize().map_err(|error| error.to_string())?;
        if !parent.starts_with(&self.root) {
            return Err("path escapes the workspace".into());
        }
        let name = candidate.file_name().ok_or("path has no filename")?;
        let path = parent.join(name);
        if !create_parents && !path.is_file() {
            return Err("file does not exist".into());
        }
        Ok(path)
    }
}

fn read_text(path: &Path) -> Result<String, String> {
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

fn persist_text(path: &Path, updated: &[u8], original: Option<&[u8]>) -> Result<(), String> {
    let parent = path.parent().ok_or("path has no parent")?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    temporary
        .write_all(updated)
        .map_err(|error| error.to_string())?;
    if original.is_some() {
        let permissions = fs::metadata(path)
            .map_err(|error| error.to_string())?
            .permissions();
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| error.to_string())?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    match original {
        Some(bytes) => {
            if fs::read(path).map_err(|error| error.to_string())? != bytes {
                return Err("file changed while preparing the edit".into());
            }
            temporary
                .persist(path)
                .map_err(|error| error.error.to_string())?;
        }
        None => {
            temporary
                .persist_noclobber(path)
                .map_err(|error| error.error.to_string())?;
        }
    }
    Ok(())
}

fn search_limit(requested: Option<usize>, default: usize) -> Result<usize, String> {
    let limit = requested.unwrap_or(default);
    if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(format!("limit must be in 1..={MAX_SEARCH_LIMIT}"));
    }
    Ok(limit)
}

fn append_search_line(output: &mut String, line: &str) -> bool {
    if output.len() + line.len() + 1 > MAX_READ_BYTES {
        return false;
    }
    output.push_str(line);
    output.push('\n');
    true
}

fn workspace_walk(path: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(path);
    builder.hidden(false).follow_links(false).require_git(false);
    builder.filter_entry(|entry| entry.path().file_name().is_none_or(|name| name != ".git"));
    builder
}

fn validate_relative(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("path must be a nonempty workspace-relative path".into());
    }
    Ok(path.to_path_buf())
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid
        && let Ok(raw) = i32::try_from(pid)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    tool_id: String,
    source: &'static str,
    live: Option<mpsc::Sender<RunEvent>>,
) -> std::io::Result<(String, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut live_chunks = 0;
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        let take = read.min(remaining);
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < read;
        if take > 0
            && live_chunks < MAX_LIVE_CHUNKS
            && let Some(sender) = &live
        {
            let _ = sender
                .send(RunEvent::ToolOutputDelta {
                    id: tool_id.clone(),
                    source: source.into(),
                    text: String::from_utf8_lossy(&chunk[..take]).into_owned(),
                })
                .await;
            live_chunks += 1;
        }
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn success(output: ToolResultOutput) -> Result<ToolResultOutput, String> {
        if let ToolResultOutput::ErrorJson { value } = &output {
            return Err(value.to_string());
        }
        Ok(output)
    }

    #[tokio::test]
    async fn write_creates_parents_and_edit_applies_original_ranges()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path())?;
        let cancel = CancellationToken::new();
        success(
            tools
                .execute(
                    "write",
                    r#"{"path":"src/note.txt","content":"\uFEFFone\r\ntwo\r\nthree\r\n"}"#,
                    &cancel,
                    "write-1",
                    None,
                )
                .await,
        )?;
        let path = workspace.path().join("src/note.txt");
        success(tools.execute(
            "edit",
            r#"{"path":"src/note.txt","edits":[{"oldText":"three","newText":"THREE"},{"oldText":"one\ntwo","newText":"ONE\nTWO"}]}"#,
            &cancel, "edit-1", None,
        ).await)?;
        assert_eq!(fs::read_to_string(path)?, "\u{feff}ONE\r\nTWO\r\nTHREE\r\n");
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_or_overlapping_edits_leave_file_intact()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let path = workspace.path().join("note.txt");
        fs::write(&path, "abc abc\n")?;
        let tools = WorkspaceTools::new(workspace.path())?;
        let cancel = CancellationToken::new();
        for edits in [
            serde_json::json!([{"oldText": "abc", "newText": "x"}]),
            serde_json::json!([
                {"oldText": "abc abc", "newText": "x"},
                {"oldText": "abc ", "newText": "y"}
            ]),
            serde_json::json!([
                {"oldText": "abc abc", "newText": "x"},
                {"oldText": "missing", "newText": "y"}
            ]),
        ] {
            let args = serde_json::json!({"path": "note.txt", "edits": edits}).to_string();
            assert!(
                tools
                    .execute("edit", &args, &cancel, "edit", None)
                    .await
                    .is_error()
            );
            assert_eq!(fs::read_to_string(&path)?, "abc abc\n");
        }
        Ok(())
    }

    #[tokio::test]
    async fn inspection_tools_respect_ignore_limits_and_workspace_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        fs::create_dir(workspace.path().join("src"))?;
        fs::write(workspace.path().join(".gitignore"), "ignored.txt\n")?;
        fs::write(workspace.path().join("src/main.rs"), "alpha\nbeta alpha\n")?;
        fs::write(workspace.path().join("ignored.txt"), "alpha\n")?;
        let tools = WorkspaceTools::new(workspace.path())?;
        let cancel = CancellationToken::new();
        let ls = success(tools.execute("ls", "{}", &cancel, "ls", None).await)?;
        assert!(
            matches!(ls, ToolResultOutput::Text { value } if value.contains(".gitignore") && value.contains("src/"))
        );
        let find = success(
            tools
                .execute("find", r#"{"pattern":"*.rs"}"#, &cancel, "find", None)
                .await,
        )?;
        assert!(matches!(find, ToolResultOutput::Text { value } if value == "src/main.rs\n"));
        let grep = success(
            tools
                .execute(
                    "grep",
                    r#"{"pattern":"alpha","glob":"*.rs"}"#,
                    &cancel,
                    "grep",
                    None,
                )
                .await,
        )?;
        assert!(
            matches!(grep, ToolResultOutput::Text { value } if value == "src/main.rs:1: alpha\nsrc/main.rs:2: beta alpha\n")
        );
        let ignored = success(
            tools
                .execute(
                    "grep",
                    r#"{"pattern":"alpha","glob":"*.txt"}"#,
                    &cancel,
                    "grep",
                    None,
                )
                .await,
        )?;
        assert!(matches!(ignored, ToolResultOutput::Text { value } if value == "No matches found"));
        let capped = success(
            tools
                .execute(
                    "grep",
                    r#"{"pattern":"alpha","limit":1}"#,
                    &cancel,
                    "grep",
                    None,
                )
                .await,
        )?;
        assert!(
            matches!(capped, ToolResultOutput::Text { value } if value.contains("[matches truncated") && !value.contains("src/main.rs:2:"))
        );
        assert!(
            tools
                .execute("ls", r#"{"path":"../"}"#, &cancel, "escape", None)
                .await
                .is_error()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn search_does_not_follow_symlinks_outside_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let outside = TempDir::new()?;
        fs::write(outside.path().join("secret.txt"), "do not return this\n")?;
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link"))?;
        let tools = WorkspaceTools::new(workspace.path())?;
        let cancel = CancellationToken::new();
        assert!(
            tools
                .execute(
                    "grep",
                    r#"{"pattern":"secret","path":"link"}"#,
                    &cancel,
                    "grep",
                    None
                )
                .await
                .is_error()
        );
        let find = success(
            tools
                .execute("find", r#"{"pattern":"*.txt"}"#, &cancel, "find", None)
                .await,
        )?;
        assert!(
            matches!(find, ToolResultOutput::Text { value } if value == "No files found matching pattern")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_parent_symlink_escape() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let outside = TempDir::new()?;
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link"))?;
        let tools = WorkspaceTools::new(workspace.path())?;
        let output = tools
            .execute(
                "write",
                r#"{"path":"link/nested/file.txt","content":"escape"}"#,
                &CancellationToken::new(),
                "write",
                None,
            )
            .await;
        assert!(output.is_error());
        assert!(!outside.path().join("nested").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_emits_output_before_exit() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path())?;
        let (sender, mut receiver) = mpsc::channel(64);
        let command = tokio::spawn(async move {
            tools
                .execute(
                    "bash",
                    r#"{"command":"printf first; sleep 0.2; printf second"}"#,
                    &CancellationToken::new(),
                    "bash-1",
                    Some(&sender),
                )
                .await
        });
        let first = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await?
            .ok_or("missing live output")?;
        assert!(matches!(first, RunEvent::ToolOutputDelta { text, .. } if text.contains("first")));
        assert!(!command.is_finished());
        let result = success(command.await?)?;
        assert!(
            matches!(result, ToolResultOutput::Json { value } if value["stdout"] == "firstsecond")
        );
        Ok(())
    }
}

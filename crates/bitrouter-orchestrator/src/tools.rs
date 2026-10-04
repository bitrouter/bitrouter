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
use crate::store::EffectStatus;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_READ_BYTES: usize = 50 * 1024;
const MAX_READ_LINES: usize = 2_000;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_LIVE_CHUNKS: usize = 8_192;
const MAX_SHELL_SECONDS: u64 = 120;
const MAX_SEARCH_LIMIT: usize = 2_000;
const DEFAULT_DIRECTORY_ENTRIES: usize = 500;
const DEFAULT_GLOB_RESULTS: usize = 1_000;
const MAX_GREP_MATCHES: usize = 100;
const MAX_GREP_LINE_CHARS: usize = 500;

#[derive(Clone)]
pub(crate) struct WorkspaceTools {
    root: PathBuf,
    mode: ToolMode,
    interpreter: Option<Interpreter>,
    #[cfg(test)]
    read_gate: Option<std::sync::Arc<ReadGate>>,
}

/// Blocks inside real file workers so scheduler tests can establish overlap and
/// cleanup without relying on filesystem speed or special blocking files.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ReadGate {
    state: std::sync::Mutex<ReadGateState>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct ReadGateState {
    entered: Vec<String>,
    allowed: std::collections::HashSet<String>,
    release_all: bool,
    active: usize,
    peak: usize,
}

#[cfg(test)]
impl ReadGate {
    fn enter(&self, arguments: &str) -> Result<(), String> {
        let label = serde_json::from_str::<serde_json::Value>(arguments)
            .map_err(|error| error.to_string())?
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.entered.push(label.clone());
        state.active += 1;
        state.peak = state.peak.max(state.active);
        while !state.release_all && !state.allowed.contains(&label) {
            state = self
                .changed
                .wait(state)
                .map_err(|error| error.to_string())?;
        }
        state.active -= 1;
        Ok(())
    }

    pub(crate) fn entered(&self) -> Result<(Vec<String>, usize), String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok((state.entered.clone(), state.peak))
    }

    pub(crate) fn allow(&self, label: Option<&str>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        match label {
            Some(label) => {
                state.allowed.insert(label.into());
            }
            None => state.release_all = true,
        }
        self.changed.notify_all();
        Ok(())
    }
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
struct ShellArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
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

#[derive(Clone)]
struct Interpreter {
    executable: PathBuf,
    dialect: &'static str,
}

impl Interpreter {
    fn discover() -> std::io::Result<Self> {
        #[cfg(windows)]
        let candidates = [("pwsh.exe", "powershell"), ("powershell.exe", "powershell")];
        #[cfg(not(windows))]
        let candidates = [("bash", "bash"), ("sh", "sh")];
        Self::search(std::env::var_os("PATH").as_deref(), &candidates)
    }

    fn search(
        search_path: Option<&std::ffi::OsStr>,
        candidates: &[(&str, &'static str)],
    ) -> std::io::Result<Self> {
        if let Some(search_path) = search_path {
            for (name, dialect) in candidates {
                for directory in std::env::split_paths(search_path) {
                    let path = directory.join(name);
                    let Ok(metadata) = fs::metadata(&path) else {
                        continue;
                    };
                    if !metadata.is_file() {
                        continue;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if metadata.permissions().mode() & 0o111 == 0 {
                            continue;
                        }
                    }
                    return Ok(Self {
                        executable: path.canonicalize()?,
                        dialect,
                    });
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no supported shell interpreter is available in the server PATH",
        ))
    }

    fn identity(&self) -> serde_json::Value {
        serde_json::json!({"executable": self.executable, "dialect": self.dialect})
    }
}

#[derive(Clone, Copy)]
enum ToolKind {
    Read,
    Glob,
    Grep,
    Write,
    Edit,
    Shell,
}

const TOOL_KINDS: [ToolKind; 6] = [
    ToolKind::Read,
    ToolKind::Glob,
    ToolKind::Grep,
    ToolKind::Write,
    ToolKind::Edit,
    ToolKind::Shell,
];

enum ToolArgs {
    Read(ReadArgs),
    Glob(GlobArgs),
    Grep(GrepArgs),
    Write(WriteArgs),
    Edit(EditArgs),
    Shell(ShellArgs),
}

impl ToolKind {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Shell => "shell",
        }
    }

    fn lookup(name: &str) -> Result<Self, String> {
        TOOL_KINDS
            .into_iter()
            .find(|kind| kind.name() == name)
            .ok_or_else(|| format!("unknown tool: {name}"))
    }

    fn read_only(self) -> bool {
        matches!(self, Self::Read | Self::Glob | Self::Grep)
    }

    fn parse(self, arguments: &str) -> Result<ToolArgs, String> {
        fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, String> {
            serde_json::from_str(arguments)
                .map_err(|error| format!("invalid tool arguments: {error}"))
        }
        let args = match self {
            Self::Read => ToolArgs::Read(parse(arguments)?),
            Self::Glob => ToolArgs::Glob(parse(arguments)?),
            Self::Grep => ToolArgs::Grep(parse(arguments)?),
            Self::Write => ToolArgs::Write(parse(arguments)?),
            Self::Edit => ToolArgs::Edit(parse(arguments)?),
            Self::Shell => ToolArgs::Shell(parse(arguments)?),
        };
        match &args {
            ToolArgs::Read(args) => {
                if args.offset == Some(0) {
                    return Err("offset must be positive".into());
                }
                search_limit(args.limit, MAX_READ_LINES)?;
                if args.path != "." {
                    validate_relative(&args.path)?;
                }
            }
            ToolArgs::Glob(args) => {
                if args.pattern.is_empty() {
                    return Err("pattern must not be empty".into());
                }
                Glob::new(&args.pattern).map_err(|error| format!("invalid glob: {error}"))?;
                search_limit(args.limit, DEFAULT_GLOB_RESULTS)?;
            }
            ToolArgs::Grep(args) => {
                if args.pattern.is_empty() {
                    return Err("pattern must not be empty".into());
                }
                search_limit(args.limit, MAX_GREP_MATCHES)?;
            }
            ToolArgs::Write(args) => {
                validate_relative(&args.path)?;
                if args.content.len() as u64 > MAX_FILE_BYTES {
                    return Err("file exceeds the 2 MiB write limit".into());
                }
            }
            ToolArgs::Edit(args) => {
                validate_relative(&args.path)?;
                if args.edits.is_empty() || args.edits.iter().any(|edit| edit.old_text.is_empty()) {
                    return Err("edits require nonempty oldText replacements".into());
                }
            }
            ToolArgs::Shell(args) => {
                if args.command.trim().is_empty() {
                    return Err("command must not be empty".into());
                }
                if !(1..=MAX_SHELL_SECONDS).contains(&args.timeout.unwrap_or(30)) {
                    return Err("timeout must be in 1..=120 seconds".into());
                }
            }
        }
        Ok(args)
    }

    fn schema(self) -> serde_json::Value {
        match self {
            Self::Read => serde_json::json!({"type":"object", "properties":{
                "path":{"type":"string","minLength":1}, "offset":{"type":"integer","minimum":1},
                "limit":{"type":"integer","minimum":1,"maximum":2000}},
                "required":["path"],"additionalProperties":false}),
            Self::Glob => serde_json::json!({"type":"object", "properties":{
                "pattern":{"type":"string"},"path":{"type":"string"},
                "limit":{"type":"integer","minimum":1,"maximum":2000}},
                "required":["pattern"],"additionalProperties":false}),
            Self::Grep => serde_json::json!({"type":"object", "properties":{
                "pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},
                "ignoreCase":{"type":"boolean"},"literal":{"type":"boolean"},
                "limit":{"type":"integer","minimum":1,"maximum":2000}},
                "required":["pattern"],"additionalProperties":false}),
            Self::Write => serde_json::json!({"type":"object", "properties":{
                "path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"],"additionalProperties":false}),
            Self::Edit => serde_json::json!({"type":"object", "properties":{
                "path":{"type":"string"},"edits":{"type":"array","minItems":1,"items":{
                    "type":"object","properties":{"oldText":{"type":"string"},"newText":{"type":"string"}},
                    "required":["oldText","newText"],"additionalProperties":false}}},
                "required":["path","edits"],"additionalProperties":false}),
            Self::Shell => serde_json::json!({"type":"object", "properties":{
                "command":{"type":"string","minLength":1},"timeout":{"type":"integer","minimum":1,"maximum":120}},
                "required":["command"],"additionalProperties":false}),
        }
    }

    fn description(self, interpreter: Option<&Interpreter>) -> String {
        match self {
            Self::Read => "Read a workspace UTF-8 file or list one directory. Use path '.' for the root. offset is one-based; offset and limit count file lines or directory entries. Directories include hidden and ignored children. Output is limited to 50 KiB; use the returned offset to continue.".into(),
            Self::Glob => "Find workspace files and directories by glob, respecting project ignore rules. Patterns without '/' match basenames recursively; patterns with '/' match relative to path. Returns workspace-relative paths; default 1000 results, maximum 2000, bounded output.".into(),
            Self::Grep => "Search workspace text files by regex, respecting .gitignore. Returns path:line matches, at most 100 matches or 50 KiB by default. Set literal for exact text.".into(),
            Self::Write => "Create or overwrite a UTF-8 workspace file, creating parent directories as needed.".into(),
            Self::Edit => "Edit one existing workspace file. Each nonempty oldText must match exactly one non-overlapping region of the original file; all edits apply together.".into(),
            Self::Shell => {
                let selected = interpreter.map_or_else(|| "unavailable".into(), |shell| {
                    let dialect = match shell.dialect { "bash" => "Bash", "sh" => "POSIX sh", _ => "PowerShell" };
                    format!("{dialect} at {}", shell.executable.display())
                });
                format!("Run a noninteractive command in the server workspace. Uses {selected} on the server. Commands can affect paths outside the workspace. Returns exit status and bounded stdout/stderr. timeout is in seconds (default 30, maximum 120).")
            }
        }
    }
}

impl WorkspaceTools {
    pub(crate) fn new(root: &Path, mode: ToolMode) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "workspace is not a directory",
            ));
        }
        let interpreter = if mode == ToolMode::Coding {
            Some(Interpreter::discover()?)
        } else {
            None
        };
        Ok(Self {
            root,
            mode,
            interpreter,
            #[cfg(test)]
            read_gate: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_read_gate(&mut self, gate: std::sync::Arc<ReadGate>) {
        self.read_gate = Some(gate);
    }

    pub(crate) fn allowed(mode: ToolMode, name: &str) -> bool {
        ToolKind::lookup(name).is_ok_and(|kind| kind.read_only() || mode == ToolMode::Coding)
    }

    pub(crate) fn read_only(name: &str) -> bool {
        ToolKind::lookup(name).is_ok_and(ToolKind::read_only)
    }

    pub(crate) fn validate(name: &str, arguments: &str) -> Result<(), String> {
        ToolKind::lookup(name)?.parse(arguments).map(|_| ())
    }

    pub(crate) fn declarations(&self) -> Vec<Tool> {
        TOOL_KINDS
            .into_iter()
            .filter(|kind| Self::allowed(self.mode, kind.name()))
            .map(|kind| Tool::Function {
                name: kind.name().into(),
                description: Some(kind.description(self.interpreter.as_ref())),
                parameters: kind.schema(),
                strict: Some(false),
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
        self.execute_with_effect(name, arguments, cancel, tool_id, live)
            .await
            .0
    }

    pub(crate) async fn execute_with_effect(
        &self,
        name: &str,
        arguments: &str,
        cancel: &CancellationToken,
        tool_id: &str,
        live: Option<&mpsc::Sender<RunEvent>>,
    ) -> (ToolResultOutput, EffectStatus) {
        let mut effect = EffectStatus::NotExecuted;
        let result = if Self::allowed(self.mode, name) {
            match ToolKind::lookup(name).and_then(|kind| kind.parse(arguments)) {
                Ok(ToolArgs::Read(args)) => {
                    let tools = self.clone();
                    #[cfg(test)]
                    let arguments = arguments.to_owned();
                    let cancel = cancel.clone();
                    tokio::task::spawn_blocking(move || {
                        if cancel.is_cancelled() {
                            return Err("cancelled before reading".into());
                        }
                        #[cfg(test)]
                        if let Some(gate) = &tools.read_gate {
                            gate.enter(&arguments)?;
                        }
                        tools.read(args)
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result)
                }
                Ok(ToolArgs::Glob(args)) => {
                    let tools = self.clone();
                    let cancel = cancel.clone();
                    tokio::task::spawn_blocking(move || tools.glob(args, &cancel))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                }
                Ok(ToolArgs::Grep(args)) => {
                    let tools = self.clone();
                    let cancel = cancel.clone();
                    tokio::task::spawn_blocking(move || tools.grep(args, &cancel))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                }
                Ok(ToolArgs::Write(args)) => self.write(args, &mut effect),
                Ok(ToolArgs::Edit(args)) => self.edit(args, &mut effect),
                Ok(ToolArgs::Shell(args)) => {
                    self.shell(args, cancel, tool_id, live, &mut effect).await
                }
                Err(error) => Err(error),
            }
        } else {
            Err(format!("tool is unavailable in this task mode: {name}"))
        };
        match result {
            Ok(value) => (
                value,
                if effect == EffectStatus::NotExecuted {
                    EffectStatus::Completed
                } else {
                    effect
                },
            ),
            Err(error) => (
                ToolResultOutput::ErrorJson {
                    value: serde_json::json!({"error":error}),
                },
                effect,
            ),
        }
    }

    fn read(&self, args: ReadArgs) -> Result<ToolResultOutput, String> {
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

    fn search_path(&self, path: Option<&str>) -> Result<PathBuf, String> {
        match path {
            None | Some(".") => Ok(self.root.clone()),
            Some(path) => self.existing_path(path),
        }
    }

    fn glob(&self, args: GlobArgs, cancel: &CancellationToken) -> Result<ToolResultOutput, String> {
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

    fn write(
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

    fn edit(&self, args: EditArgs, effect: &mut EffectStatus) -> Result<ToolResultOutput, String> {
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

    async fn shell(
        &self,
        args: ShellArgs,
        cancel: &CancellationToken,
        tool_id: &str,
        live: Option<&mpsc::Sender<RunEvent>>,
        effect: &mut EffectStatus,
    ) -> Result<ToolResultOutput, String> {
        if args.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        let timeout = args.timeout.unwrap_or(30);
        if timeout == 0 || timeout > MAX_SHELL_SECONDS {
            return Err("timeout must be in 1..=120 seconds".into());
        }
        let interpreter = self
            .interpreter
            .as_ref()
            .ok_or("shell interpreter unavailable")?;
        let mut command = Command::new(&interpreter.executable);
        let command_text = if interpreter.dialect == "powershell" {
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
            ]);
            format!(
                "try {{ [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 }} catch {{}}\n{}",
                args.command
            )
        } else {
            command.arg("-c");
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
        // The declared interpreter is fixed. Never retry a generated command
        // in another dialect after spawn failure or effects.
        let mut child = spawn_shell(command)
            .map_err(|error| format!("cannot launch declared shell: {error}"))?;
        // A launched command may already have changed external state.
        *effect = EffectStatus::Unknown;
        let child_id = child.id();
        #[cfg(unix)]
        let mut process_group = ProcessGroupGuard(child_id);
        #[cfg(not(windows))]
        let stdout = child.stdout.take().ok_or("stdout pipe unavailable")?;
        #[cfg(windows)]
        let stdout = child.stdout().take().ok_or("stdout pipe unavailable")?;
        #[cfg(not(windows))]
        let stderr = child.stderr.take().ok_or("stderr pipe unavailable")?;
        #[cfg(windows)]
        let stderr = child.stderr().take().ok_or("stderr pipe unavailable")?;
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
            result = wait_shell(&mut child) => result.map_err(|error| error.to_string())?,
            _ = cancel.cancelled() => {
                let cleanup = kill_shell(&mut child).await;
                stdout_task.abort();
                stderr_task.abort();
                cleanup.map_err(|error| format!("command cleanup failed: {error}"))?;
                #[cfg(unix)]
                { process_group.0 = None; }
                return Err("command cancelled; effects may have occurred".into());
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                timed_out = true;
                kill_shell(&mut child).await.map_err(|error| error.to_string())?;
                wait_shell(&mut child).await.map_err(|error| error.to_string())?
            }
        };
        // A shell may exit while a background child still holds a pipe open.
        stop_shell_descendants(&mut child, child_id)
            .await
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            process_group.0 = None;
        }
        let (stdout, stdout_truncated) = stdout_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let (stderr, stderr_truncated) = stderr_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        if !timed_out {
            *effect = EffectStatus::Completed;
        }
        Ok(ToolResultOutput::Json {
            value: serde_json::json!({
                "interpreter": interpreter.identity(),
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

    fn writable_path(
        &self,
        path: &str,
        create_parents: bool,
        effect: &mut EffectStatus,
    ) -> Result<PathBuf, String> {
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
            if !parent.exists() {
                *effect = EffectStatus::Unknown;
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
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

fn sort_directory_entries(entries: &mut [(String, &str)]) {
    entries.sort_by_cached_key(|(name, _)| (name.to_lowercase(), name.clone()));
}

fn directory_name(name: std::ffi::OsString) -> Result<String, String> {
    name.into_string()
        .map_err(|_| "directory contains a non-UTF-8 name".into())
}

fn read_page(
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

fn persist_text(
    path: &Path,
    updated: &[u8],
    original: Option<&[u8]>,
    effect: &mut EffectStatus,
) -> Result<(), String> {
    let parent = path.parent().ok_or("path has no parent")?;
    *effect = EffectStatus::Unknown;
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

#[cfg(windows)]
type ShellChild = Box<dyn process_wrap::tokio::ChildWrapper>;
#[cfg(not(windows))]
type ShellChild = tokio::process::Child;

#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}

fn spawn_shell(command: Command) -> std::io::Result<ShellChild> {
    #[cfg(windows)]
    {
        use process_wrap::tokio::{CommandWrap, JobObject, KillOnDrop};
        // Suspend during assignment so even immediate descendants belong to
        // the job; KillOnDrop also covers errors and dropped execution futures.
        CommandWrap::from(command)
            .wrap(KillOnDrop)
            .wrap(JobObject)
            .spawn()
    }
    #[cfg(not(windows))]
    {
        let mut command = command;
        command.spawn()
    }
}

async fn wait_shell(child: &mut ShellChild) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        // Wait only for the shell. JobObject's wait includes descendants,
        // which must be terminated before draining inherited output pipes.
        child.inner_mut().wait().await
    }
    #[cfg(not(windows))]
    child.wait().await
}

async fn kill_shell(child: &mut ShellChild) -> std::io::Result<()> {
    let child_id = child.id();
    stop_shell_descendants(child, child_id).await?;
    #[cfg(windows)]
    {
        Ok(())
    }
    #[cfg(not(windows))]
    child.kill().await
}

async fn stop_shell_descendants(
    _child: &mut ShellChild,
    _child_id: Option<u32>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        _child.start_kill()?;
        // The task is terminal only after every job process has exited.
        _child.wait().await?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        kill_process_group(_child_id);
        Ok(())
    }
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

#[cfg(not(any(unix, windows)))]
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

    fn text_output(output: ToolResultOutput) -> Result<String, Box<dyn std::error::Error>> {
        match success(output)? {
            ToolResultOutput::Text { value } => Ok(value),
            _ => Err("expected text output".into()),
        }
    }

    #[tokio::test]
    async fn registry_profiles_reject_legacy_names_and_invalid_arguments()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        for mode in [ToolMode::ReadOnly, ToolMode::Coding] {
            let tools = WorkspaceTools::new(workspace.path(), mode)?;
            let declarations = tools.declarations();
            let names: Vec<_> = declarations
                .iter()
                .filter_map(|tool| match tool {
                    Tool::Function { name, .. } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                names,
                if mode == ToolMode::Coding {
                    vec!["read", "glob", "grep", "write", "edit", "shell"]
                } else {
                    vec!["read", "glob", "grep"]
                }
            );
            for name in ["ls", "find", "bash", "powershell"] {
                assert!(!WorkspaceTools::allowed(mode, name));
                assert!(WorkspaceTools::validate(name, "{}").is_err());
                let (output, effect) = tools
                    .execute_with_effect(name, "{}", &CancellationToken::new(), "legacy", None)
                    .await;
                assert!(output.is_error());
                assert_eq!(effect, EffectStatus::NotExecuted);
            }
            if mode == ToolMode::ReadOnly {
                assert!(tools.interpreter.is_none());
                assert!(
                    tools
                        .execute(
                            "write",
                            r#"{"path":"created","content":"bad"}"#,
                            &CancellationToken::new(),
                            "forged",
                            None
                        )
                        .await
                        .is_error()
                );
                assert!(!workspace.path().join("created").exists());
            } else {
                let shell = tools.interpreter.as_ref().ok_or("missing interpreter")?;
                let Tool::Function {
                    description: Some(description),
                    ..
                } = &declarations[5]
                else {
                    return Err("shell description missing".into());
                };
                assert!(description.contains(&shell.executable.display().to_string()));
            }
        }
        for args in [
            r#"{"path":".","offset":0}"#,
            r#"{"path":".","limit":0}"#,
            r#"{"path":".","limit":2001}"#,
            r#"{"path":".","offset":-1}"#,
            r#"{"path":".","offset":1.5}"#,
            r#"{"path":".","offset":18446744073709551616}"#,
            r#"{"path":".","mode":"directory"}"#,
            "{}",
            r#"{"path":""}"#,
        ] {
            assert!(WorkspaceTools::validate("read", args).is_err(), "{args}");
        }
        for args in [
            r#"{"command":" "}"#,
            r#"{"command":"echo hi","timeout":0}"#,
            r#"{"command":"echo hi","timeout":121}"#,
            r#"{"command":"echo hi","interpreter":"bash"}"#,
        ] {
            assert!(WorkspaceTools::validate("shell", args).is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn read_directory_pages_include_ignored_entries_and_escape_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        fs::create_dir(workspace.path().join("src"))?;
        fs::create_dir(workspace.path().join(".git"))?;
        fs::write(workspace.path().join(".gitignore"), "ignored.txt\n")?;
        fs::write(workspace.path().join("ignored.txt"), "hidden from search")?;
        fs::write(workspace.path().join("src/note.txt"), "one\ntwo\nthree\n")?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
        let cancel = CancellationToken::new();
        let first = text_output(
            tools
                .execute("read", r#"{"path":".","limit":2}"#, &cancel, "page1", None)
                .await,
        )?;
        assert_eq!(
            first,
            "Directory \".\"\nE1: directory \".git/\"\nE2: file \".gitignore\"\n[output truncated; continue with offset=3]\n"
        );
        let second = text_output(
            tools
                .execute(
                    "read",
                    r#"{"path":".","offset":3,"limit":2}"#,
                    &cancel,
                    "page2",
                    None,
                )
                .await,
        )?;
        assert_eq!(
            second,
            "Directory \".\"\nE3: file \"ignored.txt\"\nE4: directory \"src/\"\n"
        );
        let file = text_output(
            tools
                .execute(
                    "read",
                    r#"{"path":"src/note.txt","offset":2,"limit":1}"#,
                    &cancel,
                    "file",
                    None,
                )
                .await,
        )?;
        assert_eq!(
            file,
            "File \"src/note.txt\"\nL2: two\n[output truncated; continue with offset=3]\n"
        );
        for path in [".", "src/note.txt"] {
            let result = text_output(
                tools
                    .execute(
                        "read",
                        &serde_json::json!({"path":path,"offset":100}).to_string(),
                        &cancel,
                        "end",
                        None,
                    )
                    .await,
            )?;
            assert!(result.ends_with("(end of input)\n"));
            assert!(!result.contains("truncated"));
        }
        fs::create_dir(workspace.path().join("empty"))?;
        fs::write(workspace.path().join("empty.txt"), "")?;
        for (path, message) in [
            ("empty", "(empty directory)"),
            ("empty.txt", "(empty file)"),
        ] {
            assert!(
                text_output(
                    tools
                        .execute(
                            "read",
                            &serde_json::json!({"path":path}).to_string(),
                            &cancel,
                            "empty",
                            None
                        )
                        .await
                )?
                .contains(message)
            );
        }
        let order = text_output(read_page(
            "Directory \"test\"\n".into(),
            vec!["E1: first\n".into(), "E2: second\n".into()].into_iter(),
            1,
            1,
            "empty",
        )?)?;
        assert!(order.ends_with("offset=2]\n"));
        Ok(())
    }

    #[tokio::test]
    async fn read_byte_pages_are_bounded_and_large_records_fail()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        fs::write(
            workspace.path().join("pages.txt"),
            format!("{}\n{}\n", "α".repeat(15000), "β".repeat(15000)),
        )?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
        let cancel = CancellationToken::new();
        let first = text_output(
            tools
                .execute("read", r#"{"path":"pages.txt"}"#, &cancel, "first", None)
                .await,
        )?;
        assert!(first.len() <= MAX_READ_BYTES);
        assert!(first.contains("offset=2]"));
        assert!(!first.contains('β'));
        let second = text_output(
            tools
                .execute(
                    "read",
                    r#"{"path":"pages.txt","offset":2}"#,
                    &cancel,
                    "second",
                    None,
                )
                .await,
        )?;
        assert!(second.len() <= MAX_READ_BYTES);
        assert!(second.contains('β'));
        assert!(!second.contains("truncated"));
        fs::write(
            workspace.path().join("huge.txt"),
            "x".repeat(MAX_READ_BYTES),
        )?;
        assert!(
            tools
                .execute("read", r#"{"path":"huge.txt"}"#, &cancel, "huge", None)
                .await
                .is_error()
        );
        fs::write(workspace.path().join("binary"), [0xff, 0xfe])?;
        assert!(
            tools
                .execute("read", r#"{"path":"binary"}"#, &cancel, "binary", None)
                .await
                .is_error()
        );
        for path in ["../outside", "/", "missing"] {
            assert!(
                tools
                    .execute(
                        "read",
                        &serde_json::json!({"path":path}).to_string(),
                        &cancel,
                        "invalid",
                        None
                    )
                    .await
                    .is_error()
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_case_ties_symlinks_special_files_and_non_utf8_names()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::ffi::OsStringExt;
        let workspace = TempDir::new()?;
        let outside = TempDir::new()?;
        fs::write(workspace.path().join("A"), "inside")?;
        fs::write(workspace.path().join("a"), "inside")?;
        // Some macOS filesystems are case-insensitive. Test the sort rule directly
        // as well as inspecting actual directory entries.
        let mut entries = [
            ("a".into(), "file"),
            ("A".into(), "file"),
            ("b".into(), "file"),
        ];
        sort_directory_entries(&mut entries);
        assert_eq!(entries.map(|(name, _)| name), ["A", "a", "b"]);
        fs::write(workspace.path().join("new\nline"), "escaped")?;
        fs::write(outside.path().join("secret"), "outside")?;
        std::os::unix::fs::symlink(
            workspace.path().join("A"),
            workspace.path().join("inside-link"),
        )?;
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link"))?;
        let socket = std::os::unix::net::UnixListener::bind(workspace.path().join("socket"))?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
        let cancel = CancellationToken::new();
        let result = text_output(
            tools
                .execute("read", r#"{"path":"."}"#, &cancel, "root", None)
                .await,
        )?;
        assert!(result.contains("file \"new\\nline\""));
        assert!(result.contains("symlink \"outside-link\""));
        assert!(!result.contains("outside\n"));
        assert!(
            text_output(
                tools
                    .execute("read", r#"{"path":"inside-link"}"#, &cancel, "link", None)
                    .await
            )?
            .contains("inside")
        );
        for path in ["outside-link", "socket"] {
            assert!(
                tools
                    .execute(
                        "read",
                        &serde_json::json!({"path":path}).to_string(),
                        &cancel,
                        "rejected",
                        None
                    )
                    .await
                    .is_error()
            );
        }
        drop(socket);
        assert!(directory_name(std::ffi::OsString::from_vec(vec![0xff])).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn glob_paths_directories_limits_and_cancellation_keep_search_semantics()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        fs::create_dir_all(workspace.path().join("src/nested"))?;
        fs::write(workspace.path().join("src/main.rs"), "root")?;
        fs::write(workspace.path().join("src/nested/lib.rs"), "nested")?;
        fs::write(workspace.path().join(".visible.rs"), "hidden")?;
        fs::create_dir(workspace.path().join(".git"))?;
        fs::write(workspace.path().join(".git/private.rs"), "exclude")?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::ReadOnly)?;
        let cancel = CancellationToken::new();
        let paths = text_output(
            tools
                .execute("glob", r#"{"pattern":"*.rs"}"#, &cancel, "all", None)
                .await,
        )?;
        for expected in ["src/main.rs", "src/nested/lib.rs", ".visible.rs"] {
            assert!(paths.contains(expected));
        }
        assert!(!paths.contains("private.rs"));
        assert_eq!(
            text_output(
                tools
                    .execute(
                        "glob",
                        r#"{"pattern":"nested/*.rs","path":"src"}"#,
                        &cancel,
                        "relative",
                        None
                    )
                    .await
            )?,
            "src/nested/lib.rs\n"
        );
        assert_eq!(
            text_output(
                tools
                    .execute(
                        "glob",
                        r#"{"pattern":"nested","path":"src"}"#,
                        &cancel,
                        "dir",
                        None
                    )
                    .await
            )?,
            "src/nested/\n"
        );
        let limited = text_output(
            tools
                .execute(
                    "glob",
                    r#"{"pattern":"*.rs","limit":1}"#,
                    &cancel,
                    "limit",
                    None,
                )
                .await,
        )?;
        assert!(limited.contains("results truncated"));
        cancel.cancel();
        assert!(
            tools
                .execute("glob", r#"{"pattern":"*.rs"}"#, &cancel, "cancelled", None)
                .await
                .is_error()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interpreter_availability_fallback_is_declared_and_never_retried()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let workspace = TempDir::new()?;
        let bins = TempDir::new()?;
        let candidates = [("bash", "bash"), ("sh", "sh")];
        assert!(Interpreter::search(None, &candidates).is_err());
        assert!(Interpreter::search(Some(bins.path().as_os_str()), &candidates).is_err());
        let sh = bins.path().join("sh");
        fs::write(&sh, "#!/bin/sh\nprintf fallback > unexpected\n")?;
        fs::set_permissions(&sh, fs::Permissions::from_mode(0o755))?;
        let fallback = Interpreter::search(Some(bins.path().as_os_str()), &candidates)?;
        assert_eq!(fallback.dialect, "sh");
        let bash = bins.path().join("bash");
        fs::write(&bash, "#!/bin/sh\nprintf selected\n")?;
        fs::set_permissions(&bash, fs::Permissions::from_mode(0o755))?;
        let preferred = Interpreter::search(Some(bins.path().as_os_str()), &candidates)?;
        assert_eq!(preferred.dialect, "bash");
        let tools = WorkspaceTools {
            root: workspace.path().canonicalize()?,
            mode: ToolMode::Coding,
            interpreter: Some(preferred),
            read_gate: None,
        };
        let value = success(
            tools
                .execute(
                    "shell",
                    r#"{"command":"echo hi"}"#,
                    &CancellationToken::new(),
                    "selected",
                    None,
                )
                .await,
        )?;
        assert!(
            matches!(value, ToolResultOutput::Json { value } if value["interpreter"]["dialect"] == "bash" && value["stdout"] == "selected")
        );
        fs::remove_file(bash)?;
        let (output, effect) = tools
            .execute_with_effect(
                "shell",
                r#"{"command":"echo hi"}"#,
                &CancellationToken::new(),
                "lost",
                None,
            )
            .await;
        assert!(output.is_error());
        assert_eq!(effect, EffectStatus::NotExecuted);
        assert!(!workspace.path().join("unexpected").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_nonzero_exit_and_output_caps_remain_explicit()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let result = success(
            tools
                .execute(
                    "shell",
                    r#"{"command":"printf out; printf err >&2; exit 7"}"#,
                    &CancellationToken::new(),
                    "exit",
                    None,
                )
                .await,
        )?;
        assert!(
            matches!(result, ToolResultOutput::Json { value } if value["exit_status"] == 7 && value["stdout"] == "out" && value["stderr"] == "err" && value["timed_out"] == false)
        );
        let result = success(
            tools
                .execute(
                    "shell",
                    r#"{"command":"head -c 40000 /dev/zero | tr '\\0' x"}"#,
                    &CancellationToken::new(),
                    "cap",
                    None,
                )
                .await,
        )?;
        let ToolResultOutput::Json { value } = result else {
            return Err("expected shell output".into());
        };
        assert_eq!(
            value["stdout"].as_str().ok_or("stdout")?.len(),
            MAX_OUTPUT_BYTES
        );
        assert_eq!(value["stdout_truncated"], true);
        Ok(())
    }

    #[cfg(unix)]
    async fn assert_unix_descendant_cleanup(
        action: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (sender, mut receiver) = mpsc::channel(64);
        let args = serde_json::json!({"command": "while true; do echo tick >>ticks.txt; sleep 0.05; done & while [ ! -s ticks.txt ]; do sleep 0.01; done; printf ready; wait", "timeout": if action == "timeout" { 1 } else { 30 }}).to_string();
        let task = tokio::spawn(async move {
            tools
                .execute("shell", &args, &task_cancel, "cleanup", Some(&sender))
                .await
        });
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = receiver.recv().await {
                if matches!(event, RunEvent::ToolOutputDelta { text, .. } if text.contains("ready"))
                {
                    return Ok::<(), String>(());
                }
            }
            Err("no readiness output".into())
        })
        .await;
        if !matches!(ready, Ok(Ok(()))) {
            cancel.cancel();
            task.await?;
            return Err("descendant failed to start".into());
        }
        match action {
            "cancel" => cancel.cancel(),
            "drop" => task.abort(),
            _ => {}
        }
        let result = tokio::time::timeout(Duration::from_secs(5), task).await?;
        match action {
            "cancel" => assert!(result?.is_error()),
            "drop" => assert!(result.is_err_and(|error| error.is_cancelled())),
            "timeout" => assert!(
                matches!(success(result?)?, ToolResultOutput::Json { value } if value["timed_out"] == true)
            ),
            _ => return Err("unknown action".into()),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ticks = fs::read(workspace.path().join("ticks.txt"))?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ticks, fs::read(workspace.path().join("ticks.txt"))?);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_cancellation_stops_unix_descendants() -> Result<(), Box<dyn std::error::Error>> {
        assert_unix_descendant_cleanup("cancel").await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_timeout_stops_unix_descendants() -> Result<(), Box<dyn std::error::Error>> {
        assert_unix_descendant_cleanup("timeout").await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_shell_future_stops_unix_descendants() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_unix_descendant_cleanup("drop").await
    }

    #[test]
    fn openai_requests_preserve_optional_tool_arguments() -> Result<(), Box<dyn std::error::Error>>
    {
        use bitrouter_sdk::language_model::protocol::{
            OutboundAdapter, chat_completions::ChatCompletionsAdapter, responses::ResponsesAdapter,
        };

        for mode in [ToolMode::Coding, ToolMode::ReadOnly] {
            let prompt = crate::context::build(
                "fixture",
                None,
                "inspect",
                &[],
                WorkspaceTools::new(TempDir::new()?.path(), mode)?.declarations(),
                512 * 1024,
            )?;
            for adapter in [
                &ChatCompletionsAdapter as &dyn OutboundAdapter,
                &ResponsesAdapter,
            ] {
                let request = adapter.render_request(&prompt)?;
                for tool in request["tools"].as_array().ok_or("missing tools")? {
                    let function = tool.get("function").unwrap_or(tool);
                    assert_eq!(function["strict"], false);
                    let name = function["name"].as_str().ok_or("missing tool name")?;
                    if name == "read" {
                        assert_eq!(
                            function["parameters"]["required"],
                            serde_json::json!(["path"])
                        );
                        assert!(function["parameters"]["properties"].get("offset").is_some());
                    }
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn write_creates_parents_and_edit_applies_original_ranges()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
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
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
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
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let cancel = CancellationToken::new();
        let ls = success(
            tools
                .execute("read", r#"{"path":"."}"#, &cancel, "ls", None)
                .await,
        )?;
        assert!(
            matches!(ls, ToolResultOutput::Text { value } if value.contains(".gitignore") && value.contains("src/"))
        );
        let find = success(
            tools
                .execute("glob", r#"{"pattern":"*.rs"}"#, &cancel, "glob", None)
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
                .execute("read", r#"{"path":"../"}"#, &cancel, "escape", None)
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
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
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
                .execute("glob", r#"{"pattern":"*.txt"}"#, &cancel, "glob", None)
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
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
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
    async fn shell_emits_output_before_exit() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let (sender, mut receiver) = mpsc::channel(64);
        let command = tokio::spawn(async move {
            tools
                .execute(
                    "shell",
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

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_exit_stops_background_descendants() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let result = tokio::time::timeout(Duration::from_secs(5), tools.execute("shell",
            r#"{"command":"while true; do echo tick >>ticks.txt; sleep 0.05; done & while [ ! -s ticks.txt ]; do sleep 0.01; done"}"#,
            &CancellationToken::new(), "shell", None)).await?;
        success(result)?;
        let ticks = std::fs::read(workspace.path().join("ticks.txt"))?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ticks, std::fs::read(workspace.path().join("ticks.txt"))?);
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_nonzero_exit_output_caps_and_streaming()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let cancel = CancellationToken::new();
        let result = success(
            tools
                .execute(
                    "shell",
                    r#"{"command":"[Console]::Write('out'); [Console]::Error.Write('err'); exit 7"}"#,
                    &cancel,
                    "exit",
                    None,
                )
                .await,
        )?;
        assert!(
            matches!(result, ToolResultOutput::Json { value } if value["exit_status"] == 7 && value["stdout"] == "out" && value["stderr"] == "err" && value["timed_out"] == false && value["interpreter"]["dialect"] == "powershell")
        );
        let result = success(
            tools
                .execute(
                    "shell",
                    r#"{"command":"[Console]::Write(('x' * 40000)); [Console]::Error.Write(('y' * 40000))"}"#,
                    &cancel,
                    "cap",
                    None,
                )
                .await,
        )?;
        let ToolResultOutput::Json { value } = result else {
            return Err("expected shell output".into());
        };
        for stream in ["stdout", "stderr"] {
            assert_eq!(
                value[stream].as_str().ok_or("missing stream")?.len(),
                MAX_OUTPUT_BYTES
            );
            assert_eq!(value[format!("{stream}_truncated")], true);
        }
        let (sender, mut receiver) = mpsc::channel(64);
        let command = tokio::spawn(async move {
            tools
                .execute(
                    "shell",
                    r#"{"command":"[Console]::Write('first'); Start-Sleep -Seconds 2; [Console]::Write('second')"}"#,
                    &CancellationToken::new(),
                    "stream",
                    Some(&sender),
                )
                .await
        });
        let first = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await?
            .ok_or("missing live output")?;
        assert!(matches!(first, RunEvent::ToolOutputDelta { text, .. } if text.contains("first")));
        assert!(!command.is_finished());
        assert!(
            matches!(success(command.await?)?, ToolResultOutput::Json { value } if value["stdout"] == "firstsecond")
        );
        Ok(())
    }

    #[cfg(windows)]
    async fn assert_windows_descendant_cleanup(
        action: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        std::fs::write(
            workspace.path().join("child.ps1"),
            "Add-Content ticks.txt tick\n[Console]::WriteLine('ready')\nwhile ($true) { Add-Content ticks.txt tick; Start-Sleep -Milliseconds 50 }",
        )?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (sender, mut receiver) = mpsc::channel(64);
        let command = if action == "exit" {
            // The shell exits normally while its background child owns a file.
            "Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -File child.ps1' -NoNewWindow; while (!(Test-Path ticks.txt)) { Start-Sleep -Milliseconds 20 }; [Console]::WriteLine('ready')"
        } else {
            "& powershell.exe -NoProfile -NonInteractive -File child.ps1"
        };
        let timeout = if action == "timeout" { 3 } else { 30 };
        let task = tokio::spawn(async move {
            tools
                .execute(
                    "shell",
                    &serde_json::json!({"command":command, "timeout":timeout}).to_string(),
                    &task_cancel,
                    "shell",
                    Some(&sender),
                )
                .await
        });
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = receiver.recv().await {
                if matches!(event, RunEvent::ToolOutputDelta {text, ..} if text.contains("ready")) {
                    return Ok::<(), String>(());
                }
            }
            Err("shell exited before descendant started".to_string())
        })
        .await;
        if !matches!(ready, Ok(Ok(()))) {
            cancel.cancel();
            task.await?;
            return Err("descendant did not start".into());
        }
        match action {
            "cancel" => cancel.cancel(),
            "drop" => task.abort(),
            _ => {}
        }
        let result = tokio::time::timeout(Duration::from_secs(10), task).await?;
        match action {
            "cancel" => assert!(result?.is_error()),
            "drop" => assert!(result.is_err_and(|error| error.is_cancelled())),
            "timeout" => assert!(
                matches!(success(result?)?, ToolResultOutput::Json {value} if value["timed_out"] == true)
            ),
            "exit" => {
                success(result?)?;
            }
            _ => return Err("unknown cleanup action".into()),
        }
        let ticks = std::fs::read(workspace.path().join("ticks.txt"))?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ticks, std::fs::read(workspace.path().join("ticks.txt"))?);
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_cancellation_stops_descendants() -> Result<(), Box<dyn std::error::Error>> {
        assert_windows_descendant_cleanup("cancel").await
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_timeout_stops_descendants() -> Result<(), Box<dyn std::error::Error>> {
        assert_windows_descendant_cleanup("timeout").await
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_exit_stops_background_descendants() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_windows_descendant_cleanup("exit").await
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn dropped_powershell_future_stops_descendants() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_windows_descendant_cleanup("drop").await
    }

    #[tokio::test]
    async fn rejected_effectful_tools_report_no_mutation() -> Result<(), Box<dyn std::error::Error>>
    {
        let workspace = TempDir::new()?;
        let tools = WorkspaceTools::new(workspace.path(), ToolMode::Coding)?;
        fs::write(workspace.path().join("note.txt"), "original original")?;
        let cancel = CancellationToken::new();
        let cases = [
            (
                "edit",
                serde_json::json!({"path":"note.txt","edits":[{"oldText":"absent","newText":"changed"}]}),
            ),
            (
                "edit",
                serde_json::json!({"path":"note.txt","edits":[{"oldText":"original","newText":"changed"}]}),
            ),
            (
                "edit",
                serde_json::json!({"path":"missing.txt","edits":[{"oldText":"old","newText":"new"}]}),
            ),
            (
                "write",
                serde_json::json!({"path":"../escape.txt","content":"new"}),
            ),
            ("shell", serde_json::json!({"command":""})),
        ];
        for (name, arguments) in cases {
            let (output, effect) = tools
                .execute_with_effect(name, &arguments.to_string(), &cancel, "rejected", None)
                .await;
            assert!(output.is_error());
            assert_eq!(effect, EffectStatus::NotExecuted);
        }
        assert_eq!(
            fs::read_to_string(workspace.path().join("note.txt"))?,
            "original original"
        );
        assert_eq!(fs::read_dir(workspace.path())?.count(), 1);
        let (_, effect) = tools
            .execute_with_effect(
                "write",
                r#"{"path":"new/note.txt","content":"written"}"#,
                &cancel,
                "written",
                None,
            )
            .await;
        assert_eq!(effect, EffectStatus::Completed);
        Ok(())
    }

    #[test]
    fn persistence_failure_after_temporary_write_retains_uncertainty()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let path = workspace.path().join("note.txt");
        fs::write(&path, "newer content")?;
        let mut effect = EffectStatus::NotExecuted;
        assert!(persist_text(&path, b"replacement", Some(b"stale content"), &mut effect).is_err());
        assert_eq!(effect, EffectStatus::Unknown);
        assert_eq!(fs::read_to_string(&path)?, "newer content");
        Ok(())
    }
}

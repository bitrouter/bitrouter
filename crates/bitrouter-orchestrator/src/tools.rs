use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use bitrouter_sdk::language_model::{Tool, ToolResultOutput};
use globset::Glob;
use ignore::WalkBuilder;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::shell::Interpreter;
use crate::agent::{RunEvent, ToolMode};
use crate::store::EffectStatus;

mod edit;
mod glob;
mod grep;
mod read;
mod shell;
mod write;

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
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

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

    #[cfg(test)]
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

    fn search_path(&self, path: Option<&str>) -> Result<PathBuf, String> {
        match path {
            None | Some(".") => Ok(self.root.clone()),
            Some(path) => self.existing_path(path),
        }
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

#[cfg(test)]
mod tests;

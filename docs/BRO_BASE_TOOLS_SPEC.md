# BRO six base tools

Version: **v0.2**. Updated: **2026-10-01**.

Status: **approved implementation contract; implemented and verified locally on
macOS; Windows declarations and shell execution passed hosted CI.** See the
[acceptance record](BRO_BASE_TOOLS_ACCEPTANCE.md) for checks, real-model evidence,
and the limits of the before/after comparison.

## Scope and authority

Expose `read`, `glob`, `grep`, `write`, `edit`, and `shell` as BRO's native base
tools. Merge directory listing into `read`, rename `find` to `glob`, and expose
one `shell` backed by a server-selected interpreter. Preserve the native
read-only profile without a general command executor.

This is the approved amendment to the tool portions of
[BRO agent runtime MVP](BRO_AGENT_RUNTIME_SPEC.md). Runtime tables and delivered
CLI/skill descriptions now use these six tools. Thread/Turn/Item, permissions,
commit ordering, concurrency, and recovery requirements remain owned by the
runtime spec. This tool change does not establish completion of any runtime
phase.

External ACP harnesses retain their own tool names and behavior. The change
does not rename external tools or cross-harness observation markers. New CLI
flags, interpreter overrides, PTY support, image reading, recursive directory
listing, and new glob syntax are outside this slice.

## Source findings and design rationale

Inspected source: HEAD `b4294b316a319031c09744b9c9a785cbecc65550`, with
uncommitted native-runtime work present on 2026-10-01. These findings describe
that worktree, not a released build.

| Source finding | Proposed change |
| --- | --- |
| `WorkspaceTools` declares eight names, but platform filtering exposes seven per coding task | Expose the same six names on supported Unix and Windows servers |
| `bash` and `powershell` already share an execution function | Move platform selection behind one declared `shell` |
| `read` accepts UTF-8 files; `ls` lists one directory | Dispatch `read` by resolved path type |
| `find` already uses `globset::Glob` | Rename the tool while preserving matching behavior |
| Declaration, validation, permission classification, and dispatch separately match names | Keep these consistent through the runtime's static registry contract |

The expected benefit is a clearer interface with fewer platform-dependent
names. Improved model accuracy, latency, or token cost is a hypothesis, not an
acceptance claim. Merging `read` also makes its description more complex; keep
its parameters small and its two output forms explicit.

Source references: [tool declarations and handlers](../crates/bitrouter-orchestrator/src/tools.rs),
[native instructions and execution](../crates/bitrouter-orchestrator/src/agent.rs),
[verification](../crates/bitrouter-orchestrator/src/service.rs), and
[execution records](../crates/bitrouter-orchestrator/src/store.rs).

## Tool interfaces and permissions

| Tool | Arguments | Effect and resource class |
| --- | --- | --- |
| `read` | required `path`; optional `offset`, `limit` | Read-only; workspace-shared |
| `glob` | required `pattern`; optional `path`, `limit` | Read-only; workspace-shared |
| `grep` | existing `pattern`, `path`, `glob`, `ignoreCase`, `literal`, `limit` | Read-only; workspace-shared |
| `write` | existing `path`, `content` | Effectful; workspace-exclusive |
| `edit` | existing `path`, `edits: [{oldText, newText}]` | Effectful; workspace-exclusive |
| `shell` | required `command`; optional `timeout` | Effectful; workspace-exclusive |

All schemas reject unknown properties. Optional arguments remain genuinely
optional through SDK rendering; retain the existing non-strict declarations.
Arguments are validated before approval or execution. A model's assertion that
a shell command is read-only never changes its resource or permission class.

`read_only` advertises and permits only `read`, `glob`, and `grep`, and rejects
forged calls to effectful tools. `ask` and `allow_effects` advertise all six and
apply the runtime spec's approval rules. Verification remains a separate
runtime action using the shell executor, not a seventh model tool.

## Read files and directories

Use one schema without a model-selected file/directory mode:

```json
{
  "type": "object",
  "properties": {
    "path": {"type": "string", "minLength": 1},
    "offset": {"type": "integer", "minimum": 1},
    "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
  },
  "required": ["path"],
  "additionalProperties": false
}
```

`path` stays required. `"."` explicitly selects the workspace root. Other paths
use the existing workspace-relative validation and canonical containment check.
Handle `"."` as a read-specific root case; do not relax write/edit validation.
Resolve the path and accept only a regular file or a directory. Reject missing
paths, special files, invalid UTF-8 file content, and paths escaping the root.
An explicitly selected symlink may resolve to an in-workspace file/directory;
an escaping target is rejected. Directory enumeration never follows child links.
These checks retain the workspace boundary and are not OS sandboxing.

### File behavior

Read UTF-8 text with the existing 2 MiB input limit. `offset` is a one-based line
number, defaulting to 1; `limit` defaults to 2000 lines and has a maximum of
2000. Reject zero or out-of-range values rather than silently accepting them.
Preserve `L<number>:` content lines. Add a short header identifying the result
as a file and JSON-quoting the requested path.

### Directory behavior

List only immediate children, including dotfiles, `.git`, and ignored entries.
This is direct inspection, so it does not apply project ignore filters. `glob`
and `grep` retain their search filters.

`offset` is a one-based entry index, defaulting to 1. `limit` defaults to 500
entries and may be raised to 2000, matching the existing listing limit range.
Sort the complete listing by lowercased name, then original name as a tie-breaker,
before applying pagination. A repeated request over an unchanged directory
must produce the same order; different calls do not share a filesystem snapshot.
Concurrent filesystem changes can shift pages.

Return a directory header and one indexed entry per line. Include entry type;
ordinary directory names end with `/`. JSON-quote displayed names so newlines
or control characters in filenames cannot create fake entries. Identify symlinks
without reading their targets. Fail explicitly on non-UTF-8 names rather than
returning an ambiguous lossy name.

Example calls:

```json
{"path": "."}
{"path": "src", "offset": 501, "limit": 500}
{"path": "src/main.rs", "offset": 80, "limit": 40}
```

Illustrative directory result:

```text
Directory "."
E1: file ".gitignore"
E2: file "Cargo.toml"
E3: directory "src/"
```

### Output bounds and continuation

Keep `ToolResultOutput::Text`. The entire result, including headers and
continuation messages, is capped at 50 KiB. Reserve space for those messages
before admitting content. Do not split a file line or directory entry into a
partial successful record.

If more content remains, give the next unread line/entry index explicitly, for
example `[output truncated; continue with offset=501]`. If the next record
cannot fit even on an otherwise empty page, return a descriptive error; do not
suggest an offset that would endlessly repeat the same empty page. Empty files,
empty directories, and offsets beyond the end have explicit end/empty messages
and no continuation marker. Negative, fractional, zero, or overflowing numeric
arguments are errors.

Suggested declaration: "Read a workspace UTF-8 file or list one directory.
Use path '.' for the root. offset is one-based; offset and limit count file
lines or directory entries. Directories include hidden and ignored children.
Output is bounded; use the returned offset to continue."

## Rename find to glob

Retain the existing `pattern`, `path`, and `limit` schema. `pattern` must be
nonempty and valid for the existing matcher. An omitted `path` or `"."` selects
the workspace root; other paths must resolve to an in-workspace directory.

Preserve these behaviors:

- Without `/`, match the basename at each traversed depth. `*.rs` therefore
  finds Rust files below the search root, not only its immediate children.
- With `/`, match paths relative to the selected search root using the existing
  matcher configuration. Use `/` as the documented pattern separator.
- Return paths relative to the workspace root, with `/` separators and a
  trailing `/` for directories. The selected root itself is excluded.
- Return both files and directories; skip symlinks and do not follow them.
- Respect the existing ignore walker configuration, include non-ignored hidden
  entries, and exclude `.git` traversal. Do not reinterpret this as `.gitignore`
  alone or change global/parent ignore handling in this rename.
- Default to 1000 results, allow a limit of 1 through 2000, and retain bounded
  output, cancellation, and no-match/truncation behavior.

No new ordering or pagination guarantee is introduced for `glob`. Update names
in declarations, errors, prompts, permissions, and native tests. Keep `grep`'s
optional `glob` filter unchanged; it is an argument, not another tool name.

## Unify shell execution

Keep the command interface small:

```json
{
  "type": "object",
  "properties": {
    "command": {"type": "string", "minLength": 1},
    "timeout": {"type": "integer", "minimum": 1, "maximum": 120}
  },
  "required": ["command"],
  "additionalProperties": false
}
```

Whitespace-only commands are invalid. Timeout defaults to 30 seconds. Do not
add model-selected interpreters or inherit the client's login shell preference.

### Interpreter selection

The server resolves the interpreter before advertising tools for a native
execution. Selection is fixed for that execution and shared by model-issued
commands and its verification action:

| Server platform | Ordered availability selection | Command invocation |
| --- | --- | --- |
| Unix | `bash`, then `sh` | resolved executable with `-c` |
| Windows | `pwsh`, then `powershell.exe` | existing noninteractive PowerShell flags and UTF-8 setup |

Resolve from the server environment and retain the executable path and dialect.
The declaration must identify both, for example "Uses Bash at /bin/bash on the
server", plus the existing host-authority/output wording. The client platform
never determines server command syntax. Results include an `interpreter` object
with `executable` (the resolved path) and `dialect` (`bash`, `sh`, or `powershell`)
alongside existing command outcome fields. PowerShell declarations also name
the selected executable, distinguishing `pwsh` from `powershell.exe`.

Fallback occurs only during availability selection, before model sampling.
After declaring Bash, failure to spawn it returns a tool error; it never retries
the generated command under `sh`. Apply the same rule to the two PowerShell
executables. Nonzero exit, cancellation, timeout, or launch failure never triggers
another interpreter. Availability does not prove a subsequent launch will work.

If no interpreter is available, a coding execution fails clearly before model
sampling; it does not silently expose five tools. Read-only execution still
works without shell discovery. There is no user override in this version.

### Execution and outcomes

Preserve workspace cwd, null stdin, piped stdout/stderr, bounded live output,
existing 32 KiB per-stream capture, timeout/cancellation behavior, and descendant
cleanup on normal exit, timeout, cancellation, or dropped execution futures.
Reuse the existing Unix process-group and Windows process-wrapper mechanisms.
This tool supports noninteractive commands, not an interactive terminal.

Retain `exit_status`, `stdout`, `stderr`, both truncation flags, and `timed_out`.
Nonzero exit remains an explicit outcome, not a success claim. Launch/cleanup
errors and cancellation retain their current error classification; possible
effects are handled by the runtime's effect-status and recovery contract.
Shell commands can affect paths outside the workspace under host authority.

Verification uses the same resolved interpreter and executor without model
dispatch, retains its own evidence/outcome policy, and stays forbidden in
`read_only`. Changing tool names must not widen verification permissions.

## History and compatibility

New model requests declare only the six canonical names. Do not register
`bash`, `powershell`, `ls`, or `find` as hidden dispatch aliases. Calls to those
names in a new request are unavailable and receive normal not-executed results.
Native instructions must name `read`, `glob`, and `grep` for inspection, and
`write`, `edit`, and `shell` for coding.

Preserve historical names, arguments, provider call IDs, Item IDs, results, and
approval records exactly. Completed history stays readable; do not rewrite
stored assistant calls or manufacture new results under renamed tools.

If future continuation/recovery encounters pending work created against the
old declarations, treat it as incompatible work requiring explicit resolution.
Settle safely unstarted calls through the runtime's not-executed path; unknown
or potentially started effects retain recovery blocking. Do not execute old
calls through aliases, reuse old approvals, or automatically replay effects.
A confirmed safe continuation may issue a fresh request with the new tools.
This requirement does not claim that cross-restart continuation exists today.

## Implementation scope and documentation

Implement within `bitrouter-orchestrator`; keep SDK provider adapters unchanged.
Use its static tool registry as the common source for declaration, validation,
effect/resource metadata, and dispatch. Do not introduce an extensible interpreter
framework, filesystem abstraction, plugin loader, or new scheduler for this slice.

Change `tools.rs`, native instructions in `agent.rs`, verification wiring in
`service.rs`, and applicable native process/integration tests. Update runtime
tool/resource tables when this contract is approved. Preserve unrelated worktree
changes and external harness behavior.

The implementation change must update `skills/bitrouter/SKILL.md`, relevant
references, `docs/CLI.md`, and current development/implementation documentation.
Review `.claude-plugin/`, `.codex-plugin/`, and `.agents/plugins/marketplace.json`
and update affected CLI/harness references in lockstep. Do not rewrite historical
spec bodies or treat local implementation as release publication.

## Acceptance and review

The gates below are verified for this slice. macOS real-model evidence and
Windows hosted tests are recorded separately in the acceptance record. Future
restart continuation remains outside this slice, as specified above:

- [x] Unix and Windows coding declarations expose exactly the six canonical
  names; read-only declarations expose only `read`, `glob`, and `grep`.
- [x] Forged effectful/legacy calls are rejected before launch; new shell naming
  preserves approval, workspace exclusion, and verification restrictions.
- [x] SDK requests preserve omitted optional arguments and the actual server
  interpreter description; validation rejects malformed/unknown arguments.
- [x] File reading preserves line numbers/UTF-8 limits. Root and child-directory
  reads include hidden/ignored entries and paginate in deterministic order,
  including case ties, empty/end pages, escaped names, and symlink entries.
- [x] Traversal/absolute-path/symlink escapes and special-file reads fail;
  boundary-size results stay bounded and continuation always makes progress.
- [x] Rename regression tests preserve `glob` basename/path behavior, ignore
  filtering, file/directory results, cancellation, and symlink exclusions.
- [x] Interpreter selection happens before sampling. Missing preferred
  interpreters select the documented fallback; loss after selection never
  re-executes a command under another interpreter. Read-only works without one.
- [x] Commands stream bounded output and preserve exit/timeout/cancellation
  outcomes. Descendant cleanup passes on Unix and Windows, including dropped
  futures and background children after the parent exits.
- [x] Verification shares interpreter selection; historic records keep their
  names/IDs and pending old calls cannot reuse approvals or replay effects.
- [x] Required source checks pass: `cargo nextest run --all-features` (or
  `cargo test --all-features`), `cargo clippy --all-features`, and
  `cargo fmt -- --check`. Report platform evidence separately from local checks.
- [x] Skills and affected harness manifests match delivered behavior.

Directory pagination, availability-only fallback, and rejection of legacy
dispatch aliases are implemented. Historic storage is unchanged; future pending
work recovery still belongs to runtime R4 and is not claimed here. The controlled
real-model comparison establishes functionality, not a quality or cost improvement.

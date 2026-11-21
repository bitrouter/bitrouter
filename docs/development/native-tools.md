# BRO six base tools


BRO exposes six workspace tools. It validates calls before approval, records
effects in local checkpoints and keeps execution within the selected workspace
grant. The [agent runtime guide](agent-runtime.md) describes authority and
recovery. External ACP adapters define their own tools and permissions.
The implementation lives under [BRO tools](../../apps/bro/src/tools.rs).

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
Resolve the path and accept only a singly linked regular file or a directory.
Reject missing paths, special files, invalid UTF-8 file content, and paths
escaping the root.
An explicitly selected symlink may resolve to an in-workspace file/directory;
an escaping target is rejected. Directory enumeration never follows child links.
After path resolution, open each child relative to its held parent directory.
These checks retain the workspace boundary and are not OS sandboxing.

`write` and `edit` sync the complete replacement before installing it in
the held directory. New files never replace an existing entry. An edit
verifies the existing file and its original bytes; on a publication conflict,
the displaced file is retained for recovery. BRO reports an unknown effect
when publication or syncing may have changed the workspace but completion is
uncertain.

On Windows, BRO installs and syncs a bounded `.bro-recovery-<id>.json`
record beside the target before moving the held original to its matching
`.bro-recovery-<id>.original` name. The record gives the exact original
basename (including its UTF-16 form), the escrow and staging basenames, and
the original SHA-256. The target can be absent if interrupted after the move
and before the staged file is installed. BRO does not retry that publication
or restore the original on restart.

To recover, stop workspace writers and inspect the record and files in that
directory. Verify the escrow bytes against the recorded digest. If the
destination is absent, restore the escrow to the recorded name with a move
that refuses to overwrite. If the destination exists, leave both files in
place and reconcile them manually. A record without an escrow can also mean
the move never happened or cleanup was interrupted; inspect the destination
before removing the record or any leftover staging file.

### File behavior

Read UTF-8 text with the 2 MiB input limit. `offset` is a one-based line
number, defaulting to 1; `limit` defaults to 2000 lines and has a maximum of
2000. Reject zero or out-of-range values rather than silently accepting them.
Preserve `L<number>:` content lines. Add a short header identifying the result
as a file and JSON-quoting the requested path.

### Directory behavior

List only immediate children, including dotfiles, `.git`, and ignored entries.
This is direct inspection, so it does not apply project ignore filters. `glob`
and `grep` retain their search filters.

`offset` is a one-based entry index, defaulting to 1. `limit` defaults to 500
entries and may be raised to 2000, subject to the same limit on every page.
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

## Glob search

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

## Shell execution

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

Whitespace-only commands are invalid. Timeout defaults to 30 seconds. BRO
selects the interpreter from the available workspace sandbox.

### Interpreter selection

On macOS, BRO runs `/bin/bash` under Seatbelt in a private working copy and
publishes changes after checking the original files. On Linux, it runs
`/bin/sh` with bubblewrap when the workspace sandbox is available. The
executor checks availability before starting and refuses shell execution
if it cannot establish the sandbox. See [workspace execution](../CLI.md#local-bro-threads-and-agents).

### Execution and outcomes

The sandbox runs noninteractive commands in the workspace with null stdin,
piped output and bounded capture. BRO supervises descendant cleanup on exit,
timeout or cancellation.

The result records `exit_status`, `stdout`, `stderr`, both truncation flags,
and `timed_out`.
A nonzero exit is an explicit outcome. If launch, cancellation or cleanup
leaves the effect uncertain, BRO records the uncertainty for recovery.

Verification uses the same sandbox and records its own result. It requires a
coding workspace with shell execution available.

## Checkpoints and recovery

BRO keeps the original call names, arguments, provider call IDs, results and
approval records. A resumed turn reuses committed results. If an effect may
have started before its result was committed, the checkpoint records that
uncertainty. Inspect it before attempting new work in the affected thread.

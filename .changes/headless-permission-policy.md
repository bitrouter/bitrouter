---
type: added
title: "Headless `acp prompt` and `spawn -p` take a permission policy and output formats"
pr: 866
---

A headless run used to deny every `session/request_permission`; now the caller
states the rule: `--deny-all` (still the default), `--approve-reads` (the ACP
tool kind is `read` or `search`), `--approve-all`, and a per-tool
`--permission-policy '{"autoApprove":[…],"autoDeny":[…],"defaultAction":…}'`
(or `@path`).

Each answer is a new NDJSON line,
`{"type":"permission","decision":"approved"|"denied","title":…,"kind":…}`, and
the process exits **5** when at least one request was denied and none approved.

`--format text` prints the transcript exactly as `bitrouter chat` prints it to a
pipe; `--format quiet` prints the assistant's text only; `json` (the default) is
the unchanged NDJSON.

The decision is made by `bitrouter_tui::permission::Policy` and reaches the
agent through one shared interpreter (`chat/effects.rs`) that the interactive
TUI, the piped `chat`, and `acp prompt` all run — the first session-verb parity
between the terminal and the headless CLI.

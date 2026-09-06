---
type: changed
breaking: true
title: "`bitrouter status --watch` becomes `--requests`, and the live view is gone"
pr: 830
---

`bitrouter status --watch` (`-w`) is replaced by `bitrouter status --requests`
(`-r`). The self-refreshing ratatui view is removed.

`--requests` prints newest-first settled requests — time, model, the provider
that **actually** served, tokens in and out, cost, latency, status — plus daemon
state and the window's spend and trailing-minute rate. It reads the metering
store directly, so it also works with **no daemon running** (`mode` reads
`history_only` rather than showing an empty list that looks like idleness).

To migrate: rename the flag, and wrap it if you want repetition —
`watch -n1 bitrouter status --requests --human`. The live view's two mutating
keys ran commands you can still run directly: `r` was `bitrouter reload`, and
`e` was your editor on `bitrouter.yaml`.

It was removed rather than moved into `bitrouter-tui` because its rows come from
the metering store and cover **every caller**, most of which never speak ACP.
Importing them would have put a daemon-wide model inside a session-scoped crate,
and ACP has no expression for them: no `model`, no `latency`, no per-request
`status` in v1 or v2, and `Usage`/`Cost` are cumulative per session. Removing it
is what let `ratatui` leave the app crate entirely — `apps/bitrouter` can no
longer construct a widget, so every drawn thing goes through the renderer crate
by compiler enforcement.

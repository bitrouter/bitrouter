---
type: added
title: "`bitrouter chat <agent>` opens a terminal session over ACP"
pr: 816
---

A thin TUI on top of the ACP agent endpoint: inline viewport, message and
tool-call rendering, a permission modal, a cost line, a log tail, and a provider
picker. It shares `RoutingOptions` with `launch`, so the same routing flags
apply.

Nothing in it renders a control that lies. The cost line shows
`cost unreported` rather than `$0.00` when nothing has been reported, and
labels the figure `daemon_wide` when per-session attribution is impossible; the
provider picker is hidden entirely when the session cannot be rerouted.

It lives in its own crate (`bitrouter-tui`) with no dependency edge to the app
crate, and reads its data from the ACP wire rather than from BitRouter
internals — so it is a client of the same surface any other ACP client sees.

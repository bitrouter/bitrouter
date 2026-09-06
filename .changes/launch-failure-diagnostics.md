---
type: fixed
title: "A failed launch reports a structured error and keeps a per-session log"
pr: 816
---

A launch that failed before the protocol came up used to `exit(1)` with nothing
to read. It now reports a structured failure, and both stderr streams are
captured into one per-session log under `~/.bitrouter/logs/`.

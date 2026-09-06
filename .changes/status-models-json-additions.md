---
type: added
title: "`bitrouter status --json` gains `providers[]` and `spend`; `models --json` gains `resolved_via`"
pr: 869
---

`bitrouter status --json` gains `providers[]` (the distinct providers behind the
routable models) and `spend`; `bitrouter models --json` gains `resolved_via`.
Both are additive — every pre-existing key is unchanged.

`bitrouter models` now prefers the running daemon's catalog, like
`bitrouter route`, and falls back to the config parse it always used. Standalone
answers (`models`, `route`, `spawn`'s Codex preflight) now resolve a config the
way the daemon does, so a subscription-backed provider (`claude-code`,
`google-ai`) is no longer missing from them.

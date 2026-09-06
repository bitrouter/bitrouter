---
type: changed
breaking: true
title: "`bitrouter status --requests` emits JSON by default"
pr: 848
---

`--requests` previously printed its table unconditionally, ignoring `--json` —
the only `status` path that did. It now honours the global format flags like
every other command: JSON by default, `--human` for the table.

A script that parsed the table needs `--human`. Anything that wanted the data
now gets one clean JSON object with a stable `rows[]`.

Rows carry `charge_status` and `episode_id` (the handle for
`bitrouter trajectory inspect`, `null` when capture is off). The spend rollup
states its `scope` — `all callers`, not one session — and reports `null` /
`unreported` when no request has charge evidence, rather than a misleading
`$0.00`. Per-session spend is `bitrouter chat`'s cost line.

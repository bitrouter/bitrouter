---
type: changed
breaking: true
title: "MCP `status` and `list_models` return the CLI's report types"
pr: 869
---

The origin server's `status` and `list_models` tools return the same report
types as `bitrouter status` and `bitrouter models`, and advertise them as
`output_schema`.

- `list_models` was a bare `[{ id, provider }]` that kept only the **first**
  provider of each model. It is now
  `{ models: [{ id, providers: [...] }], resolved_via: "live" | "config" }` —
  the whole fallback chain per model — with an optional `provider` argument (the
  same filter as `bitrouter models --provider`). On stdio + local it reads the
  daemon's live routing table over the control socket and falls back to a config
  parse, so it **answers with no daemon running**; `resolved_via` says which view
  it is.

- `status` was a `GET /v1/models` in disguise (`{ listen, models, providers }`
  locally, the raw balance on cloud). It is now
  `{ running, pid?, listen?, models?, providers, socket?, spend? }`: a stopped
  daemon is `running: false`, **not a tool error**, and `spend` carries two
  independent halves — `spent` (today's locally metered estimate, with `unpriced`
  saying how partial it is) and `limit` (a metered account's remaining credit).
  It is no longer served on HTTP + local — nothing on that transport can read the
  daemon's control socket — and no longer carries the free-text spend footer,
  which is now the typed `spend` block (`complete` keeps its footer).

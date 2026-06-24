# bitrouter-gui

Native GPUI desktop renderer for the BitRouter multi-agent coding-agent
orchestrator (see `bitrouter` issue #604). Paseo/Superset-style sidebar UX with
per-agent terminal + ACP render modes and a `brvk_` cost HUD.

- `crates/bitrouter-gui-core` — pure, UI-free core: wire types, state reducer,
  `Feed` trait + mock feed.
- `crates/bitrouter-gui` — the GPUI app (added in Phase 2).

Design + plan: see the `bitrouter` repo `docs/superpowers/` specs/plans.

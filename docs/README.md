# BitRouter docs

This folder holds **internal development docs** — the CLI reference, the
workspace architecture guide, and design specs. It is *not* published anywhere.

## Contents

- [`CLI.md`](CLI.md) — full command reference, flags, and config resolution.
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals.
- `*_SPEC.md` / `*_ACCEPTANCE.md` — design specs and acceptance criteria for
  in-flight work (spawn/launch, onboarding, the MCP `2026-07-28` upgrade,
  skills over MCP, the observability TUI, the ACP TUI, the ACP controller,
  the agent registry).
- [`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) — **phases 0–3 implemented, 4–5
  proposed.** One actions table so the CLI leaf and the MCP tool that answer
  the same question share one report type, one implementation, and a guard
  test. Written for
  [#868](https://github.com/bitrouter/bitrouter/issues/868); stands alone
  from #863 and #866.
- [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) — authoritative boundary
  for ACP controller topology, harness-owned sessions, endpoint configuration,
  native identity, and session-scoped routing.
- [`ACP_EVOLUTION_SPEC.md`](ACP_EVOLUTION_SPEC.md) — **implemented; controlled
  serving acceptance passed; historical calibration pending.** Recorded-evidence
  rubric evaluation, checkpoint feedback, batched
  Thompson sampling and session-sticky policy blocks. The
  [experiment report](ACP_EVOLUTION_EXPERIMENTS.md) separates controlled results,
  negative findings and remaining historical-data/product validation.
- [`CODE_TUI_UX_SPEC.md`](CODE_TUI_UX_SPEC.md) — **implemented; locally verified.**
  Replaces the seven-view Code dashboard with a conversation, contextual
  pickers/inspectors, and agent/route/activity/attributed-cost status; defines
  shared interaction behavior, ACP boundaries, and acceptance criteria.
- [`AGENT_INTERFACE_UNIFICATION_SPEC.md`](AGENT_INTERFACE_UNIFICATION_SPEC.md) —
  **proposed for review.** Unifies the public agent UX around native
  `claude`/`codex` shortcuts, the `code` TUI, headless `run`, and one raw
  `acp serve` bridge; retires visible `spawn`, keeps sessions harness-owned,
  and reduces MCP CLI to stdio serving plus one diagnostic while direct remote
  MCP moves into the daemon.
- [`CODE_TUI_UX_SPEC.md`](CODE_TUI_UX_SPEC.md) — **implemented.** Builds on
  merged #900/#901 with a scrollback-native Code surface: no dashboard tabs,
  one docked transient footer, explicit Enter-to-queue **Next turn** composition
  with paused recovery, and temporary alternate-screen inspectors for large
  read-only detail.
- [`REMOTE_CONTROL_MVP_SPEC.md`](REMOTE_CONTROL_MVP_SPEC.md) — **implemented.**
  Read-only remote status/models/route/requests and operations inspectors over an
  authenticated, loopback-only HTTP control listener; ACP stays local.
- [`REMOTE_ADMINISTRATION_SPEC.md`](REMOTE_ADMINISTRATION_SPEC.md) — **implemented.**
  Expands remote inspection and adds explicitly authorized reload,
  with shared action metadata, live/disk policy views, partial-failure reporting,
  and recovery after a client disconnect. Remote agent execution stays deferred.
- [`REMOTE_CLI_TUI_SUPPORT_SPEC.md`](REMOTE_CLI_TUI_SUPPORT_SPEC.md) — **Phase 2
  RFD, deferred.** Remote ACP sessions over a versioned WebSocket transport,
  constrained execution, and reconnect behavior.
- [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md) — **the live one.** Why
  the OTLP renderer ships as `crates/bitrouter-telemetry` while `bitrouter-sdk`
  keeps only the contract it renders (`observe::schema`, `SpanAttributes`).
  Start here; the two documents below are its history. Read its *The arguments
  that are dead* section before reopening anything — the crate-count and
  build-cache cases were measured, withdrawn, and are not what decided this.
- [`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md) — **D1 superseded.**
  Recorded why the exporter moved *into* `bitrouter-sdk` behind an `otel`
  feature. That placement was reversed before it reached a release. Still the
  best record of the hard constraint, the feature shape, and which names,
  targets and config keys are load-bearing.
- [`OTEL_TIERING_SPEC.md`](OTEL_TIERING_SPEC.md) — proposed splitting that
  module into schema / emission / export tiers. **Phases 0–2 landed and stand**
  (the committed span-schema artifact, the `tracing` bridge kept, the
  OTel-native ingress span); its D1 was withdrawn on measured benefit and then
  reopened on the positioning grounds it had itself reserved. Read its *cloud
  question* section before reopening any of it.
- `*_PLAN.md` — ordered execution plans derived from a spec, with per-task
  completion criteria. [`ACP_TUI_PLAN.md`](ACP_TUI_PLAN.md) is written to be
  driven by `/goal`.

- [`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md) — **proposed, nothing
  built.** Interrogates the goal "every headless CLI command has the same
  interactive TUI command" and argues against it: 103 leaves rather than 29, a
  quarter of them hostile in a session, no mature tool in the field achieving
  parity, and a three-set topology rather than a subset with a gap. Proposes ~6
  session commands dispatched through the same action ports the CLI uses, with
  `ACTIONS` extended by `tui_command` / `effect` / `requires` and five guards.
  Knowingly reverses [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §8.3 in a narrowed
  form; read its §5 and §16 D1 before agreeing to anything.
- [`CLI_TUI_PARITY_IMPL_SPEC.md`](CLI_TUI_PARITY_IMPL_SPEC.md) — **proposed,
  nothing built.** The buildable form of the above: the Rust for the `ACTIONS`
  extension, the resolver that replaces the TUI's string compares, five phases
  with the files each touches, the guards as tests, and what each open decision
  blocks. Written against the actions-table stack tip (#869 → #870 → #875),
  not `main`; its Appendix A lists the research spec's `file:line` references
  that have since moved.
- [`CLI_TUI_PARITY_BUILD_SPEC.md`](CLI_TUI_PARITY_BUILD_SPEC.md) — **ready to
  execute.** The impl spec re-cut for an autonomous agent under `/loop`: a
  one-task-per-iteration protocol, a resumable ledger
  ([`CLI_TUI_PARITY_PROGRESS.md`](CLI_TUI_PARITY_PROGRESS.md)), ten tasks with
  the impl-spec sections each reads, all fifteen open decisions collapsed into
  instructions, four stop conditions, and fifteen prohibitions. It carries no
  design of its own — every type and function body stays in the impl spec.
- [`CLI_TUI_PARITY_PROGRESS.md`](CLI_TUI_PARITY_PROGRESS.md) — the build
  plan's ledger: which task the loop is on and what has landed. Mutable; the
  only state the loop keeps.

## Where product docs live

The **product** documentation that used to live here now lives in the
**[bitrouter-docs](https://github.com/bitrouter/bitrouter-docs)** repository, under
`content/docs/` — it is authored, reviewed, and published there.

- Edit product docs in `bitrouter-docs`, not here.
- The `supported-models` / `supported-providers` tables are generated on the docs
  site from this repo's committed `dist/registry/{models,providers}.json`
  (`scripts/generate-registry-tables.mjs`), so keep the registry catalog current
  here as usual — the tables follow automatically.
- On each release, an agent in `bitrouter-docs` drafts a docs update from the
  changelog for human review.

- [Real Codex subscription ACP pilot](ACP_SUBSCRIPTION_PILOT.md): controlled tasks, frozen evidence, model-reference comparisons and observed follow-up issues.
- [Rubric v2 fresh-task comparison](ACP_RUBRIC_V2_HOLDOUT.md): four additional subscription task families, revised responsibility semantics, preserved unknown validation and version-compatibility checks.
- [TS goal audit](ACP_TS_GOAL_AUDIT.md): requirement-level evidence for the controlled reward pilot, learner experiments and serving implementation; separate limits on natural-history and live-benefit claims.

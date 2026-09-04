# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- **Breaking (CLI):** `bitrouter route --json` changes shape. `bitrouter route`
  and the MCP `route_preview` tool are now one action over one report type, and
  the report keeps `route_preview`'s richer vocabulary — it was the superset,
  and it is the one an agent reads. Duplicated keys for one fact are exactly the
  drift the unification exists to remove, so the old names are gone rather than
  deprecated.

  ```jsonc
  // before — `bitrouter route gpt-5 --json`
  {
    "model": "gpt-5",
    "resolved_via": "config",
    "chain": [{ "provider": "openai", "service_id": "gpt-5", "protocol": "openai" }]
  }

  // after
  {
    "requested_model": "gpt-5",          // was `model`
    "effective_model": "gpt-5-codex",    // new: what the policy table selects
    "effective_effort": "high",          // new, omitted when policy selects none
    "resolved_via": "config",            // unchanged: live daemon | config | zero-config
    "policy_decision": { … },            // new, omitted on the live-daemon path
    "provider_chain": [                  // was `chain`
      { "provider": "openai", "service_id": "gpt-5-codex", "api_protocol": "openai" }
                                         // `protocol` → `api_protocol`
    ],
    "estimated_cost": { … }              // new, omitted when the registry prices nothing
  }
  ```

  Migration is mechanical: `.model` → `.requested_model` (read
  `.effective_model` if you want what would actually run), `.chain` →
  `.provider_chain`, `.chain[].protocol` → `.provider_chain[].api_protocol`.

  Two behaviour fixes ride along. `bitrouter route` now runs the **policy
  table** in its config fallback, as `route_preview` always did — it could
  previously name a model the daemon would never pick, which is why
  `effective_model` is a separate field from `requested_model`. And
  `route_preview` now resolves config **per call** instead of snapshotting it at
  `bitrouter mcp serve` start, so an edited `bitrouter.yaml` is visible to a
  long-lived MCP server, as it always was to the CLI.

- **Breaking (Rust API):** `bitrouter_mcp::capabilities::routing` is gone. The
  routing port moved to `bitrouter_mcp::actions::route` and is now typed:
  `RoutingQuery::preview(RoutePreviewArgs) -> serde_json::Value` becomes
  `RouteQuery::route(RouteInput) -> RouteReport`. `ServeOptions::routing` takes
  the new trait object.

- **Breaking (CLI):** `bitrouter skills add`, `remove`, `find`, and `update` are
  removed, along with the `bitrouter-skills` crate that backed them. Installing
  skills is the ecosystem's job — `npx skills add`, or the Claude Code / Codex
  plugin marketplaces (this repo ships as one). BitRouter **reads** the
  installed-skills directory and serves it over MCP; it does not populate it.
  That is the same line as "server, not host" applied to content lifecycle:
  BitRouter handles transport, not distribution.

  `bitrouter skills list` and `bitrouter skills init` remain. `SKILL.md` format
  support moved into the binary (`apps/bitrouter/src/skills/`), where its only
  consumers live; the git-clone, source-resolution, install-to-disk, and
  registry-client code is gone. The `--registry` / `--namespace` flags and the
  `api.bitrouter.ai` skills-hub client went with them.

- **Skills over MCP (SEP-2640).** BitRouter now serves Agent Skills as an MCP
  server and proxies them as a gateway, over stdio and Streamable HTTP alike.
  `bitrouter mcp serve --backend skills` answers `skills/list`, `skills/get`,
  `resources/list`, and `resources/read` over the installed-skills root, with
  complete `sha256:` digests per file; the existing `skills_search` /
  `skills_get` tools are unchanged and still served. The daemon's aggregate
  `POST /mcp` merges upstream skill catalogs, namespacing each under its
  configured server name (`skill://<server>/<skill-path>/SKILL.md`) so two
  upstreams publishing the same URI cannot shadow one another. BitRouter is a
  skills server and gateway, never a host: no daemon path installs skills, and
  gateway-sourced content never touches a filesystem skill-discovery path.
  Caveats are documented in `skills/bitrouter/references/mcp-server.md` — the
  gateway is not a security boundary, and remote catalogs are daemon-scoped
  rather than caller-scoped.

- **Breaking (behaviour):** aggregate `resources/read` (`POST /mcp`) no longer
  tries each member and returns the first success. It resolves exactly one
  owning member — by skill-URI label, else by which member enumerates the URI —
  and errors when zero or several match, naming the candidates. First-success
  scanning let configuration order silently decide which upstream answered a
  URI two members both served, which is a cross-origin misroute (and the
  impersonation surface SEP-2640 names for skills). A URI that no member
  enumerates is now an error on the aggregate endpoint; read it from that
  server's direct route (`POST /mcp/{server}`) instead.

- The gateway's `initialize` now advertises the `resources` capability and
  declares the `io.modelcontextprotocol/skills` extension. Skills are read
  through `resources/read`, so a compliant client that saw no `resources`
  capability would never issue one. The extension is declared optimistically —
  upstream capabilities are discovered lazily, so the gateway cannot know at
  handshake time whether any member serves skills.

- **Breaking (Rust API):** `BenchmarkOutcomeRecord` has a new `request_id`
  field and strict reward feedback joins it to the persisted
  `CapturedIngressTrace.id`. Migrate Rust struct literals to
  `BenchmarkOutcomeRecord::new(session_key, task_id, reward)` followed by
  `.with_request_id(trace_id)` when producing reward-feedback artifacts. Older
  outcome JSONL remains serde-compatible (`request_id` defaults to absent),
  but it is analytical-only and strict feedback rejects it.

- **Breaking (policy routes):** active `policy_table` routing now uses one
  predictive route contract:
  `agent_route/v1|<task-family>|<role>|<risk>`. Exact task-family routes fall
  back to the corresponding `unknown`-family role/risk baseline, then to the
  policy default. Observed `agent_trace/v2` keys remain telemetry only. Static
  `agent_trace` routes, three-segment predictive v1 routes, and all v2
  predictive routes are rejected during config and lock validation. Regenerate
  policy locks and certificates with the current predictor contract.

  `key_strategy: agent_trace` selects this deterministic predictor; the retired
  `workflow_state` and `legacy_fingerprint` spellings are rejected.
  `adequacy.max_downgraded_requests_per_session` is rejected: session identity
  is diagnostic-only and no longer affects routing. `adequacy.explore_opening`
  is honored for source-neutral opening projections.

- **Rust API:** `PolicyKeyStrategy` now exposes only the canonical `AgentTrace`
  variant. `PolicyDecision` keeps
  `workflow_state_kind` and `workflow_identity`; `PolicyDecisionRecord` keeps
  `workflow_state` and `workflow_identity`; and `PolicyDecisionSummary` keeps
  `by_workflow_state`. Their JSON output uses canonical `trace_state`,
  `trace_identity`, and `by_trace_state` names while accepting the old JSON
  spellings on input. The matching `trace_*` accessors are available for new
  Rust callers.

## [1.0.0-alpha.27](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.26...v1.0.0-alpha.27)


### ⛰️ Features

- *(sdk)* Clarify request timing metrics ([#738](https://github.com/bitrouter/bitrouter/pull/738)) - ([a5b0730](https://github.com/bitrouter/bitrouter/commit/a5b073078c38c4d449b872cd1fb43fed9cc0bd78))
- *(tui)* Scrollable PTY panes (host scrollback + mouse forwarding) ([#734](https://github.com/bitrouter/bitrouter/pull/734)) - ([9eb1186](https://github.com/bitrouter/bitrouter/commit/9eb1186a011dd2f5036d3c9964572ee6604fabe2))
- *(tui)* Wire MCP and skills gateways into harnesses ([#732](https://github.com/bitrouter/bitrouter/pull/732)) - ([b199653](https://github.com/bitrouter/bitrouter/commit/b1996539706991407a1e5f1dc67ab59bf951f80a))
- *(tui)* Composite manager  ([#715](https://github.com/bitrouter/bitrouter/pull/715)) - ([7a72dff](https://github.com/bitrouter/bitrouter/commit/7a72dfffc997d9b7becc1cf2f77fc57573674f23))

### 🐛 Bug Fixes

- *(fleet)* Non-blocking spawn/prompt so long turns don't time out ([#737](https://github.com/bitrouter/bitrouter/pull/737)) - ([84c1e99](https://github.com/bitrouter/bitrouter/commit/84c1e992fdebf7b9379caaeecfc6cfb19c581023))
- *(sdk)* Isolate protocol request extras ([#733](https://github.com/bitrouter/bitrouter/pull/733)) - ([dffc9a9](https://github.com/bitrouter/bitrouter/commit/dffc9a9efdd872e2e69605ccfae6f27b11d8bcd4))

### 🚜 Refactor

- *(tui)* Split state.rs into a state/ module ([#730](https://github.com/bitrouter/bitrouter/pull/730)) - ([d730e8e](https://github.com/bitrouter/bitrouter/commit/d730e8e9b1e6ad17dc452c27d6816c8dc053ee3f))


## [1.0.0-alpha.26](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.25...v1.0.0-alpha.26)


### ⛰️ Features

- *(acp)* Route spawn sub-agents through daemon by default ([#705](https://github.com/bitrouter/bitrouter/pull/705)) - ([e069c8c](https://github.com/bitrouter/bitrouter/commit/e069c8cded12add2aa3d9a26f64394919e5cb562))
- *(cli)* Add cloud API command ([#718](https://github.com/bitrouter/bitrouter/pull/718)) - ([26cea96](https://github.com/bitrouter/bitrouter/commit/26cea96221250ceec0892ab2a7e71e890c675141))
- *(routing)* Adaptive, self-optimizing policy-table routing ([#710](https://github.com/bitrouter/bitrouter/pull/710)) - ([0828e7e](https://github.com/bitrouter/bitrouter/commit/0828e7ed40104272da4b6e429b05cb3e5f03079d))

### 🐛 Bug Fixes

- *(sdk)* Preserve upstream bad requests ([#716](https://github.com/bitrouter/bitrouter/pull/716)) - ([d6872ba](https://github.com/bitrouter/bitrouter/commit/d6872ba1ed73fd6d600b99c6c1a5adab063b699d))

### 🚜 Refactor

- Remove attestation, fold plugins/ into crates/ ([#701](https://github.com/bitrouter/bitrouter/pull/701)) - ([e9896fa](https://github.com/bitrouter/bitrouter/commit/e9896fa321c29f753db3b76e4bd6d684d0307cf4))
- Relocate bitrouter-mcp crate under crates/ ([#704](https://github.com/bitrouter/bitrouter/pull/704)) - ([a01a118](https://github.com/bitrouter/bitrouter/commit/a01a1184bf7ebaf352c082897bdd1b6d0be3fd30))


## [1.0.0-alpha.25](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.24...v1.0.0-alpha.25)


### ⚙️ Miscellaneous Tasks

- Refresh Rust dependencies and MSRV ([#699](https://github.com/bitrouter/bitrouter/pull/699)) - ([f7ad755](https://github.com/bitrouter/bitrouter/commit/f7ad75509b5a7195f279f93dbcad0e66c52e0a60))


## [1.0.0-alpha.24](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.23...v1.0.0-alpha.24)


### 🐛 Bug Fixes

- *(sdk)* Preserve streaming preflight errors ([#696](https://github.com/bitrouter/bitrouter/pull/696)) - ([24d9ecf](https://github.com/bitrouter/bitrouter/commit/24d9ecf51ce4343c9d97e33a00dedf4e82cffbaf))
- *(update)* Detect npm installs in self-updater ([#689](https://github.com/bitrouter/bitrouter/pull/689)) - ([0cb6ce2](https://github.com/bitrouter/bitrouter/commit/0cb6ce2f40284d169e16114bb2e0c59f577b07aa))
- Record canonical stream timing and OTEL latency ([#697](https://github.com/bitrouter/bitrouter/pull/697)) - ([639ec9b](https://github.com/bitrouter/bitrouter/commit/639ec9b3ed9f0548102108d49699d99a34cb2740))


## [1.0.0-alpha.23](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.22...v1.0.0-alpha.23)


### ⛰️ Features

- *(agents)* Add pi-acp to the ACP catalog ([#685](https://github.com/bitrouter/bitrouter/pull/685)) - ([c12f5d8](https://github.com/bitrouter/bitrouter/commit/c12f5d842932d1fb843dc218fbdf81662d6e3248))
- *(providers)* Add SuperGrok BYO subscription ([#676](https://github.com/bitrouter/bitrouter/pull/676)) - ([ad266c7](https://github.com/bitrouter/bitrouter/commit/ad266c7bf13aa480e9f02ea46e7f9716f88fc4cf))
- Ship Claude Code and Codex agent plugins ([#683](https://github.com/bitrouter/bitrouter/pull/683)) - ([6ea6458](https://github.com/bitrouter/bitrouter/commit/6ea6458926e0bde62ffd55415d4cd2460c7af34a))
- Add AWS Bedrock, Azure, Vertex built-in providers ([#647](https://github.com/bitrouter/bitrouter/pull/647)) - ([6b92fcf](https://github.com/bitrouter/bitrouter/commit/6b92fcf612ee5eff54e426f7ba8b50a023002caa))

### 🐛 Bug Fixes

- *(sdk)* Preserve token limits and upstream errors ([#692](https://github.com/bitrouter/bitrouter/pull/692)) - ([495b1e9](https://github.com/bitrouter/bitrouter/commit/495b1e9aa4de4e79f877ca2d7fda0cf88bffad65))


## [1.0.0-alpha.22](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.21...v1.0.0-alpha.22)


### ⛰️ Features

- Configurable upstream HTTP timeouts ([#662](https://github.com/bitrouter/bitrouter/pull/662)) - ([88d1e4d](https://github.com/bitrouter/bitrouter/commit/88d1e4da5a774bf44a7e569be2fda3e60dd47c7a))

### 🐛 Bug Fixes

- *(messages)* Drop unsigned reasoning on Anthropic wire ([#669](https://github.com/bitrouter/bitrouter/pull/669)) - ([d78ddec](https://github.com/bitrouter/bitrouter/commit/d78ddec4a3bc05e11c60e00f64874387fbdccb38))


## [1.0.0-alpha.21](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.20...v1.0.0-alpha.21)


### ⛰️ Features

- Make registry dist runtime-only ([#641](https://github.com/bitrouter/bitrouter/pull/641)) - ([733bf7f](https://github.com/bitrouter/bitrouter/commit/733bf7f576607dd5e47ca0853e241df531e3468e))
- Refactor registry provider variants ([#639](https://github.com/bitrouter/bitrouter/pull/639)) - ([0436241](https://github.com/bitrouter/bitrouter/commit/04362417b9821b715d9c858789083534cf302a3b))

### 🐛 Bug Fixes

- Preserve stream error settlement ([#656](https://github.com/bitrouter/bitrouter/pull/656)) - ([4e1df77](https://github.com/bitrouter/bitrouter/commit/4e1df77805a6f91a1607d44035ab1fea2e82d988))


## [1.0.0-alpha.20](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.19...v1.0.0-alpha.20)


### ⛰️ Features

- *(cli)* Spawn codex agent ([#631](https://github.com/bitrouter/bitrouter/pull/631)) - ([27b5bae](https://github.com/bitrouter/bitrouter/commit/27b5baecccd20ace3cf3bb211ff5a577a7cbc4a7))
- *(cli)* Route `update` through the JSON output layer ([#620](https://github.com/bitrouter/bitrouter/pull/620)) - ([a2edc3f](https://github.com/bitrouter/bitrouter/commit/a2edc3f409627cd595ffe9429196ec4648ad40ae))
- *(cli)* JSON output layer with a uniform error envelope ([#610](https://github.com/bitrouter/bitrouter/pull/610)) - ([df6e3cc](https://github.com/bitrouter/bitrouter/commit/df6e3cc487bba9e60461828c86a028656a619e21))
- *(server-tools)* Built-in web_fetch tool (BYOK) ([#612](https://github.com/bitrouter/bitrouter/pull/612)) - ([ba5308d](https://github.com/bitrouter/bitrouter/commit/ba5308df77cf8457812b93af6ff557a8c8f24728))
- *(server-tools)* Add Tavily web_search backend, drop Perplexity ([#608](https://github.com/bitrouter/bitrouter/pull/608)) - ([6579faa](https://github.com/bitrouter/bitrouter/commit/6579faa122381208b5b4902b8c59b1429fd640e6))
- *(server-tools)* Built-in web_search with BYOK backends ([#603](https://github.com/bitrouter/bitrouter/pull/603)) - ([7ebc572](https://github.com/bitrouter/bitrouter/commit/7ebc572ef33cb902a8a1ecea2b7d351a1300c4c1))
- *(update)* Add bitrouter update self-updater ([#607](https://github.com/bitrouter/bitrouter/pull/607)) - ([2b95d0c](https://github.com/bitrouter/bitrouter/commit/2b95d0c941930c8a7b10c58e33b5c66e2db16e4d))

### 🐛 Bug Fixes

- *(codex)* Harden Codex spawn checks ([#632](https://github.com/bitrouter/bitrouter/pull/632)) - ([e2264d6](https://github.com/bitrouter/bitrouter/commit/e2264d636e35689a4ac4366c421836cf4c43ed72))
- *(config)* Don't expand ${VAR} inside YAML comments ([#609](https://github.com/bitrouter/bitrouter/pull/609)) - ([7628d98](https://github.com/bitrouter/bitrouter/commit/7628d9855412da2a51e07ac11fe31a8b3510850e))
- *(observe)* Stop the settle span double-counting as a telemetry event ([#605](https://github.com/bitrouter/bitrouter/pull/605)) - ([2453112](https://github.com/bitrouter/bitrouter/commit/2453112f6e7703186c67cb1ae1f8578a5548084f))

### 📚 Documentation

- Align docs with current CLI ([#637](https://github.com/bitrouter/bitrouter/pull/637)) - ([29a693d](https://github.com/bitrouter/bitrouter/commit/29a693d9e97913b516449f9cfe11908d35c6af82))

### ⚙️ Miscellaneous Tasks

- Migrate registry into OSS dist ([#636](https://github.com/bitrouter/bitrouter/pull/636)) - ([afdb4b7](https://github.com/bitrouter/bitrouter/commit/afdb4b7f68723456efa804e1d2760ed103a59a84))


## [1.0.0-alpha.19](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.18...v1.0.0-alpha.19)


### ⛰️ Features

- *(cli)* Wait for daemon readiness on start; auto-start it on spawn ([#597](https://github.com/bitrouter/bitrouter/pull/597)) - ([7d6c85d](https://github.com/bitrouter/bitrouter/commit/7d6c85d5f06ce718dbe63c68f0a9c0ff5d7d1f2f))
- *(telemetry)* Live account-bearer refresh + $lib/$screen_name attributes ([#598](https://github.com/bitrouter/bitrouter/pull/598)) - ([b0f45e8](https://github.com/bitrouter/bitrouter/commit/b0f45e827e787d031d92d8f61770f636a1136475))


## [1.0.0-alpha.18](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.17...v1.0.0-alpha.18)


### ⛰️ Features

- *(claude-code)* Route Claude Code traffic to a dedicated subscription provider ([#593](https://github.com/bitrouter/bitrouter/pull/593)) - ([15fde63](https://github.com/bitrouter/bitrouter/commit/15fde6346c209e4f864d0b35e573006588765022))
- *(messages)* Preserve thinking-block signature through the streaming round-trip ([#592](https://github.com/bitrouter/bitrouter/pull/592)) - ([4d76676](https://github.com/bitrouter/bitrouter/commit/4d766767a29cc90149c919a3197e24d47bebad4a))
- *(observe)* Opt-in first-party telemetry export ([#588](https://github.com/bitrouter/bitrouter/pull/588)) - ([e95977f](https://github.com/bitrouter/bitrouter/commit/e95977f4fc4e484aa1311d17510a97ae26a67dca))
- *(providers)* Source built-ins from the registry snapshot ([#591](https://github.com/bitrouter/bitrouter/pull/591)) - ([e58c623](https://github.com/bitrouter/bitrouter/commit/e58c62332d74e402553ebba78939634dc5212f9b))
- *(registry)* Registry-backed model resolution ([#589](https://github.com/bitrouter/bitrouter/pull/589)) - ([fa4083e](https://github.com/bitrouter/bitrouter/commit/fa4083ec5789cb970a5e291786609ef9f8005e00))
- *(spawn)* Launch coding agents through BitRouter via `bitrouter spawn` ([#584](https://github.com/bitrouter/bitrouter/pull/584)) - ([5ca7ac8](https://github.com/bitrouter/bitrouter/commit/5ca7ac8c823d0b5d7f85f89d8d18b1dbcdc0cef0))
- *(telemetry)* Account-authenticated exports + anonymous opt-out ([#596](https://github.com/bitrouter/bitrouter/pull/596)) - ([d6406dd](https://github.com/bitrouter/bitrouter/commit/d6406ddfbb939beddb9e5cb9f4b504eb7f62f4ab))
- Read the existing Claude Code session for Claude subscription auth ([#590](https://github.com/bitrouter/bitrouter/pull/590)) - ([a720eb2](https://github.com/bitrouter/bitrouter/commit/a720eb231faab462de5c9b8929f67323772b0d3f))

### 🐛 Bug Fixes

- *(claude-code)* Detect subscription traffic by the agent-profile beta ([#594](https://github.com/bitrouter/bitrouter/pull/594)) - ([d528b47](https://github.com/bitrouter/bitrouter/commit/d528b47420dc941e77484cb1f8f8c9155a680858))


## [1.0.0-alpha.17](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.15...v1.0.0-alpha.17)


### ⛰️ Features

- *(attestation)* NEAR confidential-inference verification (L1 + L1.5) ([#563](https://github.com/bitrouter/bitrouter/pull/563)) - ([9dc0a1d](https://github.com/bitrouter/bitrouter/commit/9dc0a1d9cea18b74f7ee112c85e5968545957103))
- *(config)* JSON Schema + config validate for IaC ([#575](https://github.com/bitrouter/bitrouter/pull/575)) - ([8ab2382](https://github.com/bitrouter/bitrouter/commit/8ab2382240964dbbdb7804c808d4c22eead3b672))
- *(fusion)* Multi-model deliberation server tool + alias ([#574](https://github.com/bitrouter/bitrouter/pull/574)) - ([c7bfac9](https://github.com/bitrouter/bitrouter/commit/c7bfac9482d86287e8c44d789f532edc16516e62))
- *(mcp)* Answer the MCP initialize/ping handshake at the gateway ([#561](https://github.com/bitrouter/bitrouter/pull/561)) - ([43d1365](https://github.com/bitrouter/bitrouter/commit/43d1365fce758f4fb2b4ea528d01ef55fdd92d82))
- *(sdk)* Expose server-tool usage for observability ([#568](https://github.com/bitrouter/bitrouter/pull/568)) - ([5eaf1c3](https://github.com/bitrouter/bitrouter/commit/5eaf1c35d399d6e84f8fd0cd9c644a0f6d1cf17b))

### 🐛 Bug Fixes

- *(attestation)* [**breaking**] Pin & assert TDX base MRs ([#567](https://github.com/bitrouter/bitrouter/pull/567)) ([#578](https://github.com/bitrouter/bitrouter/pull/578)) - ([62e412c](https://github.com/bitrouter/bitrouter/commit/62e412cd5b035b7b4e379cb14829f9d4c599bfec))
- *(attestation)* Enforce DCAP TCB status floor ([#573](https://github.com/bitrouter/bitrouter/pull/573)) - ([5193bf9](https://github.com/bitrouter/bitrouter/commit/5193bf9cece32e1ae006e876e180d73d0c8e7356))
- *(chat)* Buffer streamed tool call until name arrives ([#577](https://github.com/bitrouter/bitrouter/pull/577)) - ([ab02eb6](https://github.com/bitrouter/bitrouter/commit/ab02eb6ddc4fd56110f5d7038b5b19830bb588ea))

### 🚜 Refactor

- *(server-tools)* Move advisor/sub-agent into the SDK ([#576](https://github.com/bitrouter/bitrouter/pull/576)) - ([5ffe7b1](https://github.com/bitrouter/bitrouter/commit/5ffe7b13b0d808f45b82a70cc73f8450e5965114))


## [1.0.0-alpha.15](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.14...v1.0.0-alpha.15)


### ⛰️ Features

- *(server-tools)* Router-executed server-side tool loop ([#555](https://github.com/bitrouter/bitrouter/pull/555)) - ([e41e1ce](https://github.com/bitrouter/bitrouter/commit/e41e1cef6481929918f001ca1ec4e54e95d99988))
- *(server-tools)* Replace owned provider-defined tools on inject ([#559](https://github.com/bitrouter/bitrouter/pull/559)) - ([df70e12](https://github.com/bitrouter/bitrouter/commit/df70e1260891f1694c802d32b319a66686bd2c17))
- *(server-tools)* Pass ToolContext to RouterToolset ([#558](https://github.com/bitrouter/bitrouter/pull/558)) - ([bffff57](https://github.com/bitrouter/bitrouter/commit/bffff57506fec640f6aa8e25d48094fd78a01127))
- Protocol-native routing ([#554](https://github.com/bitrouter/bitrouter/pull/554)) - ([197f812](https://github.com/bitrouter/bitrouter/commit/197f8126a108abd30a2b9f02cb59d9b0028b54d4))


## [1.0.0-alpha.14](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.13...v1.0.0-alpha.14)


### 🐛 Bug Fixes

- *(chat)* Drop provider-defined tools (no chat wire form) ([#553](https://github.com/bitrouter/bitrouter/pull/553)) - ([b0b4ef3](https://github.com/bitrouter/bitrouter/commit/b0b4ef365d83d9a5f2ee380067320ff5303d9f2c))


## [1.0.0-alpha.13](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.12...v1.0.0-alpha.13)


### ⛰️ Features

- *(language-model)* LanguageModelV3 parity for the canonical IR ([#548](https://github.com/bitrouter/bitrouter/pull/548)) - ([610ee27](https://github.com/bitrouter/bitrouter/commit/610ee2788f0496cf7268dd5244b7676543de9303))
- *(metering)* Context-tier ("staged") pricing ([#551](https://github.com/bitrouter/bitrouter/pull/551)) - ([0464703](https://github.com/bitrouter/bitrouter/commit/04647033f11f3e908178b354482bd1333ee289eb))

### 🐛 Bug Fixes

- *(protocol)* Don't fragment streamed tool calls ([#552](https://github.com/bitrouter/bitrouter/pull/552)) - ([e140b2f](https://github.com/bitrouter/bitrouter/commit/e140b2fb105de59b600104ed0f1019cad66d1cc9))


## [1.0.0-alpha.12](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.11...v1.0.0-alpha.12)


### ⛰️ Features

- *(mcp)* Origin MCP server for BitRouter ([#526](https://github.com/bitrouter/bitrouter/pull/526)) ([#530](https://github.com/bitrouter/bitrouter/pull/530)) - ([aa71d4c](https://github.com/bitrouter/bitrouter/commit/aa71d4c3bf571cae64c2a50ed1bf54603328f8f2))

### 🐛 Bug Fixes

- *(protocol)* Translate tool_choice across protocols ([#549](https://github.com/bitrouter/bitrouter/pull/549)) - ([8e60ce5](https://github.com/bitrouter/bitrouter/commit/8e60ce5e9e855d13f9d82e1381bafd4fcd0564bf))


## [1.0.0-alpha.11](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.10...v1.0.0-alpha.11)


### ⛰️ Features

- *(capabilities)* Extend Capability vocabulary (tools, reasoning, web_search, logprobs) ([#536](https://github.com/bitrouter/bitrouter/pull/536)) - ([e369f2e](https://github.com/bitrouter/bitrouter/commit/e369f2e184832c877f42bbb7b0673b9f38f3ef06))

### 🐛 Bug Fixes

- *(pipeline)* Run non-streaming requests to completion ([#538](https://github.com/bitrouter/bitrouter/pull/538)) - ([5528e3e](https://github.com/bitrouter/bitrouter/commit/5528e3e33fff495ee17ab41d1dc35962f5a6f2e8))
- *(stream)* Bill prompt tokens on client disconnect ([#537](https://github.com/bitrouter/bitrouter/pull/537)) - ([f0dc0f0](https://github.com/bitrouter/bitrouter/commit/f0dc0f0cddd55e79ff8304287ab5593f2fb8ccbd))


## [1.0.0-alpha.10](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.9...v1.0.0-alpha.10)


### ⛰️ Features

- *(capabilities)* Capability-aware routing primitives + typed structured-output fields (B4) ([#528](https://github.com/bitrouter/bitrouter/pull/528)) - ([e4fee7c](https://github.com/bitrouter/bitrouter/commit/e4fee7c85b9a7e134152cce3cf412d958183e419))
- *(observe)* [**breaking**] Forward cloud span attrs + content capture ([#535](https://github.com/bitrouter/bitrouter/pull/535)) - ([2c7edd3](https://github.com/bitrouter/bitrouter/commit/2c7edd3a54014d59c5048424ae828f9011b68a3e))
- *(sdk)* Surface MCP upstream 401/403 auth challenges (UpstreamAuth) ([#534](https://github.com/bitrouter/bitrouter/pull/534)) - ([f81a322](https://github.com/bitrouter/bitrouter/commit/f81a3221fa1918bf750612cb2c615bc4d59e67b4))
- *(sdk)* Implement VirtualModel priority/cascade strategy ([#521](https://github.com/bitrouter/bitrouter/pull/521)) - ([6b06d94](https://github.com/bitrouter/bitrouter/commit/6b06d94ac1d0a1f47cd463fd6d76b41b4c44732f))

### 🐛 Bug Fixes

- [**breaking**] Pre-1.0 security audit fixes + dead-code cleanup ([#523](https://github.com/bitrouter/bitrouter/pull/523)) - ([44ec022](https://github.com/bitrouter/bitrouter/commit/44ec02299a8df495ba4dbbb19408a39da9e3e55d))

### 🚜 Refactor

- *(cli)* [**breaking**] Subcommand cleanup (placeholders + auth) ([#524](https://github.com/bitrouter/bitrouter/pull/524)) - ([c2f5fef](https://github.com/bitrouter/bitrouter/commit/c2f5fefbcf91315b117a4739c2e3a912088a3331))


## [1.0.0-alpha.9](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.8...v1.0.0-alpha.9)


### ⛰️ Features

- *(providers)* Claude & Codex OAuth subscriptions ([#519](https://github.com/bitrouter/bitrouter/pull/519)) - ([6983051](https://github.com/bitrouter/bitrouter/commit/6983051e5dc93b4b9bfb9ddcf1e26a2f1c29f8d7))


## [1.0.0-alpha.8](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.7...v1.0.0-alpha.8)


### ⛰️ Features

- *(sdk)* [**breaking**] Per-target Messages auth scheme (x-api-key | Bearer) ([#516](https://github.com/bitrouter/bitrouter/pull/516)) - ([da10012](https://github.com/bitrouter/bitrouter/commit/da10012e4db97db650ccd69c8ac243ef2172e511))


## [1.0.0-alpha.7](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.6...v1.0.0-alpha.7)


### ⛰️ Features

- *(skills)* [**breaking**] Converge marketplace.json on Claude Code native schema ([#512](https://github.com/bitrouter/bitrouter/pull/512)) - ([0f83a39](https://github.com/bitrouter/bitrouter/commit/0f83a399fa21c414a4c55c6c0e4e93001aac6d43))

### 🐛 Bug Fixes

- *(cli)* Correct auth login --scope help to namespace-read ([#513](https://github.com/bitrouter/bitrouter/pull/513)) - ([b420fc1](https://github.com/bitrouter/bitrouter/commit/b420fc1d347a73e97558d5c4133d2c318bbc17e4))


## [1.0.0-alpha.6](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.5...v1.0.0-alpha.6)


### ⛰️ Features

- *(cloud-sdk)* Namespace-scoped management client + CLI ([#508](https://github.com/bitrouter/bitrouter/pull/508)) - ([f4ff599](https://github.com/bitrouter/bitrouter/commit/f4ff599cd2927d56c163b5281162b92b62fdfdcb))
- *(messages)* Integrate Opus 4.8 Messages API parameters ([#509](https://github.com/bitrouter/bitrouter/pull/509)) - ([c768b50](https://github.com/bitrouter/bitrouter/commit/c768b508f6374e861566ab1ba94e546a0ed1a30d))
- *(observe)* [**breaking**] Split otel feature into transport sub-features ([#506](https://github.com/bitrouter/bitrouter/pull/506)) - ([dcb3830](https://github.com/bitrouter/bitrouter/commit/dcb3830a1612b8284a092798ec0247543b4c622b))
- *(skills)* Skills gateway client — bitrouter-skills crate + CLI ([#511](https://github.com/bitrouter/bitrouter/pull/511)) - ([5de0786](https://github.com/bitrouter/bitrouter/commit/5de0786a811a0eedaf77ba981fd64f418350cee7))


## [1.0.0-alpha.5](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.4...v1.0.0-alpha.5)


### ⛰️ Features

- *(guardrails)* [**breaking**] Plugin + per-account rule resolution ([#503](https://github.com/bitrouter/bitrouter/pull/503)) - ([ee06962](https://github.com/bitrouter/bitrouter/commit/ee069621f1b56eaecc45d54b9409b69ce3383d92))
- *(observe)* [**breaking**] GenAI-semconv trace hierarchy + outbound traceparent ([#495](https://github.com/bitrouter/bitrouter/pull/495)) - ([d05fd5c](https://github.com/bitrouter/bitrouter/commit/d05fd5cbd155f3d798ac70c0b341b00176ff9c55))
- *(sdk)* [**breaking**] Streaming gen_ai.response.id via StreamPart::ResponseStarted ([#500](https://github.com/bitrouter/bitrouter/pull/500)) - ([4432941](https://github.com/bitrouter/bitrouter/commit/44329412689e41aca16b743f452b689c8e22d998))

### 🚜 Refactor

- *(sdk)* [**breaking**] Rename API protocols to spec names ([#504](https://github.com/bitrouter/bitrouter/pull/504)) - ([820ec67](https://github.com/bitrouter/bitrouter/commit/820ec675f56b7ae4b537829b4297fbee65c5274f))


## [1.0.0-alpha.4](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.3...v1.0.0-alpha.4)


### ⛰️ Features

- *(bitrouter)* Windows support for the daemon control surface ([#490](https://github.com/bitrouter/bitrouter/pull/490)) - ([9a960df](https://github.com/bitrouter/bitrouter/commit/9a960df6e8e65555e62db979fb1cfd19e5330609))
- *(sdk/mcp)* Expose headers, set_caller, evict for auth ([#494](https://github.com/bitrouter/bitrouter/pull/494)) - ([7bf6e76](https://github.com/bitrouter/bitrouter/commit/7bf6e76efc9ceecfcaf3da80efc5bedfb0a184f8))

### 🐛 Bug Fixes

- *(sdk/anthropic)* Fold cache buckets into prompt_tokens (Usage subset contract) ([#492](https://github.com/bitrouter/bitrouter/pull/492)) - ([5ee6921](https://github.com/bitrouter/bitrouter/commit/5ee6921a3125f6d2245338f23bff98640b71d1cc))


## [1.0.0-alpha.3](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.2...v1.0.0-alpha.3)


### ⛰️ Features

- *(cli)* Bitrouter cloud subcommand for /v1/* management ([#489](https://github.com/bitrouter/bitrouter/pull/489)) - ([c0dc761](https://github.com/bitrouter/bitrouter/commit/c0dc761b824777c8b3e36483a3328746e525c824))
- *(cli)* OAuth 2.0 device-flow auth subcommand ([#480](https://github.com/bitrouter/bitrouter/pull/480)) - ([f5cba27](https://github.com/bitrouter/bitrouter/commit/f5cba270795ace61eec682eee449c60fe2e11c7b))
- *(mcp)* Aggregation + caching + SSE + sampling-deny ([#484](https://github.com/bitrouter/bitrouter/pull/484)) - ([f1ab313](https://github.com/bitrouter/bitrouter/commit/f1ab313582bc92655c9eec9c27524a0622e3e063))
- *(observe)* Migrate to OpenTelemetry with multi-tenant attribution ([#475](https://github.com/bitrouter/bitrouter/pull/475)) - ([be513cf](https://github.com/bitrouter/bitrouter/commit/be513cf631b121a86fe57decc4ef89509802e73f))
- Bitrouter-cloud SDK + LLM provider with OAuth or API-key onboarding ([#486](https://github.com/bitrouter/bitrouter/pull/486)) - ([35e8894](https://github.com/bitrouter/bitrouter/commit/35e8894a461088a78540b7536c50137a1703d414))
- PKCE OAuth login for Anthropic and OpenAI Codex ([#481](https://github.com/bitrouter/bitrouter/pull/481)) - ([62d3b29](https://github.com/bitrouter/bitrouter/commit/62d3b29855039c51828a35e003a454cac457df18))

### 🐛 Bug Fixes

- *(bitrouter)* Drop duplicate reqwest dep entry ([#485](https://github.com/bitrouter/bitrouter/pull/485)) - ([d65fd1b](https://github.com/bitrouter/bitrouter/commit/d65fd1b6a2a3389f4707676de658c8a40a8488fb))


## [1.0.0-alpha.2](https://github.com/bitrouter/bitrouter/compare/v1.0.0-alpha.1...v1.0.0-alpha.2)


### ⛰️ Features

- *(routing)* Multiple accounts per provider — failover + load-balance ([#473](https://github.com/bitrouter/bitrouter/pull/473)) - ([a248fd3](https://github.com/bitrouter/bitrouter/commit/a248fd3644b08aebbde8b64f24bfe05716a536e0))
- *(sdk)* Structured outputs (response_format) for all 4 protocols ([#472](https://github.com/bitrouter/bitrouter/pull/472)) - ([8d4dc54](https://github.com/bitrouter/bitrouter/commit/8d4dc542e65f28ab751b38c1633839d8e9e4c243))
- *(sdk)* Add RouterOptions to opt out of built-in routes ([#478](https://github.com/bitrouter/bitrouter/pull/478)) - ([c548527](https://github.com/bitrouter/bitrouter/commit/c5485279531f351d4ebd2422435c4fb968bc9bb1))
- *(sdk)* Derive JsonSchema on wire types ([#467](https://github.com/bitrouter/bitrouter/pull/467)) ([#469](https://github.com/bitrouter/bitrouter/pull/469)) - ([901ec3e](https://github.com/bitrouter/bitrouter/commit/901ec3e83fdbbe689f3943cee46de2a3c0f46cdb))

### 🐛 Bug Fixes

- *(daemon)* Re-apply built-in provider catalog on file reload ([#476](https://github.com/bitrouter/bitrouter/pull/476)) - ([0bd11aa](https://github.com/bitrouter/bitrouter/commit/0bd11aaf84bf115fd6e9bfd4315400b33dfff166))

### 🚜 Refactor

- *(db)* Sea-orm migrations + all backends, no concrete driver ([#474](https://github.com/bitrouter/bitrouter/pull/474)) - ([f6a28dd](https://github.com/bitrouter/bitrouter/commit/f6a28dd78b52f1d038de2538b2a699e82d388888))

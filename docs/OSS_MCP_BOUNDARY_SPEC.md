# Spec: OSS agent control without a first-party MCP origin

Status: **implemented for review** · Date: 2026-09-14
Baseline: `b1cd8f33` (`feat(mcp): add native skills gateway support (#912)`)

This document defines the target boundary between BitRouter OSS, agent skills,
the `bro` CLI, the MCP gateway, and a future multi-tenant BitRouter Cloud MCP
origin.

The decision is deliberately narrower than “remove MCP from BitRouter.”
BitRouter OSS keeps its MCP client, gateway, downstream MCP endpoint,
Skills-over-MCP relay, and server-side tool loop. It stops publishing
BitRouter-owned control actions and the host's installed skills through a
first-party origin MCP server.

For a local single-tenant installation, an agent that already has permission to
run local commands controls BitRouter through the shipped `/bitrouter` Skill and
the structured `bro` CLI. A Skill describes how to act; the host's shell or
local-process capability performs the action. MCP is not required between two
components already running as the same user on the same machine.

For a multi-tenant hosted installation, any BitRouter-owned MCP origin belongs
to BitRouter Cloud, where caller identity, tenant isolation, authorization,
quotas, audit, and version rollout can be enforced at the actual ownership
boundary. That implementation is not shipped from this OSS repository.

---

## 1. Decision summary

The target architecture has three independent paths:

```text
Local agent control (single tenant)

  Claude / Codex / other local agent
               │ native Skill + host-provided command execution
               ▼
             bro CLI
               │ direct typed action calls
               ▼
       local config / socket / daemon / database


MCP gateway (OSS, retained)

  downstream MCP host
               │ MCP: tools, resources, prompts, skills
               ▼
       BitRouter aggregate /mcp endpoint
               │ MCP client connections
               ▼
       configured upstream origin servers


Hosted BitRouter control (multi tenant)

  remote MCP host
               │ authenticated MCP
               ▼
       BitRouter Cloud MCP origin
               │ tenant-scoped action services
               ▼
       cloud control plane
```

The resulting OSS product surface is:

```text
Agent-facing local control
  skills/bitrouter/SKILL.md
  bro status
  bro models
  bro route <model>
  bro requests
  bro providers ...
  bro skills ...
  other existing structured CLI actions

MCP gateway and diagnostics
  daemon aggregate endpoint at mcp.aggregate.route (default /mcp)
  bro mcp check [server]
  mcp_servers: configuration
  server-side MCP tool execution for routed LLM requests
  Skills-over-MCP relay and aggregation

Removed from OSS
  bro mcp serve
  bro mcp serve --backend skills
  bro mcp install
  daemon /mcp-control origin endpoint
  bitrouter-mcp crate
  first-party plugin mcpServers entries
  automatic bitrouter_skills subprocess injection
```

“Removed from OSS” describes the target state. Section 12 defines how releases
reach it without silently breaking an existing plugin installation.

---

## 2. Terminology

The word “server” is overloaded in the current code and documentation. This
spec uses the following terms consistently.

| Term | Meaning | Target ownership |
| --- | --- | --- |
| **Origin MCP server** | Publishes capabilities it owns, such as BitRouter `status` or a filesystem skill catalog | Removed from OSS; Cloud may operate one |
| **Upstream origin** | A configured third-party MCP server consumed by BitRouter | External to BitRouter |
| **MCP gateway** | Acts as a client to upstream origins and as a server to downstream hosts; routes, aggregates, namespaces, caches, and applies policy | Retained in `bitrouter-sdk` and the daemon |
| **Downstream MCP endpoint** | The gateway's server-facing HTTP surface, normally `/mcp` | Retained in OSS |
| **BitRouter action** | A typed local operation such as status, models, route preview, or requests | Retained; transport-neutral and app-owned |
| **Native tool** | A tool adapter that calls a BitRouter action directly inside a BitRouter-owned tool loop | Allowed when a concrete consumer needs it; no MCP hop |
| **Agent Skill** | Instructions teaching an agent when and how to use `bro` | Retained and shipped in the plugin |
| **Host execution capability** | Shell or local-process permission supplied by Claude, Codex, Cowork, an IDE, or another agent host | Required for Skill + CLI; not supplied by the Skill itself |
| **Skills-over-MCP** | The `io.modelcontextprotocol/skills` extension and its `skills/list`, `skills/get`, and Resources behavior | Retained as a gateway protocol feature |

A gateway is technically an MCP server from the downstream host's perspective.
Removing the **origin MCP server** must never be interpreted as removing the
gateway's downstream server role.

---

## 3. Product assumptions

### 3.1 Primary OSS target

BitRouter OSS is local-first and single-tenant:

- one operating-system user owns the installation;
- `bro`, its config, the daemon socket, and the local metering database share
  that user's trust boundary;
- an agent acting locally is expected to use the permissions its host already
  grants it; and
- remote administration, when configured, is reached through `bro --context`
  and the authenticated control API rather than by pretending the remote
  machine is local.

Single tenancy does **not** itself grant command execution. Skill + CLI is a
supported integration only on hosts that provide a shell or equivalent local
process tool. A text-only chat surface can read a Skill but cannot execute
`bro`; this is a host limitation, not a reason for BitRouter OSS to ship a
parallel origin protocol.

### 3.2 Primary Cloud target

BitRouter Cloud is multi-tenant:

- every request has an authenticated caller and tenant;
- advertised tools vary by scopes and deployment capabilities;
- rate limits, billing, audit, and redaction are server responsibilities; and
- local config files, sockets, processes, and installed-skill directories have
  no meaning to a remote caller.

These properties justify a Cloud-owned MCP origin. They do not justify putting
the Cloud server implementation or its tenant policy in the OSS runtime.

### 3.3 Non-shell hosts

Supporting an MCP-only local host is no longer a core OSS requirement. Such a
host may use one of these explicit alternatives:

1. gain a host-provided command executor;
2. use the BitRouter HTTP APIs appropriate to the operation;
3. connect to the future BitRouter Cloud MCP origin; or
4. rely on a separately maintained compatibility adapter outside the core OSS
   workspace.

The core project does not retain `bitrouter-mcp` solely for this compatibility
case.

---

## 4. Local agent contract: Skill + structured CLI

### 4.1 Capability composition

The local contract is:

```text
instructions          execution               operation
/bitrouter Skill  +  host shell/process  +  bro structured CLI
```

Each part has one responsibility:

- the Skill decides which command is appropriate, explains preconditions, and
  interprets the result;
- the host obtains user approval and executes the local process; and
- `bro` resolves configuration, validates inputs, calls the typed action, and
  returns a stable machine-readable report.

The Skill must never claim that loading it grants shell access. When the host
does not expose execution, it should explain the missing capability rather than
inventing an MCP dependency.

### 4.2 Replacement mapping

The first-party origin tools do not need new CLI duplicates:

| Removed origin capability | Local replacement | Notes |
| --- | --- | --- |
| `status` | `bro status` | Same local action and metering source |
| `list_models` | `bro models [--provider ID]` | Preserve all-provider output and `resolved_via` semantics |
| `route_preview` | `bro route <model>` | Preview only; no upstream model call |
| `skills_search` | `bro skills list` for inspection; native host skill discovery for activation | BitRouter does not become a general host skill manager |
| `skills_get` | Native host skill loader | Do not add `bro skills show` merely to preserve an obsolete MCP tool |
| origin `skills/list` / `skills/get` | Native host/plugin skill installation and discovery | Configured upstream origins remain available through the gateway |
| `mcp install` | Install the BitRouter agent plugin/Skill through the host's normal plugin mechanism | No generated `bro mcp serve` config block |

### 4.3 CLI requirements

An agent-facing CLI is an API even when its transport is a subprocess. Every
command referenced by the Skill must therefore provide:

1. JSON by default or an explicit, documented `--json` mode;
2. stable field names and typed error envelopes;
3. meaningful non-zero exit codes for failed operations;
4. no prompts when a documented non-interactive flag is used;
5. no ANSI decoration in JSON output;
6. stdout reserved for the result and stderr for progress or human guidance;
7. secrets through environment variables, stdin, or the OS credential store,
   not command-line arguments where avoidable; and
8. config/context resolution reported in the result whenever operating on the
   wrong machine would be dangerous.

The `/bitrouter` Skill and its references are the shipped client of this
contract. Any CLI rename, flag change, port change, environment-variable
change, default change, or harness-wiring change must update the Skill in the
same patch, as required by `AGENTS.md`.

### 4.4 Permissions and safety

Replacing narrow MCP tools with a generic shell can increase the permission
ceiling of the host. The mitigation belongs at the correct layers:

- the host remains responsible for command approval and sandboxing;
- the Skill uses fixed `bro` commands and never constructs arbitrary shell from
  untrusted model or upstream output;
- destructive CLI actions retain their existing confirmation/non-interactive
  rules;
- commands do not interpolate untrusted strings into a shell; and
- remote contexts preserve their credential scopes and never fall back to the
  local machine after a remote error.

BitRouter must not ship a second command-runner protocol to compensate for a
host whose shell policy is intentionally restrictive.

---

## 5. Native BitRouter tools

“Put the tools inside BitRouter” means direct use of typed actions, not an
in-process MCP server and not an implicit loopback connection.

### 5.1 Direct action path

```text
BitRouter-owned agent/tool surface
             │ typed call
             ▼
       action interface
             │
             ▼
   app implementation / daemon state
```

If `bro code`, a BitRouter-owned agent, or the server-side tool loop needs
`status`, `models`, or `route`, it may register a native tool adapter that calls
the existing action directly. It must not serialize an MCP request, dial
`bro mcp serve`, or dial the daemon back through `/mcp-control`.

### 5.2 No default administrative injection

This spec does not automatically inject BitRouter administrative tools into
every proxied LLM request. Doing so would:

- change caller-visible tool choice and token usage;
- give untrusted prompts a new control surface;
- create name-collision and authorization questions; and
- conflate an inference gateway with an autonomous agent host.

A native action toolset is added only with a concrete BitRouter-owned consumer,
an explicit enablement policy, tool-name ownership, and authorization tests.
No placeholder toolset or unused abstraction is created during origin removal.

### 5.3 Existing server-side tool loop

The current server-side tool loop is retained. It discovers tools from
configured upstream MCP origins, converts them to provider-neutral function
tools, executes model-selected calls through the MCP client/gateway, returns
the results to the model, and continues the turn.

That path is the evidence that MCP **consumption** and LLM tool execution do not
depend on BitRouter publishing its own MCP origin.

---

## 6. MCP gateway contract

The following OSS capabilities are explicitly retained:

- `McpRequest`, `McpResponse`, `McpTarget`, routing, and hook contracts;
- stdio and HTTP clients for configured upstream origins;
- downstream MCP lifecycle handling on the aggregate endpoint;
- direct-server and aggregate routing;
- tool-name and prompt-name prefixing;
- resource ownership resolution;
- partial-success aggregation and `_bitrouterErrors`;
- caller propagation, authorization hooks, observability, and metering;
- notification-driven cache invalidation;
- `bro mcp check [server]`; and
- the MCP-backed server-side `RouterToolset`.

The daemon's aggregate route remains the MCP server surface presented to a
downstream host. Its server information may continue to identify it as the
`bitrouter-mcp-gateway`; it must not advertise BitRouter-owned action tools
merely because the old origin crate once did.

`mcp_servers:` remains the declarative source of upstream origins. Removing
`bro mcp serve` does not remove stdio upstream support: BitRouter must still be
able to spawn a configured third-party stdio MCP origin as a client.

---

## 7. Skills-over-MCP is a gateway feature

Skills-over-MCP is independent of any BitRouter-owned origin.

### 7.1 Retained protocol behavior

The SDK keeps:

- the `io.modelcontextprotocol/skills` extension identifier;
- `skills/list`, `skills/get`, and `resources/directory/read` method names;
- Skills request/result and manifest wire types;
- conservative extension-method relay allowlisting;
- URI namespacing under the aggregate member label;
- reverse routing for `skills/get` and `resources/read`;
- digest, size, result, TTL, and cache-scope preservation;
- per-origin collision isolation; and
- empty-list and partial-failure semantics on the aggregate endpoint.

The relevant target modules stay under `bitrouter-sdk::mcp`, especially
`skills`, `rmcp_executor`, `aggregating_executor`, and `caching_executor`.

### 7.2 Removed origin behavior

OSS stops reading local installed-skill directories for publication through
MCP. The following are removed:

- the origin `SkillCatalog` port in `bitrouter-mcp`;
- `InstalledSkillCatalog` and its origin-only resource reader;
- local `skills/list`, `skills/get`, `resources/list`, and `resources/read`
  responses owned by BitRouter;
- tool-shaped `skills_search` / `skills_get` publication; and
- `bro mcp serve --backend skills`.

`bro skills list` and local skill-format parsing may remain for user-facing
inspection, scaffolding, and validation. They must no longer be described as
the backing store for an MCP origin.

### 7.3 Harness ownership

`bro launch` and ACP session setup stop injecting the `bitrouter_skills` stdio
subprocess. A harness discovers locally installed skills through its own native
skill/plugin system.

The `bitrouter_tools` gateway remains injectable where the harness accepts an
MCP server. Because that gateway retains Skills-over-MCP aggregation, a
configured upstream origin's skills can still reach a compatible downstream
host through the same endpoint.

BitRouter does not copy every skill found on the launching machine into every
harness. Cross-harness skill installation is a plugin/package-manager concern,
not an inference router responsibility.

---

## 8. Action contract placement

Deleting `bitrouter-mcp` must not delete or fork the shared action reports.
Today the crate contains both transport-neutral reports/ports and the MCP
binding because the `#[tool]` macros needed to name those types. Once the
binding disappears, that placement is inverted ownership.

The target is:

```text
apps/bitrouter/src/actions/
  status.rs       types + port + implementation
  models.rs       types + port + implementation
  route.rs        types + port + implementation
  skills.rs       local inspection types + implementation
  commands.rs     command inventory types + implementation
  ...

Consumers
  CLI renderer
  Code TUI/session commands
  local/remote HTTP administration
  optional native tool adapter with a real consumer
```

The implementation phase moves the neutral report types and port traits from
`crates/bitrouter-mcp/src/actions/` into the app's existing action modules.
It updates imports without changing serialized report shapes.

The action inventory remains useful for CLI, TUI, and HTTP-control parity, but
its `mcp_tool` field and MCP-schema guard are removed. HTTP exposure and scopes
continue to live in the remote-control inventory. No replacement crate is
created unless a non-app crate has a demonstrated need to consume these
contracts.

BitRouter Cloud may define its MCP adapters against Cloud-owned action services.
The OSS action types are not required to become a public Cloud SDK merely to
keep the deleted crate's abstraction alive.

---

## 9. Plugin and distribution contract

The first-party agent plugins become Skill-only packages.

### 9.1 Claude plugin

`.claude-plugin/plugin.json` retains the BitRouter Skill and removes the
`mcpServers.bitrouter` entry. Its description says that the host uses the
installed `bro` CLI; it no longer promises an origin MCP server.

### 9.2 Codex plugin

`.codex-plugin/plugin.json` retains `skills/bitrouter` and removes the
`mcpServers` reference. `.codex-plugin/mcp.json` is deleted if the manifest
format permits omission, rather than retained as an empty compatibility file.

### 9.3 Marketplace metadata

Both marketplace descriptions describe the Skill + CLI integration. They do
not claim that installing the plugin exposes local tools to every chat surface.
Host execution remains a declared compatibility requirement.

### 9.4 Binary packaging

The `bro` binary no longer depends on `bitrouter-mcp`. The workspace removes the
crate, its examples, origin-server tests, installer, backend HTTP clients, and
MCP server macro dependencies that have no remaining consumer.

The implementation must measure the release artifact before and after the
removal rather than claiming a size or compile-time benefit from crate count
alone.

---

## 10. Cloud MCP origin boundary

A future BitRouter Cloud MCP origin is a Cloud feature, not a remote mode of
`bro mcp serve`.

It must:

- authenticate every MCP session and bind it to a tenant/caller;
- advertise only tools allowed by the caller's scopes and deployment tier;
- resolve status, models, routes, usage, budgets, and policies against Cloud
  services rather than a local socket or filesystem;
- apply Cloud rate limits, audit, redaction, and billing;
- avoid exposing local-only actions whose semantics cannot be made tenant-safe;
- use the Cloud deployment and rollback process; and
- publish its own stable URL and client configuration.

It must not:

- call back into a user's local daemon as an implicit fallback;
- read a user's local skill directories;
- reuse `BITROUTER_TOKEN` as an unscoped universal credential; or
- require the OSS `bro` binary to be installed on the MCP host.

The Cloud service may share wire-level protocol libraries where that is
operationally useful. Sharing an implementation repository or shipping the
multi-tenant server in the OSS binary is not a requirement.

---

## 11. Deliberate losses and accepted tradeoffs

Removing the origin is not behavior-preserving for every possible client.

| Loss | Why it is accepted | Mitigation |
| --- | --- | --- |
| MCP-only local hosts cannot call BitRouter control tools | They lack the host capability required by the primary local-agent contract | Use a shell-capable surface, HTTP API, Cloud MCP, or external adapter |
| Generic MCP tool discovery no longer reveals `status`, `list_models`, or `route_preview` | The local Skill already teaches the same single-tenant operations with less protocol duplication | Keep CLI JSON and Skill mappings stable |
| Local skills are no longer republished to harnesses over SEP-2640 | Native host skill systems own local installation and activation | Ship the BitRouter Skill through each supported plugin system; retain upstream Skills aggregation |
| A shell has a broader potential permission set than five read-only MCP tools | Permission is already owned by the local agent host | Fixed commands, host approvals, scoped credentials, and no shell interpolation |
| Existing manual `bro mcp serve` configurations stop working | Maintaining a duplicate origin indefinitely conflicts with the product boundary | Atomic manifest/Skill/docs/release-note update; clap reports the removed subcommand |
| OSS no longer provides a standards-based remote control origin | Single-tenant remote administration already has a scoped HTTP control API; multi-tenant MCP belongs in Cloud | Skill invokes `bro --context`; Cloud publishes its own MCP endpoint |
| Third parties cannot import `bitrouter-mcp` as a library | The crate is an application adapter, not a stable general-purpose SDK | Retain generic MCP gateway APIs in `bitrouter-sdk` |

These losses are release-note material. They must not be hidden behind a claim
that Skill + CLI is literally equivalent on a host with no process execution.

---

## 12. Migration plan

### Phase 0 — accept the boundary

- Review and approve this spec.
- Record the release that begins deprecation and the release that removes the
  origin.
- Confirm that supported Claude and Codex local surfaces can load the plugin
  Skill and invoke `bro` through a host-provided executor.

Completion criterion: maintainers agree that MCP-only local control is no
longer a core OSS requirement and that gateway Skills support remains in scope.

### Phase 1 — make Skill + CLI sufficient

- Audit every command referenced by `skills/bitrouter/` for non-interactive
  JSON behavior, exit codes, stderr/stdout separation, and current names.
- Add only missing CLI behavior required to replace a currently shipped origin
  action.
- Update plugin descriptions to lead with Skill + CLI.
- Add an end-to-end fixture in which an agent-shaped runner follows the Skill
  and obtains status, models, and route preview without MCP.

Completion criterion: the supported local workflow completes without starting
an origin server.

### Phase 2 — stop automatic origin use

- Remove first-party `mcpServers` manifest entries.
- Stop `bro launch` and ACP setup from injecting `bitrouter_skills`.
- Keep only the aggregate `bitrouter_tools` gateway injection where supported.
- Deprecate `bro mcp serve`, `bro mcp install`, and `/mcp-control` with messages
  naming the Skill + CLI, aggregate `/mcp`, or Cloud replacement as applicable.

Completion criterion: a fresh installation never starts `bro mcp serve`, while
configured upstream MCP tools and skills still traverse the gateway.

### Phase 3 — move neutral actions

- Move report types and port traits out of `bitrouter-mcp` and into the app's
  action modules.
- Preserve JSON shapes used by CLI, TUI, and HTTP control.
- Remove MCP-only action metadata and guards; retain relevant cross-surface
  parity checks.
- Rewire remote administration and output renderers to the neutral action
  modules.

Completion criterion: `apps/bitrouter` no longer imports action types from
`bitrouter-mcp`, and no serialized report changes unintentionally.

### Phase 4 — remove the OSS origin

- Remove `bro mcp serve`, its hidden backend/transport flags, and `mcp install`.
- Remove the daemon `/mcp-control` origin route while retaining the ordinary
  authenticated control HTTP routes.
- Delete `InstalledSkillCatalog` and origin-only skills resource serving.
- Delete `crates/bitrouter-mcp`, its tests, examples, README, and Cargo edges.
- Delete obsolete plugin MCP files.
- Update `README.md`, `docs/CLI.md`, `docs/DEVELOPMENT.md`, the changelog, and
  `skills/bitrouter/` in the same change.

Completion criterion: the workspace contains no BitRouter-owned MCP origin,
but all retained MCP gateway and server-tool tests pass.

### Phase 5 — Cloud origin, separately owned

- Specify Cloud tool inventory, auth scopes, tenant semantics, and endpoint.
- Implement and deploy it in the Cloud-owned system.
- Publish host setup independently from the OSS plugin's local Skill path.

Completion criterion: a remote MCP client can inspect only its authorized Cloud
tenant without installing `bro` and without any local fallback.

Phase 5 is not a blocker for completing phases 1–4. The local OSS workflow does
not depend on Cloud availability.

---

## 13. Compatibility policy

The recommended window is one alpha release because the project is pre-1.0 and
the first-party manifests can migrate atomically:

- release N stops new plugin installations from configuring the origin and
  prints deprecation messages for manual invocations;
- release N+1 removes the command, endpoint, and crate; and
- release notes provide exact replacement commands and explain that `/mcp`
  aggregation is unaffected.

After removal, invoking `bro mcp serve` may fail through clap's normal unknown
subcommand behavior. No permanent hidden alias, loopback shim, or generic shell
MCP server remains in the binary.

If review selects immediate removal instead, the implementation must still ship
the manifest, Skill, docs, and release-note changes atomically. The architecture
does not change; only the compatibility schedule does.

**Implementation decision (2026-09-14): immediate removal was selected.** The
origin command, endpoint, crate, automatic injection, manifests, Skill, docs,
and release note change together in this implementation.

---

## 14. Acceptance criteria

### 14.1 Local agent control

1. The first-party Claude and Codex plugins install the BitRouter Skill without
   configuring an MCP origin.
2. On a supported shell-capable host, the Skill can obtain status, models, and
   route preview through `bro` alone.
3. The JSON reports retain the fields and semantics previously shared with the
   origin tools.
4. The Skill identifies a missing local execution capability honestly.
5. Remote-context commands do not fall back to local state after failure.

### 14.2 MCP gateway

6. The aggregate `/mcp` endpoint still completes MCP initialization.
7. Direct and aggregate `tools/list` / `tools/call` continue to work.
8. Resources and prompts retain their current routing and collision behavior.
9. `bro mcp check [server]` remains the canonical upstream diagnostic.
10. Configured stdio upstream origins still start and negotiate normally.

### 14.3 Skills-over-MCP

11. A configured upstream serving Skills is visible through aggregate
    `skills/list`.
12. Aggregated skill URIs are namespaced by origin and round-trip through
    `skills/get` and `resources/read`.
13. Private or unknown cache hints never enter a cross-caller global cache.
14. Removing the local catalog does not remove Skills wire types, relay
    allowlists, aggregation, caching, or lifecycle advertisement.
15. A fresh local launch does not start `mcp serve --backend skills`.

### 14.4 Origin removal

16. `apps/bitrouter` has no dependency on `bitrouter-mcp`.
17. No first-party manifest references `bro mcp serve`.
18. The daemon has no `/mcp-control` route, while its scoped HTTP control API
    and aggregate `/mcp` route remain intact.
19. The workspace has no origin-server backend, installer, skill catalog, or
    origin-only MCP server tests.
20. CLI, TUI, and HTTP-control reports retain one typed action implementation.

### 14.5 Regression gates

21. `cargo nextest run --all-features` passes, or `cargo test --all-features`
    when nextest is unavailable.
22. `cargo clippy --all-features` passes.
23. `cargo fmt -- --check` passes.
24. Strict rustdoc and `git diff --check` pass under the repository's normal CI
    contract.
25. Release artifact size is measured before and after; any reported benefit is
    based on identical build profiles.

---

## 15. Non-goals

This spec does not:

- remove MCP support from BitRouter;
- replace configured upstream MCP origins with CLI commands;
- remove the aggregate downstream MCP endpoint;
- remove Skills-over-MCP from the gateway;
- turn BitRouter into an MCP host that installs, approves, or activates skills;
- expose a general shell tool to routed models;
- inject BitRouter control tools into all inference requests;
- make the Skill itself a permission or execution mechanism;
- add remote ACP;
- define the complete Cloud MCP tool inventory; or
- promise that every text-only desktop chat can run local commands.

---

## 16. Relationship to existing specifications

Once accepted, this document supersedes only the conflicting origin decisions
in existing specs:

- `AGENT_INTERFACE_UNIFICATION_SPEC.md` §13.1–13.4 and Phase 5, where they
  retain `bro mcp serve` and `/mcp-control`;
- `ACTIONS_SPEC.md`, where it makes the MCP binding the permanent owner of the
  shared action contract; and
- `SKILLS_MCP_SPEC.md`, where it places a BitRouter-owned local SkillCatalog and
  filesystem origin in the target product.

It preserves:

- the agent-interface spec's distinction between native harnesses, BitRouter
  ACP clients, and protocol plumbing;
- the actions spec's one-answer/one-report rule across real consumers;
- the Skills spec's protocol types, security invariants, cache semantics, and
  gateway aggregation behavior;
- the remote-administration specs' scoped HTTP control model; and
- `MCP_2026_07_28_SPEC.md` wherever it applies to the retained gateway.

Historical specs should receive a short supersession note when implementation
begins; they should not be rewritten as if the former decision never existed.

---

## 17. Review decisions

Approval of this spec accepts these product decisions:

1. **Local contract:** supported local agents control BitRouter through the
   `/bitrouter` Skill and structured `bro` CLI when their host provides local
   execution.
2. **Origin scope:** BitRouter OSS does not publish BitRouter-owned controls or
   the host filesystem as an origin MCP server.
3. **Gateway scope:** the MCP gateway, downstream `/mcp` endpoint, upstream MCP
   client, server-side tool loop, and `bro mcp check` remain first-class OSS
   features.
4. **Skills scope:** Skills-over-MCP remains a gateway feature independent of
   any origin; local host skill publication is removed.
5. **Native tools:** BitRouter-owned surfaces call action ports directly when a
   real consumer needs tools; general inference traffic receives no new
   administrative tools by default.
6. **Cloud scope:** a multi-tenant BitRouter origin is implemented and operated
   by Cloud, outside this OSS repository.
7. **Migration:** immediate removal is selected; manifests, Skill, docs, and
   release notes change atomically with the origin removal.
8. **Contract ownership:** neutral action types move app-side; no replacement
   crate is introduced without a demonstrated cross-crate consumer.

Implementation must not begin from “delete every MCP server.” It begins from
the narrower invariant: **delete the BitRouter-owned origin while preserving
the gateway's server role.**

# Development Guide

This document is the workspace-level guide for BitRouter internals. Start with [`README.md`](../README.md) for the product introduction, then use this guide when you need to understand how the workspace is assembled or how to build on top of the SDK.

## Workspace Architecture

BitRouter is a Cargo workspace organized into `crates/` for shared libraries and contracts, `apps/` for product hosts, and `extensions/` for independently delivered capabilities. Each extension groups its related Cargo packages in one directory; `helpers/` contains contributor tooling.

| Crate                            | Tier    | Responsibility                                                                                                          |
| -------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/bitrouter-ai`            | crate   | Model semantics, three-protocol codecs, SSE framing, selected-model HTTP calls, injected auth transactions and explicit catalog runtime; no router/application dependency |
| `crates/bitrouter-sdk`           | crate   | The SDK: three protocol pipelines, hook traits, staged AI executor integration, config loading, the axum HTTP server, and the observability contract (`observe`) |
| `extensions/regex-checker/matcher` | extension library | Rules and native request-check callback; optional `sdk` retains legacy hooks |
| `crates/bitrouter-telemetry`     | crate   | Optional telemetry egress: the OTLP exporter (traces + metrics, multi-tenant attribution), the inbound ingress span, and the `tracing` ↔ OTel bridge — all default-off |
| `crates/bitrouter-orchestrator` | crate | Native model/tool loop, durable Thread/Turn facts and context, instance-owned workers, and the optional native and external-agent ACP runtime |
| `apps/bitrouter`                 | app     | Assembly library + the `bro` CLI binary (package/lib stay `bitrouter`) — turns a `Config` into a running `App`, owns management commands, ACP host adapters and durable storage |

The `extensions/` directory expresses ownership and delivery boundaries; it does not create a loader or force a transport. Cargo packages remain ordinary Rust libraries or binaries. Custom hosts register native request checks through `bitrouter_sdk::extension::ExtensionApi` inside `assemble::build_app_with_extensions`; the SDK also retains legacy `Plugin`/hook interfaces for host assembly. See [the extension directory guide](../extensions/README.md) and [guardrails](../extensions/regex-checker/README.md).

For the native runtime module map, execution reading order and test layout, see
the [orchestrator source guide](../crates/bitrouter-orchestrator/README.md).

### External interfaces

`onboarding/resources.rs` runs finite optional setup prompts and delegates MCP
publication to `mcp_config`. Skill acquisition belongs to `npx skills`; local
inspection and file writes use `skills/directory.rs`.
Router settings are saved first; a resource-only no-op keeps original text,
comments and environment references. Headless partial failure returns one typed
onboarding report with a nonzero exit and no Finish action. It grants no MCP
skill-source authority and introduces no execution owner or installer database.

Clients reach BitRouter through its external **interfaces** — the ways *in*. These are distinct from AI's three internal *wire-protocol adapters* (Chat Completions / Responses / Messages, described below): an interface is an entry point, an adapter is a dialect the `language_model` pipeline parses and speaks.

| Interface                 | Where it lives                                                                                            | Entry point              |
| ------------------------- | -------------------------------------------------------------------------------------------------------- | ------------------------ |
| **API** (HTTP LLM router) | `bitrouter-sdk` `server` feature (`crates/bitrouter-sdk/src/server.rs`) over the `language_model` pipeline | `bro serve`        |
| **MCP gateway**           | `bitrouter-sdk` `mcp` feature plus app assembly: downstream `/mcp` aggregation, configured upstream clients, Skills-over-MCP relay, and server-side tool execution | `POST /mcp`; `bro mcp check` |
| **ACP**                   | `bitrouter-orchestrator` `acp` feature (`crates/bitrouter-orchestrator/src/acp/`): `controller` is the ACP-client-facing server, `client` the transport-generic ACP client, `up` the agent-process transport, and `translate` the typed view of `session/update` used by `run`. The client-facing stdio bridge, one-shot runner, all consume the same controller/client stack. Subcommand glue lives in `apps/bitrouter/src/acp_cli.rs`. | `bro acp serve <agent>`; `bro run` |
| **Native ACP** | `bitrouter-orchestrator::acp::native`: negotiated v1/v2 handlers, projectors, approval delivery and native controls over the host's single ThreadService. `apps/bitrouter/src/native_acp.rs` owns the OS-local endpoint and stdio bridge; EOF detaches. | bare `bro acp serve` |
| **BRO native agent** | `bitrouter-orchestrator::service::ThreadService` owns one Thread/Turn execution path, durable context, FIFO/steering, owner/workspace fencing and bounded observation. App owns the database and transport adapters. Safe hot unload/reload and cold public queries share the same authority. | `bro threads`; `bro task run` (create/start client); local v19 native socket (including cold Thread directory); opt-in `/agent/v2` HTTP API |
| **Managed native task** | Daemon `managed_task::owner` hosts the existing NativeSession/Core and DatabaseNativeStore journal. The product CLI uses bounded, instance-fenced managed v1; accepted operation identities stay in Core checkpoints. | `bro task managed`; `task managed-status`; `task managed-cancel`; owner-local `.managed.sock` |
| **CLI**                   | `apps/bitrouter` — the composition-root binary                                                            | `bro <subcommand>` |

Bare `bro agents` uses `connected_agents` to read the owner-local control
endpoint. ThreadService worker guards, supervisor live handles, native ACP
attachments and managed Core channel owners supply the projection; no second
runtime registry is maintained. Catalog (`agents list`) and retained runs
(`agents sessions`) stay separate. Coverage, unknown values and bounded,
boot-scoped pagination are described in [CLI reference](CLI.md#acp-agent-management).

`managed_task::local` is a separate v1 endpoint, preserving the Thread v19 and
HTTP managed-channel contracts. Its live registry owns native drivers, while
durable state stays in the original `bro_managed_sessions` row and journal
format. Same-key duplicates check Core's retained input receipt, including
after clean restart; cold status neither binds resources nor restores Core.
Only confirmed released journals can continue with the original session/harness
IDs and a new epoch. CLI disconnect leaves the driver running; Ctrl-C or an
exact boot/input cancellation requests joined settlement. Daemon cleanup owns
cancellation and driver joining even if an ingress server fails. Native library
embeddings remain available independently; no Thread IDs or mirror store are
introduced. Explicit directory/MCP skill bindings stay in the original Core
WorkUnit. MCP roots require independent content consent;
`managed-status` exposes the original requests and `managed-answer` answers an
exact boot/input/consent identity. Cold answer retries only return committed
receipts. Persisted roots and supporting files are reverified on reuse; native
`skill_read` delivers only selected manifest files as protected Turn-scoped user
data. Parent consent never authorizes a child's activation.

`thread_cli` forwards single operations to the same ThreadService over local
native v19, including explicit load/close/recover. `agent_sessions/control`
uses exact-scope supervisor grants and transient generation-fenced leases;
protocol v3 adds background skill constraints while ordinary v1/v2 operations
remain supported (v2 introduced boot-local ACK inspection). Supervised ACP reuses
the existing local directories and explicit candidate constraints. Its original
supervisor ledger acknowledges selection and the prepared receipt before ACP
dispatch; no automatic decision request or skill database is introduced. Codex receives local text instructions; Claude requires negotiated
embeddedContext and receives user resources. MCP-origin external allocation is
unsupported. Vendor model token/history admission remains outside this guarantee.
Neither adapter starts a second scheduler, stores mirrored execution state,
or acquires control for observation. `control_output` shares endpoint selection,
request identities and plain JSON/NDJSON output. Evolution candidate/checkpoint
CLI adapters call the existing live owner. See [controls](CLI.md#single-operation-native-and-acp-controls)
for runtime/control-boot identity domains and recovery contracts.
Native v19 rejects older clients before their blanket task-approval policy can
answer a skill request. HTTP v2 advertises explicit skill consent and rejects
generic `/inputs` answers to an MCP skill unless `skill_consent: true` is set.
ACP grants are confirmed against the discovered boot before use and are never
refreshed across restart. Evolution local control negotiates v1 and sends an
instance-fenced operation checked by the daemon before owner dispatch; the
legacy operation remains for existing clients without serving as CLI fallback.

`mcp_config` edits selected source entries using the existing config validators, publication lock and atomic writer in `policy_lock`. Onboarding publication shares that lock. MCP config CRUD never constructs a gateway/client or queries the daemon; saved-only receipts direct running-state inspection to `bro status`. Native MCP changes remain startup-owned. See [MCP configuration](CLI.md#mcp-configuration-and-gateway-inspection).

G09 adds explicit native source policy through `skills.mcp_sources`, separate
from ordinary MCP tools. The original caller/workspace owner admits full skill
entries and source-bound reads; each network/cache use verifies raw bytes and
root frontmatter. Read-only owners expose no MCP tools and connect only to
granted skill sources. Policy changes are startup-only and reported as `skills`
by the existing configuration-state/reload coordinator. These primitives do
not alone prove activation. Core binds explicitly selected entries and persists per-skill consent
before root reads. The root and complete entry enter source-tagged user data,
with protected provenance excluded from automatic child sharing; only a fixed
handling policy enters system context. Root cache reuse preserves the original
entry and verifies the original bytes. Supporting files retain the same source,
entry, consent and per-file byte commitments; admission checks their frozen Turn
allowance and final model/context budget without truncating content.

`skills/directory` owns CLI discovery and direct file reads/writes, reusing the
shared format parser and publication lock/atomic writer. Acquisition belongs to
`npx skills`; BRO owns no installation index, Git downloader or version store.
`skills/native` implements `SkillDirectory` over existing directories. It freezes
path/caller/workspace and a complete content manifest for explicit native/ACP
bindings; each use checks actual bytes and containment. Changed local files
reject the old binding instead of reading a retained version. New Turns can bind
new content. `skills explain` reads original native Thread/Turn or managed input
checkpoints without an App, source reads, owner restoration or execution.

Native Core and ACP skill delivery only select explicit includes. Native context
preparation keeps complete admitted evidence and never calls the decision backend
for classification, skill relevance or lossy view choice. Explicit evidence tools,
checkpoints, tool permissions, budgets and conditional recovery remain. SDK model
routing assessment and provider Decisions remain separate consumers. Historical
decision records stay readable for validation/accounting without new dispatch.

The first-party TUI and its app drivers have been removed. `apps/bitrouter`
uses ordinary CLI reports and line-based onboarding, with no Ratatui/Crossterm
dependency. Native launchers retain the harness's own interface.

ACP permission policy/replies, prompt-template resolution, journal projection
and plain transcripts live in `bitrouter_orchestrator::acp`. App prompt-template
validation and command discovery remain in `actions/session.rs`; process-log
diagnostics remain in `session_diagnostics.rs`. `agent_sessions.rs` retains
protocol clients/scopes/leases, while `agent_sessions/control.rs` owns the
single-operation CLI. Read-only control never acquires a mutation lease.

The daemon, ThreadService, Core, Harness and ACP controller remain execution
authorities. No renderer, composer, UI event loop or mirrored execution database
is retained. UI design documents are historical; current boundaries and
migration evidence are in the [CLI-only spec](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md)
and [implementation record](CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md).

BRO native tools are `read`, `glob`, `grep`, `write`, `edit`, and `shell`.
`read` dispatches known workspace paths to file lines or direct directory
entries. The three inspection tools form the read-only profile. The server
resolves one interpreter before a coding execution; model commands and
verification share it. See [the six-tool contract](BRO_BASE_TOOLS_SPEC.md).

The native BRO runtime has one `ThreadService` per serving instance. App-owned
SQLite stores Thread/Turn facts, acceptance keys, public events and owner/version
fences. Clients only project state; detaching does not cancel, approve or resume.
Bounded tool workers, ordered exclusive effects, FIFO/steering, joined shutdown,
workspace locks/markers and bounded startup discovery preserve execution barriers.
Cold queries do not execute. Safe same-owner reload and explicit stopped-owner
checkpoint recovery preserve identities and budgets without replaying completed
effects; lost owners, unknown effects and uncertain accounting remain blocked.

Use [the standalone runtime contract](BRO_AGENT_RUNTIME_SPEC.md) for current
requirements, including root format 2 and local v19/HTTP v2. The
[runtime acceptance index](BRO_AGENT_RUNTIME_IMPLEMENTATION.md) separates local
fixtures, historical provider/platform checks and remaining product gates.
[Conversation UI](BRO_CONVERSATION_UI_SPEC.md) and
[six tools](BRO_BASE_TOOLS_SPEC.md) specialize that contract. Older phase ledgers
are historical references, not competing descriptions of current behavior.
Future [core/harness separation](BRO_AGENT_RUNTIME_HANDOFF.md) assigns live
scheduling/checkpoint semantics to Core and durable storage/ACK authority to
Harness; the standalone implementation does not prove that integration.
Router metering and external ACP session ownership retain their own boundaries.

The CLI is the **host** interface: it owns `main()` and mounts the other
interfaces as subcommands. The SDK owns protocol interop such as the MCP
gateway and ACP pipeline; the app owns local actions, composition, and human
presentation.

### Dependency Logic

Model semantic types, wire codecs, authentication contracts and credential values
have one owner in **`bitrouter-ai`**. The SDK, provider integrations and application
consumers depend directly on AI; AI has no SDK, application or agent-runtime
dependency. AI owns three generation codecs, the native Decisions codec, SSE framing and selected-model HTTP
invocation (`ModelClient` and `HttpTimeouts`); its errors carry domain facts rather
than gateway status policy. Explicitly registered `AuthApplier`s can shape a body,
authenticate and recover the same selected account once after a 401. `ModelClient`
performs no implicit catalog loading, account selection, login or routing fallback.

Native Decisions uses `DecisionRequest`/`DecisionResult` and
`ModelClient::decide`. SDK envelopes distinguish `PipelineInput::Generation`
from `PipelineInput::Decisions`, and the output variants likewise retain their
semantic operation. The HTTP gateway serves `POST /v1/decisions` through the
shared execution, delivery and settlement lifecycle. Streaming and generative
continuation/tool defaults are rejected or scoped before native dispatch.

Routing selects a compatible operation before preferred-protocol matching.
`ModelTarget.api_protocol` determines the AI client's outbound wire. Existing
generation calls prefer a supported inbound wire or the first configured
generation wire; the enum/unknown-host fallback is Chat Completions.

Route hooks may capture evidence in `after_resolve`, after every mutable
`resolve` hook and operation/effort filtering. App-owned `CaptureTariffs` freezes
each effective target's independent protocol tariff and endpoint profile here.
Published tariffs retain their optional `endpoint_profile` provenance across
registry merge and configuration changes, so global rates cannot silently rebind
to a regional endpoint. Legacy/custom tariffs without a declaration bind to the
configured endpoint. Metering and evaluation use that snapshot; `UsagePricingSnapshot` projects the
same rates to the SDK's conservative stream usage selection. Unknown frozen
prices disable live price lookup. Custom hosts composing `MeteringRecorder`
must also install `recorder.tariff_capture(require_known)` at the route stage.

`server.require_known_pricing` is opt-in and restart-required. It rejects routes
without guaranteed price coverage before I/O, including native Decisions while
cache billing remains unverified. Prices and endpoint profile anchors also
require restart. Native nonzero cache usage retains counters and raw evidence
with an unavailable cost. See [Decisions specification](DECISIONS_API_SPEC.md)
and the shipped [setup reference](../skills/bitrouter/references/decisions.md).

Codex subscription `generate` calls use upstream SSE and AI's canonical stream
collector; the SDK shares that collector and retains its gateway request policy
and execution envelope. Collection requires a terminal and consumes through EOF
before returning success. It preserves fields already present in canonical
events. Completed Responses reasoning now has a redacted AI-owned native capsule
carrying the final item payload. Responses JSON/SSE output retains its identity,
opaque encrypted content and native summary/content fields, while visible text
lanes remain separate. Snapshot consistency checks prevent native data from
restoring text changed by hooks. Responses ingress retains native reasoning, including
encrypted-only items. Native replay still requires future target/account authority
proofs. AI's content-free `ConversionReport` covers initial native-reasoning and
structured-output admission; direct invocation and SDK HTTP preflight use the same
request preparation. Router exclusions reach `ObserveHook::on_conversion_excluded`
before hop observers or provider execution, while an all-excluded chain returns a
typed incompatibility report. Other client codecs return output incompatibility
when they cannot retain native reasoning. Gateway status mapping stays in the SDK.
Built-in request parsing now also refuses initial ingress omissions before any partial
Prompt can enter the pipeline: unknown items/blocks, unsupported content values,
lossy system/tool-result shapes and unsigned Messages thinking.
Reports carry original wire indices and remain content-free. HTTP handler tests verify
400 responses before any executor call in ordinary and streaming modes. Shared request admission now also excludes initial structural history losses: omitted
reasoning/sources, provider execution or MCP identity lost in projection, unsupported
approval parts/reasons and omitted tool-result media/file IDs. Existing native-family
tool declarations remain eligible; unknown cross-family translation and non-object
argument loss are refused. Native continuity tokens cannot cross a wire that omits them. Native token
presence/representation and local denial pairing are not complete authority proofs.
Projection now also refuses explicit strict flags, schema names/descriptions and file
names that the selected codec would omit, plus tool-result error/denial status loss.
Google Chat admits a conservative target-scoped schema/option subset without rewriting constraints. Explicit cache resources and unverified strict/schema metadata are refused. A suppressed paired denial cannot lose its supplied reason.
Malformed argument strings cannot become empty objects on Messages. Chat and
Messages preserve text-only tool-result arrays, and Chat preserves multiple text parts.
Unclassified Chat ordering/reasoning concatenation and selected three-wire ingress
attribute/boundary losses are refused. Value-preserving JSON encoding into
string-only tool slots remains eligible; error/denial status is checked separately.
`ConversionReport.admitted` retains classified JSON encoding separately from refusal issues.
Selected-target preparation can return
the report through `ModelClient::render_request_with_report`; executor preflight returns
that assessment. The SDK rechecks refusals and invokes `on_conversion_admitted` for
eligible detected effects before hop start, without counting a candidate as an HTTP
attempt. Empty assessments emit no conversion callback. Remaining ingress attributes,
media/options/boundary effects, schema/model validation, other admitted classifications,
Core/durable recording, output effects and degradation authorization remain open.

`bitrouter-ai::catalog` owns the consumed provider/model schema, explicit bounded
refresh and source-keyed injected stores. A catalog starts empty with memory-only
storage; loading and network permission are separate explicit operations. Failed
parse, validation or persistence retains its prior snapshot. `apps/bitrouter/src/catalog.rs`
selects the 24-hour file cache and startup/reload refresh policy; `bundled_registry.rs`
supplies the committed offline baseline for the default public source. Unbound
legacy caches are preserved but not adopted as an assumed source. Activation,
credential lookup and user-config merge remain in the application/provider bridge.

`bitrouter_ai::providers::{anthropic,antigravity,claude_code,codex,copilot,hosted,supergrok}` implements
request-time authentication and adaptations over caller-injected sessions/stores. Generic OAuth
response decoding and bounded refresh live in `auth::oauth`; browser/login UI,
CLI import and file-path policy stay above AI. Application assembly constructs
the native sessions from its selected file backend and OAuth registration.
The selected default account label is owned by `auth::store::DEFAULT_ACCOUNT`.
Anthropic's API-key applier receives a credential backend directly. Claude Code
receives an `OAuthSession` and an explicitly permitted fallback token. Its live
CLI adoption backend and environment capture remain in
`bitrouter::providers::claude_code`; application assembly chooses those sources
and the public Anthropic OAuth client registration. These are explicit inputs;
AI performs no ambient source discovery or interactive login.
Explicit device authorization and bounded polling now live in
`bitrouter_ai::auth::device_code`. AI's opt-in `pkce` feature supplies
`auth::{pkce,auth_code,listener,login}` and `providers::login` registration inputs.
The application's `LoginUx` implementation supplies terminal hints and callback
HTML; the library owns state/redirect validation and the caller's total async
login deadline. The application retains its existing 15-minute deadline and
login-method/account selection. Dropping the library future releases its
listener and pending async callbacks; already-started blocking stdin work
remains an application limitation.
AI's `hosted-login` feature supplies `providers::hosted::flow` over explicit
`LoginParams`, including device authorization, deadline-bounded polling,
revocation and complete hosted envelope construction. Cloud settings/default
scopes and file persistence remain above AI. The `hosted` feature alone still
supplies request authentication/refresh without login's random-number dependency.
AI's opt-in `file-store` feature now owns ordinary explicit-path snapshots in
`auth::file::snapshot` and selected-account leases in `auth::file::backend`.
`apps/bitrouter/src/provider_credentials.rs` retains the existing XDG/Windows/home
location precedence and `oauth-tokens.json` filename. Reads preserve labeled and
legacy formats without writing; a later mutation uses the labeled format.
Administrative writes and refresh/composite backends bind to the same selected
path, including existing file/directory aliases and missing parent directories.
Relative paths bind at construction rather than following a later cwd change.
The [revised providers-retirement plan](BITROUTER_AI_REFACTOR_SPEC.md#revised-next-steps-retire-bitrouter-providers)
now places product activation/configuration and credential discovery in
`apps/bitrouter/src/providers`, and hosted account persistence/settings/assembly in
`apps/bitrouter/src/cloud/account`. The old providers package is removed locally.
Model, Cloud management and telemetry retain the same shared AI hosted session;
external consumer migration and publication remain separate verification work.
Native Gemini Generate Content and the private `google-ai` Antigravity transport are retired. Metered Google calls use the official Chat endpoint. Tool signatures retain canonical metadata and a selected-credential replay proof; clients must echo the entire tool-call `extra_content`. The proof is removed before upstream dispatch. See [retirement progress](GEMINI_PROTOCOL_RETIREMENT_PROGRESS.md) for the actual SDK limitation and live validation gates.

`ModelTarget` carries effective connection values, compatibility and an optional
caller-selected account label. SDK routing, header policy, continuation admission
and public error policy remain above AI. Stable credential/scheme proof primitives
live in AI; the SDK enforces bindings against the final outbound request. SDK
`HttpExecutor` delegates projection, HTTP I/O and decoding while retaining its
pipeline retry budget, hooks and settlement integration.

AI's injected account store holds a transaction through OAuth rotation and commit.
After acquiring the lease, `OAuthSession` owns completion independently of a dropped
caller future. The Codex applier uses this contract with either injected memory
storage or AI's explicitly constructed optional file backend. The file backend serializes
same-account refresh in process, preserves unrelated file entries, and rejects a
commit after login/logout replaces its original credential. Failed persistence
retains the rotated replacement for a later commit; this is volatile recovery,
not a claim of crash durability or cross-process exclusion. Ordinary subscription providers and Claude CLI adoption also use these transactions.
Explicit call overrides outrank stored credentials; permitted fallback is used only
for an absent slot. Copilot caches are source-bound. Google Chat tool continuity is bound to the effective static credential, endpoint and model. Hosted OAuth uses the richer owned
transaction contract in AI's optional `hosted` feature. `HostedSession` receives
a selected slot, full-envelope store and HTTP client; its shared metadata/token
decoders also serve application-driven device login. The application account
manager assembles a process-local file backend, while model calls, telemetry and
Cloud management all resolve through the same AI session. File policy, account
activation, CLI defaults and onboarding text stay above AI. See
[implementation progress](BITROUTER_AI_REFACTOR_PROGRESS.md).
**`apps`** composes the libraries, and the SDK never depends back on the host.

Note what that does *not* say: it constrains the direction of dependencies, not how much lives in the SDK. A capability belongs in the SDK when it is an **interop surface** — a *contract* the SDK's own domain model is rendered into, which must be identical across deployments to mean anything — and it goes behind a default-off feature so consumers who skip it pay nothing. Deployment business logic (auth, policy, charging, metering, content policy) stays out, whether it would point down cleanly or not.

Observability is the case in point, and it is the one the workspace previously got wrong in both directions. The contract is the **span schema** — the span names (`chat`, `route`, `settle`, the per-hop `chat`), the `bitrouter.*` attribute vocabulary, and the invariants that fail silently when a deployment re-derives them wrong (a hop is not a `gen_ai` generation; stamping it as one makes gen_ai-aware backends double-count cost). *That* is BitRouter semantics, it lives in `bitrouter-sdk` as `observe`, and it is behind no feature gate at all, because a deployment implementing the contract must not have to enable a renderer it is not using.

Rendering the contract onto a wire is not the contract. OTLP transport, bearer refresh, batch processing, endpoint configuration and cardinality limiting are one egress path's implementation — by volume, roughly 40% of the old module's production lines were transport and vendor glue and another 17% deployment configuration, against ~43% span semantics — and they ship in `bitrouter-telemetry`. They were briefly folded into the SDK behind a default-off feature; that placement cost the foundation crate two permanent 0.x public dependencies and needed three separate paragraphs of "do not read this as a precedent" to hold its shape. Both are gone. `ObserveHook` is the seam the renderer plugs into, and it is a seam with more than one production implementation: `apps/bitrouter` registers its own observers beside the OTLP one. See [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

Because the schema is the contract, it is written down rather than inferred from a renderer's call sites. `crates/bitrouter-sdk/src/observe/schema.rs` declares every span, attribute, event, metric and silent-failure invariant, names no `opentelemetry` type, depends on nothing but `serde`, and renders to the committed artifact `crates/bitrouter-sdk/span-schema.json` — regenerate with `UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter-sdk committed_artifact` (default features: the module is ungated, and a staleness guard that only fired under `--all-features` would let the artifact rot everywhere else), and the ordinary test run fails when it is stale. It sits beside `public-api-deps.txt`, the crate's other generated manifest — but unlike that one it *ships* with the crate, because it is the interop surface. The declaration is `pub` for the same reason: a second renderer needs it at compile time, not just as JSON. The three helpers only a conformance suite calls — `span_def_for`, `value_type_matches` and `render_json` — sit behind the default-off `testing` feature instead, since public API with no production caller is what CLAUDE.md rule 4 forbids; the feature carries no dependency, so a renderer enables it under `[dev-dependencies]` and pays nothing. Conformance tests in `bitrouter-telemetry`'s `otel/exporter.rs`, `otel/acp.rs` and `otel/http_layer.rs` drive real lifecycles and assert that nothing reaches the wire the declaration does not describe, so the artifact is checked rather than aspirational. See [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

1. **`bitrouter-sdk`** — the pipeline and gateway foundation. Knows nothing about which providers exist or how the binary is wired. It consumes model semantics from `bitrouter-ai` and owns:
   - **Three independent pipelines**, one per wire family. They are deliberately *not* generic over a shared hook trait — each has its own hook set:
     - `language_model` — the main pipeline: LLM completions with the full hook chain (pre-request → route → execute → settle), an interleaved stream stage, and read-only observation.
     - `mcp` — Model Context Protocol routing (pure routing, no settlement).
     - `acp` — Agent Client Protocol routing (pure routing, no settlement).
   - **Hook traits** — `PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook` — trusted host assembly interfaces used by builtins and legacy packages. New request-check extensions use the restricted author API rather than mutable pipeline hooks.
   - **Config + routing** — YAML parsing, `${VAR}` substitution, the `ConfigRoutingTable`.
   - The **axum HTTP server** and the `App` builder.
2. **`bitrouter-ai`** — owns catalog metadata, protocol codecs, selected-target invocation and authentication. It has no SDK or application dependency. Explicit login and ordinary file storage are opt-in features; external CLI/Keychain discovery, account selection, product activation and Cloud management remain in the application.
3. **`bitrouter-guardrails`** provides the regex matcher and the SDK request-check callback; its explicit `sdk` feature enables legacy global/stream hooks. It depends on `bitrouter-sdk` with default features disabled. The default host still has no normal/build dependency on the matcher. **`bitrouter-telemetry`** implements SDK hooks; telemetry's whole OpenTelemetry stack sits behind `otel-*` and its ingress span behind `server`, so `cargo add bitrouter-telemetry` on its own pulls neither. The `feature-isolation` CI job enforces all of it, plus the invariant that gives the split its point: **no `opentelemetry*` crate is in `bitrouter-sdk`'s tree at any feature combination**, and the two OTLP transports stay isolated from each other.
4. **`apps/bitrouter`** — owns `providers::{apply,builtin,entry,registry,import,claude_code,antigravity}` for configuration, activation and credential-source policy; `cloud::account::{credentials,transaction,manager,settings}` retains Cloud account files and shared AI session assembly. The only compiled-in provider entry is `apps/bitrouter/providers/bitrouter.toml`; other providers come from AI catalog metadata and the product registry bridge. The app assembles the default host without a guardrails matcher dependency. The assembly layer (`assemble.rs`) turns a parsed `Config` into a running `App` by wiring the builtin hooks (auth, policy, metering, observability) and router-bound native request checks onto the `language_model` pipeline; `main.rs` is a thin CLI shell over that library.

### Extension authors and host assembly

New request-check authors use an ordinary function accepting
`bitrouter_sdk::extension::ExtensionApi` and call
`request_check(id, revision, callback)`. Business inputs and decisions live in
`extension::request_check`, without a wire envelope. The custom host passes the
registration function to `host::serve_with_extensions` for the shared foreground
daemon lifecycle, or `assemble::build_app_with_extensions` for low-level embedding.
Fragment and coverage types also live in `extension::request_check`.
Router bindings
control execution scope and order; registration is not global activation.
Beta supports compiled extensions only: no HTTP checker service, wire crate,
probe command, runtime installation or extra extension API crate.

Valid unconfigured registrations are inactive with sorted startup diagnostics;
configured instances require a matching registration even without bindings.
The regex example uses the same inference, local/remote management, reload and
shutdown path as `bro serve`. Restart custom hosts with their own executable;
the example does not implement the default CLI's background-launch protocol.

The SDK entry does not expose host builders, migrations, credentials or mutable
pipeline context. The host retains resource bounds, fixed binding identity and
bounded content-free tracing. Synchronous callbacks cannot be forcibly
terminated by deadlines; they are trusted in-process code.

Legacy `Plugin` / `AppBuilder::plugin` and optional `GuardrailsPlugin` retain
custom-host global, stream/output and migration semantics. Input-only checks do
not replace those capabilities. The recently introduced app ExtensionApi and
Native map assembly entry migrate to the SDK entry. `PluginId`, existing
`Config::plugins` consumers, Context extensions and external agent-plugin
manifests retain their separate meanings. See [migration](GUARDRAILS_EXTENSION.md).

### SDK feature flags

The SDK keeps its default dependency tree minimal — capabilities that pull weight are feature-gated:

| Feature        | Pulls in                              | Purpose                                                       |
| -------------- | ------------------------------------- | ------------------------------------------------------------- |
| `server`       | axum, tower                           | The HTTP server, SSE handlers, admin endpoints                |
| `config_file`  | serde-saphyr, `tokio::fs`             | YAML `bitrouter.yaml` loading                                 |
| `mcp`          | rmcp                                  | The bundled `RmcpExecutor` for the `mcp` pipeline             |
| `testing`      | nothing                               | `observe::schema`'s conformance helpers (`span_def_for`, `value_type_matches`, `render_json`) — for a renderer's `[dev-dependencies]`, never a production build |

ACP is no longer an SDK feature. The controller, client, capture port, telemetry, translation and agent-process transport live in `crates/bitrouter-orchestrator/src/acp/`; the orchestrator's optional `acp` feature owns the official ACP SDK and conductor, and the app enables it. `config::agent` retains only `AcpAgentConfig`, `AcpTransport` and their validation error, so existing `agents:` YAML and schema stay compatible without pulling the protocol runtime into the SDK. The conductor still reaches the trace viewer and its HTTP server, but that dependency is confined to the orchestrator's ACP feature. Native-only consumers leave that feature disabled. CI checks that no ACP runtime package reaches the SDK, even under `--all-features`.

`observe` — the span schema and the `SpanAttributes` hatch — is otherwise **not** in this table: the declaration itself is ungated and carries no dependency beyond `serde`. Only the three helpers a conformance suite calls sit behind `testing`, which is why that row's "Pulls in" column is empty.

### `bitrouter-telemetry` feature flags

| Feature        | Pulls in                              | Purpose                                                       |
| -------------- | ------------------------------------- | ------------------------------------------------------------- |
| `otel`         | (selects `otel-http`)                 | OTLP export of the span / metric model — the entry point       |
| `otel-http`    | opentelemetry\*, tracing-opentelemetry, tracing-subscriber, opentelemetry-http, dashmap | The above over OTLP/HTTP + protobuf (reqwest + rustls) |
| `otel-grpc`    | the same stack plus tonic             | The above over OTLP/gRPC (tonic + native trust roots)          |
| `server`       | axum, http-body, pin-project-lite, `bitrouter-sdk/server` | The inbound ingress SERVER span, as a middleware over the SDK's router |

> **The public-dependency commitment lives with the renderer, and that is the point of the split.** `otel::subscriber::tracing_subscriber_layer` returns `impl tracing_subscriber::Layer<S>`, so **tracing-subscriber 0.3** and **tracing-core 0.1** are semver-committed public dependencies — of `bitrouter-telemetry`, which is default-off and which nothing else in the workspace links. They were public dependencies of `bitrouter-sdk` for one release cycle, where a `tracing-subscriber` 0.4 would have forced a breaking release on every consumer including the five that never enabled `otel`. `crates/bitrouter-sdk/public-api-deps.txt` records the removal. `server` is split out because the two halves have different consumers: a deployment that builds its own ingress span and only wants the bridge should not compile axum for it — the out-of-tree consumer is exactly that, since it installs its own `TraceLayer` so a public multi-tenant edge does not let callers control its trace ids or sampling.

Without `mcp` / `acp`, the SDK still exposes those pipelines, hook traits, and transport enums — a consumer can plug in a custom `Executor` without pulling rmcp or the stdio bridge.

> **Rule of thumb:** a feature exists only if disabling it removes a non-trivial set of dependencies. Pure module-visibility toggles are not features — the module is always compiled.

## Request Flow

A streaming LLM request moves through the workspace like this:

1. The `bitrouter` binary resolves the config source (see *Configuration*), loads or synthesises a `Config`, and `assemble.rs` builds an `App` — the `language_model` pipeline with the builtin hooks wired on.
2. The SDK's axum server receives the inbound HTTP request on one of the protocol routes and the matching **inbound adapter** parses it into a canonical `PipelineRequest` (model name, messages, tools, params).
3. The `language_model` pipeline runs its stages:
   - **Pre-request** — local policy hooks, followed by explicitly bound external request checks. Authentication and normalization occur before router admission.
   - **Route** — the `RoutingTable` resolves the model name to a fallback chain of `RoutingTarget`s (provider + upstream model id + protocol); `RouteHook`s may rewrite the chain.
   - **Execute** — the executor dials the first target; on failure the `FallbackPolicy` decides whether to try the next. The **outbound adapter** for the target's protocol renders the provider request and decodes the provider response (and its SSE stream).
   - **Settlement** — every `SettlementRecorder` runs (metering, etc.), success or failure.
4. For streaming, the canonical `StreamPart` stream flows through the `StreamHook` stage and is re-encoded by the inbound adapter into the **client's** protocol — so a client written for the Responses protocol can transparently use a Messages upstream, and vice versa.
5. `ObserveHook`s receive read-only lifecycle events throughout; `bitrouter-telemetry` turns them into OTLP traces and metrics, and the binary's own observers consume the same events.

The `mcp` and `acp` pipelines are simpler: pure routing with no settlement.

## Configuration Model

### Config source resolution

When a subcommand doesn't pass `-c <path>`, the binary walks a fixed order (see `apps/bitrouter/src/paths.rs`):

1. **`-c <path>`** — explicit; a missing file is a hard error.
2. **`./bitrouter.yaml`** in the current directory.
3. **`$BITROUTER_HOME/bitrouter.yaml`** — if the env var is set, that file must exist.
4. **`~/.bitrouter/bitrouter.yaml`** — used if present.
5. **Zero-config in-memory defaults** — when nothing above exists. No file is written; `bro init` is the explicit way to scaffold one.

The daemon `chdir`s into the bitrouter home (the config file's directory, or `~/.bitrouter` for zero-config) on startup, so relative paths in the config — `database.url`, `server.control_socket` — and the socket / pid / log all resolve against one stable location.

### Zero-config and the provider catalog

In zero-config mode `bitrouter::providers::apply::zero_config()` builds a `Config` with `skip_auth: true`, `listen: 127.0.0.1:4356`, and the compiled-in hosted gateway auto-enabled when its API key is set in the environment. Every other public provider comes from the fetched-or-cached registry merge: an env-keyed registry provider becomes active when its credential is available, and a local-OAuth provider becomes active after `bro providers login <provider>`.

## HTTP Server Surface

The axum server lives behind the SDK's `server` feature (`crates/bitrouter-sdk/src/server.rs`):

| Route                               | Handler                          |
| ----------------------------------- | -------------------------------- |
| `POST /v1/chat/completions`         | Chat Completions inbound         |
| `POST /v1/responses`                | Responses inbound                |
| `POST /v1/messages`                 | Messages inbound                 |
| `POST /v1/decisions`               | Native Decisions inbound         |
| `GET  /v1/models`                   | model catalog listing            |
| `POST /mcp/{server}`                | MCP gateway (JSON-RPC proxy)     |
| `GET  /metrics`                     | OTLP-migration banner (see below)|
| `GET  /health`                      | health check                    |

`GET /metrics` is retained for endpoint compatibility only. Prometheus accumulation was removed: metrics are now *pushed* over OTLP by `bitrouter-telemetry`, and the endpoint serves a short banner pointing at `plugins.bitrouter-telemetry.otel` (`EmptyMetricsRenderer` in `apps/bitrouter/src/assemble.rs`). The SDK's `MetricsRenderer` trait and its `text/plain; version=0.0.4` content-type default still exist — a deployment that wants a real pull-based endpoint implements the trait itself.

Daemon mutation (`stop` / `restart` / `reload`) runs over a Unix domain socket.
When the optional authenticated HTTP control listener is enabled, it exposes
typed operator reads and reload operations behind its own bearer gate and
loopback exposure policy. It does not mount an MCP origin endpoint. See
`apps/bitrouter/src/{daemon,remote_control}.rs`.

## CLI Surface

Bare `bro` runs line-based first-run onboarding when needed, then ensures a ready background daemon and returns. It uses the lifecycle config precedence, startup lock, pending-process record and control-socket status probe; no chat agent is required. It never implicitly reloads an existing runtime. Prompts/secrets use canonical terminal lines with independent echo control; no TUI selector. `bro <subcommand>` exposes lifecycle and reports plus agent-facing entries: `task run` (BRO headless), bare `code` (BRO interactive), `launch <agent>` (native interface), `code <agent>` (ACP UI), `run <agent>` (headless turn), and `acp serve` (native v1/v2 ingress) and `acp serve <agent>` (external manager endpoint). `claude`, `claude-code`, and `codex` are native shortcuts. `requests` is the canonical request-history report; `mcp check [server]` inspects configured upstream MCP servers. Old `spawn`, `chat`, `tui`, `tools`, and related compatibility leaves are hidden. See `apps/bitrouter/src/main.rs`.

### Observability surface (`bro requests`)

One surface, and no module of its own. `RequestsReport`
(`apps/bitrouter/src/output/reports/requests.rs`) is an ordinary `CliReport`:
the builder in `main.rs` polls the metering store and the control socket, and
`Human::table` / `Human::status_block` render it. There is no second table
implementation and no terminal-only path.

The `apps/bitrouter/src/tui/` module it replaced is **deleted**. It had become a
618-line private implementation of one public function that bypassed `Output`
(so `--json` was silently ignored) and reimplemented `output/human.rs`'s table.
Its signal arm moved to `chat/signals.rs`, next to its only caller.

All first-party terminal rendering has been removed. `bro requests` exposes
request state as ordinary JSON/human reports. `bro launch` uses inherited
terminal I/O only for the external harness's own interface. PTY infrastructure
is a dev dependency for onboarding, launcher and process behavior tests.

`spawn::prepare` still builds the child once and `exec_inherited` runs it; the `Prepared` seam is kept independent of hosting.

## Where To Extend The System

### Add or update a provider

Add a provider definition under `registry/providers/*.yaml` (the registry source; `dist/` is regenerated by `helpers/dist-helper`). `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` — it is resolved from the environment at merge time (e.g. Bedrock `https://bedrock-mantle.${AWS_REGION}.api.aws/v1`). For stateful auth (OAuth, token-exchange), add an injected `AuthApplier` impl in `crates/bitrouter-ai/src/providers/` keyed by the registry `auth.handler` and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`). See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for the step-by-step.

### Add a new wire protocol

Protocol adapters live in `crates/bitrouter-ai/src/protocol/`. A new protocol needs an inbound adapter (parse request / encode response + SSE), an outbound adapter (render request / decode response + SSE), a variant on `ApiProtocol`, dispatch wiring, and coverage in the protocol-conversion test matrix.

### Add a provider whose wire isn't HTTP+JSON+SSE

Rare — no current registry provider needs this. The big clouds (Bedrock, Azure) speak one of BitRouter's built-in protocols over SSE and are ordinary Bearer registry providers. Only if an upstream uses a wire an existing outbound adapter can't decode (e.g. a vendor SDK's binary event-stream) do you add an `ApiProtocol::Custom` outbound adapter + `Transport` in a standalone crate, registered on the dispatch executor at startup. See the `Custom` escape-hatch docs in `crates/bitrouter-ai/src/protocol/mod.rs`.

### Add a hook (auth, policy, metering, guardrail, observability)

Implement one of the SDK hook traits (`PreRequestHook`, `RouteHook`, `ExecutionHook`, `StreamHook`, `SettlementRecorder`, `ObserveHook`) and wire it onto the pipeline in `apps/bitrouter/src/assemble.rs`. A hook that brings real dependency weight belongs in its own `crates/` library behind a default-off feature — the telemetry pattern. Guardrails run as an independently deployed input checker in the default product; the library's optional SDK hooks are for explicit trusted-host assembly. What goes in the SDK is the *contract* such a hook binds to, not the hook: `observe::schema` is in the SDK, its OTLP renderer is not. A lightweight hook can live in the binary.

### Embed the SDK in your own service

`apps/bitrouter/src/assemble.rs` is the worked example: it builds an `App` via `App::builder()`, registers the `language_model` pipeline with a routing table, an executor, and the hook chain, then serves it. A consumer that wants BitRouter's routing + protocol conversion without the stock CLI composes the same builder with its own hooks and routing table.

## Validation

Before opening a change, run the workspace checks from [`CONTRIBUTING.md`](../CONTRIBUTING.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

CI additionally runs `doc` (rustdoc under `-D warnings`), `doctest`, `feature-isolation` (the SDK's default tree stays free of axum, the OTel stack never reaches the SDK at any feature combination, and the two OTLP transports stay isolated), `sdk-public-api` (see below), and `msrv` (pinned to Rust 1.93). AI agents should also read [`CLAUDE.md`](../CLAUDE.md).

### The `sdk-public-api` job

`bitrouter-sdk` owns the observability contract without owning a renderer of it, which only holds while no `opentelemetry*` or `tracing_opentelemetry` type appears in any public SDK signature. `feature-isolation` now proves the stronger form — the stack is not in the SDK's tree at all — so this job is the *forward* guard: it is what catches the next change that pulls a renderer back in. The job renders the SDK's entire public surface with [`cargo public-api`](https://github.com/cargo-public-api/cargo-public-api) — one fully-qualified line per public item — greps that rendering for forbidden types, then reduces it to the set of foreign crates the public API reaches and diffs that against `crates/bitrouter-sdk/public-api-deps.txt`. That second half is the broader guard: a grep can only find a type you already know to look for, whereas the manifest catches the SDK quietly gaining a *new* public dependency — which is how `tracing_core` got there, through a generic bound. Full rationale in [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md).

**If this job fails on your PR**, read which step failed. A forbidden-type or re-export failure is a real design problem — keep the OTel type out of the public signature (make it `pub(crate)`, or return `impl Trait`), do not regenerate around it. A public-dependency failure means your change added or removed a crate from the SDK's public API; a `+` line means an upstream breaking release in that crate now becomes a BitRouter breaking release, so confirm that is intended before regenerating:

```sh
rustup toolchain install nightly-2026-05-05
cargo install cargo-public-api --locked --version 0.52.0
cargo +nightly-2026-05-05 public-api \
  -p bitrouter-sdk --all-features --simplified \
  | grep -oE '\b[a-z_][a-z0-9_]*(::[a-zA-Z_][a-zA-Z0-9_]*)+' \
  | cut -d: -f1 | sort -u \
  | grep -vxE 'bitrouter_sdk|core|std|alloc'
```

Append the output under the comment header in `public-api-deps.txt` — the header is stripped before comparison, so keep it out of the generated part. Both versions are pinned in `crates/bitrouter-sdk/public-api.pins`, which the CI job reads; take them from there rather than from this snippet if the two ever disagree. They are pinned because the manifest is derived from `cargo public-api`'s rendering, and both rustdoc's JSON format and that rendering change across releases. Run it on Linux or macOS; the SDK has `#[cfg(unix)]` items, so a Windows-generated listing will not match.

### Named-router request checks

Named-router entry checks use the SDK's typed `language_model::request_checks`
contract. The app assembles startup-owned compiled registrations in
`request_checks`; a custom host supplies callbacks through
`extension::ExtensionApi`. The official `bro` binary has no custom
registrations, and there is no request-check inventory or receipt store.

Authentication and session/continuation normalization precede router binding.
The logical router/check binding remains fixed through candidate preparation,
model selection and fallback. One preparation path serves both response modes.
Checked routers apply effective defaults before content checks; ordinary local
hooks cannot change their selector afterward. Unguarded requests preserve the
legacy pre-request rewrite/defaults timing. The existing model selector and
executor run after required checks allow. Local policy rejection never invokes
a registered checker. The runtime emits bounded, content-free tracing for
registration and invocation outcomes; it does not expose checker inventory,
progress reporting or durable workflow state. Registration readiness and a
real invocation remain separate evidence.

See [REQUEST_CHECKS_SPEC.md](REQUEST_CHECKS_SPEC.md) for coverage, resource limits,
activation and the acceptance ledger. The default host no longer links
the matcher and rejects legacy `plugins.bitrouter-guardrails` configuration. See
[GUARDRAILS_EXTENSION.md](GUARDRAILS_EXTENSION.md) for custom-host assembly,
input-only scope, explicit migration blockers, and process-level verification.

### Native harness resources

`bitrouter-orchestrator::harness` owns direct MCP connections and local skills
discovery. The app supplies daemon-owned transport configuration and global roots;
active ThreadService Turns freeze inventories and execute MCP through existing
durable approval/tool barriers. SDK gateway interop and routed provider generation
retain their existing responsibilities. See [BRO harness resources](BRO_HARNESS_RESOURCES.md).

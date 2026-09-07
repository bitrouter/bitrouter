# Spec: `agents` + `runtimes` — a registry primitive pair for ACP agents

Status: **proposed — not yet implemented** · Author: Claude (with Spikel) ·
Date: 2026-09-06

**Implementation note (2026-09-06) — phase 1 landed.** `registry/agents/`
(7 entries) and `registry/runtimes/local.yaml` ship as source YAML, generated
into `dist/registry/{agents,runtimes}.json` by `dist-helper`, with the drift
gate `registry_agents_match_the_compiled_catalog` proving the data agrees with
`CATALOG` before phase 2 flips consumption. Where this document is ahead of the
code:

- **Ids are today's catalog ids** (`claude-acp`, not `claude-agent-acp`).
  Renaming would break `bitrouter spawn claude-acp` for no gain.
- **`acp.capabilities` is not written yet.** It is asserted by conformance T0,
  which lands in phase 3; an unverified capability list is worse than none.
- **`config_file` templates and the `args` override list are not written yet.**
  They are proven by the phase-2 differential test; phase 1 records only the
  variables each harness is pointed at, which the drift gate checks against
  `launch_overlay`.
- **`isolation`, and the `gateway` block of §6, are not written yet.** With
  `local` the only runtime they would carry no information; they arrive with
  the second runtime kind.
- **Phase 2 has landed.** `crate::config_synthesis` replaced the four
  hand-written synthesis arms (182 lines deleted), and `apps/bitrouter/build.rs`
  now generates the ACP half of the catalog from `dist/registry/agents.json`,
  so adding or re-routing an agent is a PR against `registry/` alone. The four
  `Routing` synthesis variants collapsed into `ConfigFile(&ConfigSynthesis)`,
  and `grok`/`antigravity` moved to a hand-written `INTERACTIVE_ONLY` list
  (§13). Verified two ways: the equivalence differential ran before the arms
  were deleted, and the golden fixtures under
  `apps/bitrouter/tests/fixtures/config_synthesis/` now pin the exact bytes each
  harness receives. A deliberate registry edit was confirmed to change those
  bytes with no Rust change, and to be caught by the golden gate.
- **A code review (2026-09-06) found two ways T2 could certify an agent that
  is not actually routed, both fixed.** It accepted any authenticated
  non-`/models` request as a generation — a startup probe would have done — so
  it now requires a `POST` and the pinned model in the path or body, for every
  routed harness rather than only those whose *env/args* can pin one. And it
  appended the routing overlay's arguments to the ACP invocation, which for a
  config-file harness are the *interactive* facet's (`openclaw acp tui
  --local`); those arguments are now applied only to env/args-routable
  harnesses. Because a config-file harness's ACP facet launches direct
  (SPAWN_SPEC §6), T2 reports `skipped` for those four rather than verifying a
  facet BitRouter does not route.
- **Phase 3 has landed, minus T1.** `bitrouter agents conformance` runs
  `acp_compat_1` — T0 handshake and T2 routability — against an ephemeral
  loopback `StubGateway`, needing no provider credentials. Runtime entries take
  a `conformance:` block, and the validator enforces its provenance plus the
  gate (an active runtime may not serve an agent whose record reports a
  failure). **T1 (session lifecycle) is not implemented**: a record simply
  omits it, so the §10 gate reads handshake + routability rather than handshake
  + lifecycle. The suite is self-tested against bash stub agents that really
  call the gateway, including one that omits the credential — otherwise
  `routability: pass` would only mean "something happened".
- **The conformance gate of §10 is a phase-3 rule.** Until the suite exists no
  entry can carry a record, so `active` cannot require one. The unpinned-spec
  rule is an **advisory** until then, and becomes an error when a record is
  required — a record must name an `agent_version`, which a floating tag
  cannot supply.

BitRouter already knows how to find, launch, and route nine coding harnesses.
That knowledge is a Rust `const` — `CATALOG` in
[`apps/bitrouter/src/harness.rs`](../../apps/bitrouter/src/harness.rs) — so the only
way to add a harness is a patch to the binary. This spec promotes it to
registry data, adds a second primitive for *where an agent executes*, and
defines the ACP-compatibility suite that gates admission, so a third party can
register an agent by opening a PR against `registry/` instead of against
`apps/`.

**Load-bearing constraint — the registry is not a launch gate.** Exactly as
`registry/models/` is the blessed default catalog and *not* a routing gate
(`registry/README.md`), `registry/agents/` is the blessed default set and not a
launch gate. `agents:` in `bitrouter.yaml`
([`config/mod.rs:83`](../../crates/bitrouter-sdk/src/config/mod.rs)) remains the
local escape hatch: you do not edit this catalog to run your own agent. What
the catalog buys you is *routing by default* — a registered agent's LLM traffic
is redirected through the daemon with no user configuration, because the
registry carries a verified routing contract.

## 1. Motivation

Three problems, one cause.

1. **The catalog is compiled.** `CATALOG` has nine entries. Adding a tenth is a
   Rust PR, a review, and a release. There is no path for the author of an
   ACP agent to make it routable by BitRouter.
2. **Routing knowledge is code, not data.** `Routing`
   ([`harness.rs:64`](../../apps/bitrouter/src/harness.rs)) has seven variants,
   four of which (`OpencodeConfig`, `PiConfigDir`, `HermesHome`,
   `OpenclawProfile`) exist only because each harness wants its redirection
   written to a config file in a slightly different shape. They are
   structurally identical and could be one declarative form.
3. **"ACP-compatible" is asserted, never verified.** `bitrouter agents check`
   sends `initialize` and stops
   ([`up.rs:329`](../../crates/bitrouter-sdk/src/acp/up.rs)). Nothing checks that a
   session can be created, that updates stream, that permissions round-trip, or
   — most importantly — that the agent's LLM traffic actually arrives at the
   gateway when the declared routing is applied. Without that last check,
   "routable by default" is a claim about a config block, not about behaviour.

There is also a fourth thing this unlocks. Once *where an agent runs* is a
registry primitive rather than an implicit "on this machine", remote agent
sandboxes (E2B, Daytona) become entries in a catalog rather than a fork in the
code — which is the seam feature #735 (sandbox as an isolation rung) needs
anyway.

## 2. Goals / non-goals

**Goals (v1).**

- `registry/agents/` and `registry/runtimes/` as source YAML, generated into
  `dist/registry/{agents,runtimes}.json` and fetched at runtime by the same
  path that fetches `models.json` / `providers.json`.
- A declarative routing contract that covers every redirection shape in today's
  `Routing` enum except `OwnAuth`, so no new harness needs a Rust patch to be
  routed.
- `bitrouter agents conformance` — a three-tier ACP-compatibility suite whose
  lower two tiers need no provider credentials and can therefore run in CI on a
  contributor's PR.
- A contribution flow with the same shape as a provider contribution:
  `status: staging` with a verification header, flipped to `active` by a
  maintainer once conformance is on record.

**Non-goals (v1).**

- Remote runtimes. The schema reserves them and §12 specifies what they need,
  but v1 ships `local` only. Container and remote runtimes are phase 4.
- Replacing the official ACP registry. `agent_registry.rs` stays the discovery
  tier over `cdn.agentclientprotocol.com`; this catalog answers a different
  question (*is it routable through BitRouter, how, and was that verified*).
- Interactive-only harnesses. `grok` and `antigravity` have no ACP adapter and
  stay compiled (§13).
- Agent-level pricing or metering. Runtimes that bill by wall-clock (E2B,
  Daytona) raise this; it is an open question (§17).

## 3. The primitive pair

| models / providers | agents / runtimes |
|---|---|
| `registry/models/<vendor>.yaml` — canonical ids, limits, modalities, `benchmarks:` | `registry/agents/<vendor>.yaml` — bare harness ids, ACP capabilities, routing contract |
| `registry/providers/<name>.yaml` — `api_base`, auth, protocol, per-model `pricing` | `registry/runtimes/<name>.yaml` — where it executes, auth, isolation, per-harness `transport` + `conformance` |
| a model is routable because an **active provider serves it** | an agent is launchable because an **active runtime can run it** |
| `GET /v1/models` = de-duplicated union of active providers' models | `bitrouter agents list` = union of active runtimes' harnesses |
| a provider may serve models beyond the curated set → advisory | a runtime may run harnesses beyond the curated set → advisory |
| `status` gates routing; only `active` is served | identical |
| `provider_model_id` — the same model, packaged differently per provider | `transport` — the same harness, invoked differently per runtime |

The last row is the one that carries the design. A harness is not re-declared
per runtime any more than a model is re-declared per provider; the runtime file
says how *it* invokes the harness, and that is the only thing that varies.

## 4. Id shape and addressing

**Harness ids are bare** — `claude-acp`, `codex-acp`, `opencode` — today's
catalog ids, unchanged. No vendor prefix. This matches both today's `CATALOG` and the official ACP
registry, whose ids are also bare (`gemini`, `opencode`, `pytool` — see the
fixture in [`agent_registry.rs`](../../apps/bitrouter/src/agent_registry.rs)).
Uniqueness is enforced across the whole catalog by the validator; a curated
registry can do that, and it keeps the second segment stable across runtimes.

**Every addressable id is `<runtime>/<harness>`.**

```
local/claude-acp      e2b/claude-acp      daytona/opencode
```

An addressable id is never *declared*. It exists because
`registry/runtimes/e2b.yaml` lists `claude-acp`, exactly as a routable
model exists because a provider's `models[]` lists it. This is why bare harness
ids matter: with vendor-prefixed ids, `e2b/claude-acp` and
`anthropic/claude-acp` would be indistinguishable in shape, and the
`/` separator would be ambiguous. With bare ids the first segment is always a
runtime.

`local/` is the eliding default: `bitrouter spawn claude-acp` resolves to
`local/claude-acp`. A bare harness id that no active runtime lists is an
error naming the runtimes that *do* list it.

This is the same split the routing table already uses for models: `models.json`
holds the canonical `z-ai/glm-5.1`, while a pin is written `opencode-go:glm-5.1`
([`routing_table.rs`](../../crates/bitrouter-sdk/src/config/routing_table.rs)) —
identity and address are different strings.

## 5. `registry/agents/` — the canonical catalog

One file per harness vendor (filing only; ids carry no vendor). A YAML sequence
of harness entries. Facts here are **runtime-independent**: anything that
changes depending on where the agent runs belongs in §6 instead.

```yaml
# registry/agents/agentclientprotocol.yaml
- id: claude-acp
  name: Claude Agent ACP
  description: Anthropic Claude via the maintained Claude Agent ACP adapter
  project_url: https://github.com/agentclientprotocol/claude-acp
  license: Apache-2.0
  acp:
    protocol_version: 1
    # Declared by the agent in `initialize`. Verified by conformance T0 — a
    # capability claimed here and absent from the handshake fails the tier.
    capabilities: [permissions, terminal, mcp]
  routing:
    kind: env
    base_url_env: ANTHROPIC_BASE_URL
    auth_env: ANTHROPIC_AUTH_TOKEN
    bearer_auth: true
    model_env: ANTHROPIC_MODEL
  # Optional `bitrouter launch` facet — the harness's own native TUI. Local
  # runtime only.
  interactive_binary: claude
  # Substring that maps a user-renamed `agents:` entry back to this catalog
  # entry, so routing follows the invocation, not the YAML key.
  package_marker: claude-acp
```

Fields, and the model-catalog rule they inherit: *include only facts you can
verify; omit what you can't.* An unverified `capabilities` entry is worse than
an absent one, because conformance T0 will assert it.

| field | required | notes |
|---|---|---|
| `id` | yes | bare, lowercase, `[a-z0-9-]`, unique across the catalog |
| `name`, `description`, `project_url` | yes | `project_url` is the source of the recommended invocation |
| `license` | no | SPDX id |
| `acp.protocol_version` | yes | integer ACP major version |
| `acp.capabilities` | no | asserted against the `initialize` response by T0 |
| `routing` | yes | §7; `kind: none` for agents that cannot be redirected |
| `interactive_binary` | no | presence declares a `bitrouter launch` facet |
| `package_marker` | yes | invocation → catalog matching |

## 6. `registry/runtimes/` — where an agent executes

One file per machine class. The runtime descriptor plus the harnesses it can
run, each with its invocation and its conformance record.

```yaml
# registry/runtimes/local.yaml
name: local
display_name: Local process
kind: local              # local | container | remote
isolation: none          # none | container | vm
status: active           # active | staging | suspended | withdrawn
# What `{base_url}` resolves to in an agent's routing contract.
gateway: { kind: loopback }
agents:
  - id: claude-acp
    transport:
      type: stdio
      command: npx
      args: ["-y", "@agentclientprotocol/claude-acp@0.70.0"]
    conformance:
      acp_compat_1:
        handshake: pass
        lifecycle: pass
        routability: pass
        suite_version: 1.0.0
        agent_version: 0.70.0
        measured_by: bitrouter
        as_of: 2026-09-06
  - id: opencode
    transport:
      type: stdio
      command: opencode
      args: ["acp"]
    # No package-runner install path; the user provides the binary.
    requires_binary: opencode
    conformance:
      acp_compat_1:
        handshake: pass
        lifecycle: pass
        routability: pass
        suite_version: 1.0.0
        agent_version: 1.17.15
        measured_by: bitrouter
        as_of: 2026-09-06
```

**Conformance is the `pricing:` of agents.** It lives in the runtime file, not
the agent file, for the same reason pricing lives in the provider file: it is a
property of the pair, not of the artifact. An agent that routes correctly
locally can fail routability in a container on egress alone, and a single
`conformance:` block on the agent would have to lie about one of them.

The `gateway` block is the seam that makes remote runtimes possible (§12):

| `gateway.kind` | resolves `{base_url}` to | used by |
|---|---|---|
| `loopback` | the daemon's own `server.listen` (`127.0.0.1:4356`) | `local` |
| `host_alias` | `http://{alias}:{port}`, e.g. `host.docker.internal` | `container` |
| `cloud` | `https://api.bitrouter.ai/v1` with a scoped key | `remote` |
| `tunnel` | a reverse tunnel the daemon opens per session | `remote` (deferred) |

## 7. The routing contract

`routing.kind` has four values. Together they cover every variant of today's
`Routing` enum except `OwnAuth`, which is out of scope (§13).

**`env`** — set variables on the child. Replaces `Routing::Env`.

```yaml
routing:
  kind: env
  base_url_env: ANTHROPIC_BASE_URL
  auth_env: ANTHROPIC_AUTH_TOKEN
  bearer_auth: true            # false ⇒ provider-native header; routing only
                               # works under `skip_auth: true`, and callers warn
  model_env: ANTHROPIC_MODEL
  extra: {}                    # fixed vars the redirect needs
```

**`codex_args`** — Codex's one-shot `-c` provider overrides. Replaces
`Routing::CodexArgs`, and is named for the harness rather than the mechanism
on purpose: the override list is compiled, so a second agent selecting a
generic `args` kind would silently receive Codex's
`model_providers.bitrouter.*` arguments. A general args kind needs its own
fields before it can honestly exist.

```yaml
routing:
  kind: args
  args:
    - "-c"
    - "model_providers.bitrouter.base_url={base_url}/v1"
    - "-c"
    - "model_providers.bitrouter.wire_api=responses"
  env: { BITROUTER_API_KEY: "{auth}" }
```

**`config_file`** — render a file into a per-launch scratch directory and
point the harness at it. Replaces `OpencodeConfig`, `PiConfigDir`,
`HermesHome`, and `OpenclawProfile`.

The config's fixed structure is a JSON `skeleton` with scalar substitution
only; everything that varies structurally is a **knob with a closed set of
values** (see D4 for why this is not a template language):

```yaml
routing:
  kind: config_file
  dir: pi-agent                 # subdir under the launch state dir; omit for the dir itself
  file: models.json
  skeleton: |
    { "providers": { "bitrouter": {
        "name": "BitRouter", "baseUrl": "{base_url_v1}",
        "api": "openai-completions", "apiKey": "{auth}", "models": [] } } }
  models:                       # where the daemon's catalog lands
    at: /providers/bitrouter/models
    shape: array_of_id          # map_of_empty | array_of_id | array_of_profile
    order: catalog_then_model   # catalog_then_model | model_then_catalog
  default_model:                # omit when the harness takes it another way
    at: /model
    format: provider_prefixed   # bare | provider_prefixed
  mcp:                          # omit when the harness has no MCP mechanism
    at: /mcp
    entry: opencode_typed       # opencode_typed | command_args_or_url
  env: { PI_CODING_AGENT_DIR: "{dir}" }
  args:
    always: []
    with_default_model: ["--provider", "bitrouter", "--model", "{default_model}"]
```

A key the renderer fills must already appear in the `skeleton`, in the position
the harness expects: values are replaced in place, and only new keys are
appended. That is what keeps the rendered bytes stable.

MCP injection for the **env/args** harnesses (claude's `--mcp-config` file,
codex's `-c mcp_servers.*` overrides) is deliberately outside this schema — it
is MCP injection layered on env routing, not config-file routing, and folding
the two would widen the schema for no registry gain.

**`none`** — the agent cannot be redirected; `spawn` runs it direct with a note.

Substitutions available in every template and value: `{base_url}` (resolved by
the runtime's `gateway` block), `{auth}`, `{model}`, `{dir}` (the scratch
directory), `{mcp_servers}` (the gateway MCP server block, JSON).

**Safety rules on `config_file`, enforced twice.** Templates are network-fetched
data that get written to disk, so:

- `path` must be relative, and must normalize to a location inside `{dir}`. No
  `..`, no absolute paths, no symlink escape. Checked by the validator at build
  time *and* by the renderer at launch time — the second check is the one that
  matters, because a runtime-fetched catalog was not validated on this machine.
- Templates are written, never executed. No shell, no interpolation of anything
  but the substitution table above.
- `{dir}` is always under the repo's `.bitrouter/launch/`, per-launch, and is
  never a path the catalog chooses.

## 8. Distribution

Identical to models and providers, deliberately.

`registry/{agents,runtimes}/*.yaml` → `dist-helper registry build` →
`dist/registry/{agents,runtimes}.json` (committed, `{"data": [...]}` envelope,
key-sorted by `serialize_data`) → fetched at runtime from
`DEFAULT_REGISTRY_URL` by
[`bitrouter-providers/src/registry/fetch.rs`](../../crates/bitrouter-providers/src/registry/fetch.rs),
cached under `$XDG_CACHE_HOME/bitrouter/` with the same 24h freshness window and
stale-fallback read, merged in `apply`.

**What is different about these bytes, and what to do about it.**
`providers.json` names an `api_base` — a URL to send HTTPS to, with a key the
user configured. `runtimes.json` names a `command` — a child process spawned
with the user's ambient privileges. A poisoned provider entry misdirects
traffic; a poisoned runtime entry is local code execution.

Once invocations are exact-pinned the two are close to equivalent in practice,
because both end in `npx -y <pinned package>` and the residual risk is the npm
supply chain either way. The delta that remains is *reach*: with a runtime
fetch, a bad merge or a compromised CDN reaches users without a release. Three
rules close it to the level of the compiled catalog:

1. **Exact pins only.** No `@latest`, no floating tags, in any registry-listed
   invocation. Validator rule (§10). This has teeth today — of the nine catalog
   entries, `gemini-cli` (`@google/gemini-cli@latest`) and `pi-acp`
   (`pi-acp@latest`) are unpinned and could not go `active` unchanged.
2. **`requires_binary` instead of silent `$PATH` resolution.** `opencode`,
   `hermes` and `openclaw` invoke a bare binary. That is legitimate — the user
   installed it — but it must be a declared expectation, not an invocation that
   quietly resolves to whatever is on `$PATH`.
3. **`dist/registry/*.json` is a reviewed release artifact.** Committed,
   diffable in the PR that changes the source, and covered by
   `dist-helper check`. This already holds; it is restated because it is now
   load-bearing for more than pricing accuracy.

## 9. Conformance — `acp_compat_1`

Three tiers. A runtime's per-agent `conformance` block records one result per
tier; an unrun tier is **absent**, never `pass`.

**T0 — handshake.** Spawn, `initialize`, assert the protocol version matches
`acp.protocol_version` and that every capability the agent entry claims is
present in the response. Hermetic, no credentials, seconds. This is today's
`health_check` plus assertions.

**T1 — lifecycle.** `session/new` → `session/prompt` → typed `session/update`
stream → permission request round-trip (approve and deny) → `session/cancel` →
correct `stop_reason` on both the completed and cancelled paths. Asserts the
translated update variants survive round-trip, which
[`tests/acp.rs`](../../apps/bitrouter/tests/acp.rs) already does for the stub agent.

**T2 — routability.** Apply the agent's declared routing contract with
`{base_url}` pointing at an **ephemeral in-process gateway** that answers with a
canned completion and records what it received. Assert the request arrived, and
that it carried `Authorization: Bearer <minted>`, the pinned model, and
`x-bitrouter-harness`.

**T1 and T2 require no provider credentials**, because the stub gateway answers
in place of a model. That is what makes the suite runnable in CI on a
contributor's PR, and it is the tier that turns "routable by default" from a
config assertion into a verified behaviour. The rig is the one
[`tests/full_stack.rs`](../../apps/bitrouter/tests/full_stack.rs) and
[`observe_hierarchy.rs`](../../apps/bitrouter/tests/observe_hierarchy.rs) already
use (a real assembled app in front of a `wiremock` upstream), with the one
change that the conformance command needs a real bound listener rather than an
`axum_test::TestServer`, because the agent is a child process dialling over TCP.

Provenance fields mirror `benchmarks:` in the model catalog, and for the same
reason — a bare `pass` is not reproducible:

| field | meaning |
|---|---|
| `suite_version` | semver of `acp_compat_1`; a bump invalidates older records |
| `agent_version` | the version actually exercised |
| `measured_by` | `bitrouter`, or a third-party source name |
| `as_of` | `YYYY-MM-DD` snapshot date |
| `source_url` | required when `measured_by` is a third party |

## 10. Validator rules

Added to `validate_loaded`
([`helpers/dist-helper/src/registry.rs:1038`](../../helpers/dist-helper/src/registry.rs)).
Errors unless marked advisory.

**Agents.**

- `id` is bare, lowercase `[a-z0-9-]+`, and unique across the whole catalog.
- `acp.protocol_version` is present and a positive integer.
- `routing.kind` is one of the four forms, with the fields that form requires.
- `config_file` templates: every `path` is relative and normalizes inside
  `{dir}`; every substitution used is in the substitution table.
- `project_url` is HTTPS.

**Runtimes.**

- `name` equals the filename stem, and is the env-var root (`{NAME}_API_KEY`)
  when `kind: remote`.
- Every listed `agents[].id` that is in the curated catalog resolves; ids
  **not** in the catalog are an **advisory**, exactly as non-curated provider
  models are.
- Every listed agent has a `transport` (the analogue of "a `usage_token`
  provider prices every model it lists").
- Every `stdio` invocation is exact-pinned: a package-runner spec carries an
  explicit version, or the entry declares `requires_binary`. No `@latest`.
- `status: active` requires an `acp_compat_1` record with `handshake` and
  `lifecycle` at `pass`; `routability` may be absent only when the agent's
  `routing.kind` is `none`.
- `kind: remote` requires a `gateway` block that is not `loopback`.

Per `CLAUDE.md`, changes confined to `registry/` are validated with
`dist-helper` — not with Rust tests that freeze catalog entries or counts.

## 11. CLI surface

```
bitrouter agents list [--runtime <name>] [--remote]
bitrouter agents check
bitrouter agents install <runtime>/<harness>
bitrouter agents conformance <runtime>/<harness> [--suite acp_compat_1]
                                                 [--tier t0|t1|t2] [--report <path>]
bitrouter spawn <runtime>/<harness> -p "…"
bitrouter launch --agent <harness>
```

`list` gains a runtime column and lists the union of active runtimes'
harnesses. `check` is unchanged — it probes *configured* agents. `conformance`
is new: it runs the suite locally and writes the JSON record a contributor
pastes into their runtime entry. `spawn` accepts the addressable id with
`local/` elided; `launch` is local-only and takes a bare harness id, since the
interactive facet is a native TUI on this machine by definition.

`skills/bitrouter/` must be updated in the same change (`CLAUDE.md` rule 1) —
new subcommand, new id form.

## 12. Remote runtimes (phase 4, specified now)

Docker is nearly free: `docker run --rm -i <image@sha256:…>` is an
`AcpTransport::Stdio` invocation, and `gateway: { kind: host_alias, alias:
host.docker.internal }` handles reachability. No new transport.

E2B and Daytona break both directions, and each break is real work.

**Transport.** The harness runs on someone else's VM, so ACP stdio must be
tunnelled through a local broker that holds the sandbox session and proxies
stdin/stdout. That is the second `AcpTransport` variant that
[`transport.rs`](../../crates/bitrouter-sdk/src/acp/transport.rs) anticipates when
it says v1.0 ships stdio only. It is **per-vendor** — E2B and Daytona have
different session APIs — so remote runtimes need a compiled adapter the way
providers need a protocol adapter. The YAML carries only `api_base`, the auth
env root, and the template/image id.

**Gateway reachability.** A daemon on `127.0.0.1:4356` is not reachable from a
remote VM. `gateway.kind: cloud` points the sandboxed agent at
`https://api.bitrouter.ai/v1` with a session-scoped key — no new
infrastructure, since the `bitrouter` cloud provider already exists in the
registry — at the cost of making remote runtimes require cloud.
`gateway.kind: tunnel` keeps everything local at the cost of maintaining
tunnelling. This is open question OQ1; it decides what `{base_url}` means for
every remote runtime, so it should be settled before the runtime schema is
frozen.

## 13. What this does to `harness.rs`

Scope is ACP agents only, and the split falls out cleanly: of the nine catalog
entries, exactly the two with no ACP adapter — `grok` and `antigravity` — are
also the two with `Routing::OwnAuth`, i.e. no routing knowledge to move. They
stay as a small compiled interactive-only list. The other seven become registry
data, and the six of those with an `interactive_binary` keep their `launch`
facet by consuming the *same* declarative routing block that `spawn` uses.

SPAWN_SPEC's one-store invariant therefore survives — it becomes one *data*
store rather than one `const`. What stays compiled:

- `HarnessEndpointPlan` / `endpoint_plan`
  ([`harness.rs:459`](../../apps/bitrouter/src/harness.rs)) — the maintained-adapter
  provider-configuration path, today only `claude-acp` and `codex-acp`, pinned
  against the fixtures in
  [`tests/fixtures/acp_adapters/`](../../apps/bitrouter/tests/fixtures/acp_adapters).
  Registry entries declare env/args/config-file routing; the pinned provider
  plan stays a reviewed in-repo contract.
- `grok` / `antigravity`, as above.
- Remote-runtime transport adapters (§12).

## 14. Config schema change

`Config.agents` is unchanged in shape and meaning. `AcpTransport` gains its
second variant only in phase 4. The new block is:

```yaml
runtimes:
  local: {}                      # implicitly active; listed for explicitness
  e2b:
    api_key: "${E2B_API_KEY}"
```

A runtime absent from config is inactive unless it is `local`, which is
implicitly active — the same treatment the compiled-in `bitrouter` cloud
gateway provider gets in the registry merge. Regenerate
`dist/schema/bitrouter.config.schema.json` (`dist-helper generate-schema`).

## 15. Testing

Registry data needs no Rust tests (`CLAUDE.md`). Code does:

- **Declarative routing renderer** — each `kind` produces the overlay the
  current `Routing` variant produces. Written as a differential test during
  phase 2: for all seven migrating harnesses, the rendered overlay equals
  `routing_overlay` / `launch_overlay` byte-for-byte before the old code is
  deleted.
- **Path containment** — `routing.dir` and `routing.file` must be relative and
  free of `..`. With the knob model the renderer joins them onto the scratch
  path directly and builds no paths by concatenation, so this is a validator
  rule rather than a second runtime check.
- **Skeleton position invariant** — a key the renderer fills (`models.at`) must
  already exist in the skeleton, or the collection would be appended instead of
  landing where the harness expects it. Validator rule; it is the failure the
  golden fixtures would otherwise catch only after the fact.
- **Id parsing** — `<runtime>/<harness>`, `local/` eliding, ambiguity errors
  naming the runtimes that list a bare id.
- **Drift gate (phase 1)** — the generated catalog equals `CATALOG`, so the data
  lands correct before anything consumes it. *Retired in phase 2*: once
  `build.rs` generates the catalog from that same artifact the comparison is
  tautological, so it was replaced by
  `registry_membership_follows_having_an_acp_adapter`, which pins the split
  invariant (§13) and that the codegen produced a non-empty catalog.
- **Conformance suite** — self-test against the existing bash stub agent
  ([`tests/acp.rs`](../../apps/bitrouter/tests/acp.rs)), including a stub that
  deliberately fails each tier, so a green suite means something.

## 16. Phasing

| phase | content | risk |
|---|---|---|
| 1 | **Done.** `registry/{agents,runtimes}/` source, validator rules, `{agents,runtimes}.json` build, drift gate asserting catalog ≡ registry. No behaviour change. | data-only |
| 2 | **Done.** Generated catalog is the source; declarative `config_file` replaced the four synthesis variants (differential test, then delete); `grok`/`antigravity` split out. | medium — routing regressions |
| 3 | **Done (T0 + T2).** `bitrouter agents conformance` + ephemeral stub gateway + CI job + contributor docs in `registry/README.md`. **Registration opens here.** T1 lifecycle deferred. | new surface |
| 4 | `container` runtime, then `remote` (new `AcpTransport` variant, OQ1 resolved). Ties into #735. | largest |

## 17. Open questions

- **OQ1 — remote gateway reachability.** `cloud` (ships now, requires cloud) or
  `tunnel` (local-first, more to maintain)? Decides `{base_url}` for every
  remote runtime. §12.
- **OQ2 — runtime cost.** E2B and Daytona bill by wall-clock. Providers price
  tokens per model; do runtimes price seconds per harness, and does that feed
  the cost attribution the ACP controller already decorates `usage_update`
  with?
- **OQ3 — conformance staleness.** A record pins `agent_version`. When a
  runtime's pin bumps, the record is stale by construction. Providers solve the
  equivalent with `auto_sync`; agents have no feed. Does a version bump
  auto-demote `status` to `staging`, or is a stale record merely surfaced as an
  advisory?
- **OQ4 — third-party conformance trust.** `measured_by: <third party>` with a
  `source_url` is enough for a benchmark number. Is it enough to grant routing
  by default, or does `active` always require a `bitrouter`-measured record?

## 18. Decisions log

- **D1 — agents and runtimes are a primitive pair mirroring models and
  providers.** Not one primitive with an environment field: the fan-out
  (one harness, many machines) is exactly the fan-out providers already model,
  and reusing that shape reuses its validator, its dist pipeline, its fetch and
  cache layer, and its contribution norms.
- **D2 — harness ids are bare; every addressable id is `<runtime>/<harness>`.**
  Vendor-prefixed harness ids would make `e2b/x` and `anthropic/x`
  indistinguishable in shape. Bare ids match today's `CATALOG` and the official
  ACP registry, and make the first segment unambiguously a runtime.
- **D3 — conformance lives in the runtime file, not the agent file.** It is a
  property of the (agent, runtime) pair, like pricing. An earlier draft put it
  in the agent file by analogy with `benchmarks:`; that analogy is wrong,
  because an agent can pass routability locally and fail it in a container.
- **D4 — routing is declarative, including config-file synthesis, expressed as
  a closed set of shape knobs.** *Revised 2026-09-06.* The original claim — that
  the four synthesis variants "are one operation with different filenames" — is
  **false**, and building phase 2 is what showed it. They share writing a file;
  they do not share building its content. opencode wants its models as a map
  keyed by id, pi as an array of `{id}`, openclaw as an array of fully-specified
  model objects (its validation rejects less), and hermes carries no model list
  at all; each writes its default model to a different path in a different
  format, and two append CLI arguments. That needs iteration and conditionals —
  a template language — and evaluating a network-fetched template that writes
  files is a far larger surface than substituting scalars into one.
  So the variation is modelled as **enums with fixed value sets**
  (`crate::config_synthesis`): nothing is evaluated, and a registry entry can
  only select among behaviours reviewed in this repo. A new config-file harness
  matching an existing shape still needs no Rust; only a genuinely new shape
  does. Leaving them compiled was the alternative, and it would have limited
  self-serve registration to the env/args third of the catalog.
- **D5 — distribution is identical to models and providers.** The
  code-execution delta is real but is closed by exact pins plus the fact that
  `dist/registry/` is a reviewed, committed artifact. Diverging the transport
  would have bought a security property the exact-pin rule buys more cheaply.
- **D6 — scope is ACP agents only.** `grok` and `antigravity` stay compiled.
  They are precisely the two entries with `Routing::OwnAuth`, so the split
  duplicates nothing.

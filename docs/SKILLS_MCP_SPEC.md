# Skills over MCP — server and gateway spec

Adopting [SEP-2640](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640)
(`io.modelcontextprotocol/skills`) so BitRouter serves skills it holds and
proxies skills held by upstream MCP servers — over stdio and Streamable HTTP
alike.

Status: **complete — Phases 0–4 implemented, Phase 5 decided against** ·
Author: Claude (with Spikel) · Date: 2026-08-03

All open decisions are resolved (§11). Phase 5 is closed as a set of decisions,
not deferred work: registry fronting (D8), per-caller credentials (D7), and
`resources/directory/read` are each decided against with reasons in §12, and
the planned `gateways.rs` retirement is withdrawn because its rationale was
factually wrong.

**Position statement.** BitRouter is a skills **server** and **gateway**. It is
not, and does not become, a skills **host**. §2 defines the line and shows we
are already on the right side of it.

**Implementation note (2026-08-03).** Phases 0–4 landed; see
[the plan](2026-08-03-skills-over-mcp-plan.md) for the task-level record. Four
things were decided differently once the code was in front of us, each because
the spec had assumed something that turned out not to hold:

1. ~~**Wire types live in `bitrouter-mcp`, not `bitrouter-sdk`.**~~
   **Reversed 2026-08-03 — the spec was right and this deviation was wrong.**
   Both justifications failed on inspection: `bitrouter-mcp` has no stated
   objection to depending on `bitrouter-sdk` (the recorded dependency-inversion
   rule names `bitrouter-substrate`, `bitrouter-skills`, and
   `bitrouter-observe`, not the SDK) and the edge is not a cycle; and
   "raw JSON preserves unknown fields" was true only of the type *as written* —
   a `#[serde(flatten)]` catchall gives verbatim passthrough **and** typed
   access. The split also produced a duplicated `SKILL_SCHEME` constant in two
   crates, which is the exact drift the single-source rule exists to prevent.
   The line now falls in three tiers, per §5.
2. **The URI invariant does not hold by construction, and it is worse than
   §7 said.** `install_as` lets the installed directory name differ from the
   frontmatter name — reached via `bitrouter skills update` (which reinstalls
   into the existing directory when an upstream name drifts), not a CLI flag as
   an earlier draft of the plan claimed. URIs are derived from the frontmatter
   name; `skill://<dir>/<name>/SKILL.md` when the two differ.
3. **No digest mtime cache.** Planned, then dropped as unjustified complexity:
   `skills/list` is already cached at the MCP layer, and hashing a local skill
   tree is milliseconds. Adding a `Mutex` + invalidation for it would have been
   over-design.
4. **`directoryRead` is not implemented anywhere, and the origin server
   declares it `false`** (§7 planned `true`). A skill entry's `resources`
   already enumerates every file, so a host holding the entry can filter by
   prefix; the optional method has nothing to add for a consumer that does not
   yet exist.

Phase 5 was subsequently closed by decision rather than implemented — see the
status note above and §12.

## Contents

- [1. Motivation](#1-motivation)
- [2. Position: server and gateway, not host](#2-position-server-and-gateway-not-host)
- [3. Verified starting state](#3-verified-starting-state)
- [4. The three blockers](#4-the-three-blockers)
- [5. Crate boundaries](#5-crate-boundaries)
- [6. Wire types](#6-wire-types)
- [7. Origin server — local skills](#7-origin-server--local-skills)
- [8. Gateway — remote skills](#8-gateway--remote-skills)
- [9. Remote transport and tenancy](#9-remote-transport-and-tenancy)
- [10. Phases](#10-phases)
- [11. Open decisions](#11-open-decisions)
- [12. Non-goals and deferred](#12-non-goals-and-deferred)
- [13. Risk register](#13-risk-register)
- [14. Acceptance](#14-acceptance)

## 1. Motivation

BitRouter has a skills surface today, but it is shaped as a **package manager**:
`bitrouter skills add` resolves a source, `git clone --depth 1`s it, and copies
the tree into `.claude/skills/<name>` so a harness discovers it on disk. The
only MCP-facing part is a pair of custom tools (`skills_search` / `skills_get`)
served by an origin server that is injected into TUI-launched harnesses.

Three forces make that shape wrong going forward:

1. **The stdio injection is not a general surface.** `bitrouter_skills` reaches
   a harness only when BitRouter launches it — via the TUI orchestrator or the
   fleet backend's ACP subagents. Nothing reaches the daemon's skills over HTTP,
   so no ordinary MCP client can use them.

   *(Corrected 2026-08-03: this originally read "injected only into harnesses
   launched by the TUI orchestrator, which [#749] removes", and concluded the
   injection should be retired. Both halves were wrong — the fleet backend is a
   second, surviving consumer. §12 and the plan's Task 9 carry the correction.)*
2. **The ecosystem is converging on a wire format.** SEP-2640 specifies how
   skills ride on MCP Resources. Several implementations independently landed
   on `skill://` and diverged on structure; the SEP is the reconciliation.
   Inventing a third BitRouter-specific shape has no upside.
3. **"Remote skill" should stop meaning "clone to disk."** Today a remote skill
   requires a fetch, a disk write, and a host filesystem rescan. Under SEP-2640
   it is a `resources/read` against an MCP session. That removes the install
   step entirely — and it removes BitRouter from the host's filesystem.

The SEP's own rationale names our shape as a motivating server type:

> a skill gateway fronting an external index (unbounded)

We already have that index: `marketplace.json`, served from
`/v1/namespaces/{ns}/skills/hub` ([`marketplace.rs:96`](../crates/bitrouter-skills/src/marketplace.rs#L96)).

## 2. Position: server and gateway, not host

SEP-2640 places a large, ongoing compliance burden on **hosts**: origin tagging
at the point content enters model context, per-origin name namespacing under
host-assigned labels, content-bound approval with revocation on digest-set
change, suppression of `allowed-tools` from MCP origins, gating of code
execution while acting on an MCP skill, and cache isolation from filesystem
skill discovery paths.

A **server** and a **gateway** inherit almost none of it.

**The line is not "touches disk." It is "decides what enters a model's
context."** Measured against that line, BitRouter is already compliant:

| Concern | Where it would live | Actual state |
|---|---|---|
| Installs skills onto disk from the daemon | `install::{install,remove}` | Called **only** from [`commands.rs:1029`](../apps/bitrouter/src/commands.rs#L1029), the user-invoked CLI. No daemon path calls it. |
| Puts SKILL.md text into a model's context | `skill_body` | Exists **only** in [`skills_query.rs:85`](../apps/bitrouter/src/skills_query.rs#L85), reached when a model calls the `skills_get` tool. Model-driven, not host-injected. |
| Loads skills into a harness | [`gateways.rs:47`](../apps/bitrouter/src/gateways.rs#L47) | Injects an **MCP server** into the harness. The harness (Claude Code, Codex) is the host; BitRouter is its server. |

`bitrouter skills add` is a package manager, not a host: it is a peer of `npx
skills add`, it runs under explicit user command, it puts nothing in a context
window, and it makes no approval or execution decision. The host is whatever
later reads `~/.claude/skills/`.

**Two rules keep it that way, and they are the normative content of this
section:**

- **R1.** No daemon code path may call `bitrouter_skills::install::*`. Skill
  installation is a CLI-only operation, invoked by a human.
- **R2.** Gateway-sourced skill content must never be written to a filesystem
  skill discovery path. SEP-2640 requires that a host caching MCP-served
  content place it somewhere *excluded from every filesystem-skill discovery
  path*; `install.rs` writes into exactly that path by design. The §8 content
  cache therefore gets its own content-addressed store and shares no code with
  `install.rs`.

## 3. Verified starting state

Measured in this worktree, not inferred.

| Fact | Evidence |
|---|---|
| Gateway `initialize` advertises tools only — no `resources`, no `extensions` | [`server.rs:334`](../crates/bitrouter-sdk/src/server.rs#L334) |
| `RmcpExecutor` has a closed method table; unknown methods are `-32601` | [`rmcp_executor.rs:825`](../crates/bitrouter-sdk/src/mcp/rmcp_executor.rs#L825) |
| Aggregate rejects unknown methods, including on the empty-member path | [`aggregating_executor.rs:232`](../crates/bitrouter-sdk/src/mcp/aggregating_executor.rs#L232), [`:253`](../crates/bitrouter-sdk/src/mcp/aggregating_executor.rs#L253) |
| Aggregate `resources/read` is first-success-wins across members | [`aggregating_executor.rs:185`](../crates/bitrouter-sdk/src/mcp/aggregating_executor.rs#L185) |
| Response cache key carries no caller identity | [`caching_executor.rs:102`](../crates/bitrouter-sdk/src/mcp/caching_executor.rs#L102) |
| `cacheScope: private` is correctly declined rather than partitioned | [`caching_executor.rs:305`](../crates/bitrouter-sdk/src/mcp/caching_executor.rs#L305) |
| Streamable HTTP exists in both directions | outbound [`transport.rs:29`](../crates/bitrouter-sdk/src/mcp/transport.rs#L29); inbound `POST /mcp`, `POST /mcp/{name}` ([`app.rs:208`](../crates/bitrouter-sdk/src/app.rs#L208)) |
| `rmcp` 3.1.0 carries a catch-all `CustomRequest` in `ClientRequest` | `Cargo.lock:4314`; `rmcp-3.1.0/src/model.rs:948`, `:4461` |
| Nothing outside the CLI depends on `install::*` | `rg` over `apps/`, `crates/` |

Two consequences follow directly. First, **no transport work is required** —
both HTTP directions already exist. Second, **method passthrough needs no rmcp
upgrade** — `CustomRequest`/`CustomResult` landed in the stable 3.1.0 line we
already depend on.

## 4. The three blockers

These are the whole of what stands between today and skills flowing over HTTP.

**B1 — the gateway does not advertise `resources`.** Skills are built entirely
on `resources/read`. A spec-compliant client reading
`"capabilities": {"tools": {}}` will never issue one. The existing comment
explains the original reasoning ("so clients don't probe upstreams that may not
implement them"), which no longer holds once skills are in scope.

**B2 — `skills/*` cannot traverse the gateway.** Both executors close their
method tables. `RmcpExecutor`'s comment states the intent plainly: *"The spec
catalogue is closed for v0 of the protocol."* Extension methods are exactly the
case that invalidates the assumption.

**B3 — aggregate `resources/read` is a cross-origin confused deputy.**
`try_each` returns the first member that answers. Two upstreams both serving
`skill://refunds/SKILL.md` means whichever is configured first silently wins.
SEP-2640 names this vector:

> a malicious server can publish a skill under the name of a popular one …
> Hosts MUST resolve skill names within a per-origin namespace

This is a latent bug today for any colliding resource URI; skills make it
reachable and security-relevant.

**Note on why the tool-prefix trick does not transfer.** For tools the gateway
prepends `{server}__` to the name. That is unavailable here: SEP-2640 requires
the final `<skill-path>` segment to equal `frontmatter.name`, and requires the
entry `frontmatter` to match the fetched `SKILL.md` field-by-field. Prefixing
the name breaks both invariants. Namespacing must happen in the **URI prefix**
(§8).

## 5. Crate boundaries

Three tiers. The protocol tier merges into the SDK; the port tier stays with
its siblings; the package-manager tier does not move at all.

| Tier | Concern | Home | Rationale |
|---|---|---|---|
| **1 — protocol** | `SKILL_SCHEME`, the `skills/*` method names, `SkillEntry`, `SkillResource`, `ListSkillsResult`, `GetSkillParams/Result`, URI namespacing | `bitrouter-sdk::mcp::skills` | Both halves of the gateway speak these, and so must anything that reasons about skills in flight. Pure `serde_json` — ungated, pulls no rmcp, so a consumer gets it at `default-features = false`. |
| **2 — port** | `SkillCatalog`, `SkillFile`, `SkillFileBody` | `bitrouter-mcp::capabilities::skill_catalog` | A port is "what the app implements for the origin server", not protocol. Its five siblings — `Fleet`, `CostQuery`, `HumanBridge`, `SkillsQuery`, `RoutingQuery` — all live here; moving one to the SDK would be the inconsistency. |
| **3 — package manager** | frontmatter parsing, source resolution, git fetch, marketplace types, install | `bitrouter-skills` (unchanged) | Already tested and hardened (argument-injection guards in `clone_into`, traversal checks in `subdir_is_safe`). Nothing about "skills is a context primitive" argues for putting a package manager inside a routing SDK — the primitive is the wire shape, not the fetch mechanism. |
| — | Filesystem `SkillCatalog` impl | `apps/bitrouter` | Same seam as `skills_query.rs` today. |

**Why tier 1 belongs in the SDK, stated once so it is not re-litigated.** The
`mcp::PreRequestHook` / `RouteHook` / `ExecutionHook` traits see raw JSON
(`McpRequest::params`, `McpResponse::result`). Any hook that wants to apply
policy to a skill in flight — refuse one whose frontmatter declares
`allowed-tools`, or record which skills entered which agent's context as signal
for the adequacy ledger — would otherwise re-derive these shapes downstream.
Keeping them beside the hook traits is what prevents that. The first draft split
them out and immediately produced two `SKILL_SCHEME` constants; that is the
failure mode in miniature.

`SkillEntry` carries a `#[serde(flatten)]` catchall so an unmodelled top-level
field survives a round trip. Without it, routing an upstream entry through the
type would silently drop whatever a future SEP revision adds — for a gateway,
quietly degrading what an upstream published.

**`bitrouter-skills` is not deprecated.** Three of its five modules are
load-bearing for the server we are building.

**Corrected 2026-08-03 — it is one of five, not three.** That claim was written
before D8 decided against registry fronting, which was the only thing that
would have made `source.rs` and `marketplace.rs` server-side. As built:

| Module | Consumed by | Server-load-bearing? |
|---|---|---|
| `frontmatter.rs` | `skills_catalog.rs` (SEP server) + `skills_query.rs` (tools) | **Yes** — `skills/list` MUST carry verbatim frontmatter, so something must parse `SKILL.md` |
| `source.rs` | `commands.rs` only | No — CLI. Would have been server-side under registry fronting; D8 declined it |
| `marketplace.rs` | `commands.rs` only | No — CLI. Same reason |
| `install.rs` | `commands.rs` only | No — the host-adjacent module; CLI-only under R1/R2 |
| `lib.rs` (errors, home dir) | all of the above | Shared |

So the crate is now **one shared parser plus a CLI package manager**, which is
a narrower structural story than the original argument claimed. It is still not
deprecated, for three reasons that survive the correction:

1. Deleting it means reimplementing SKILL.md frontmatter parsing for the SEP
   server, which is the one thing the server genuinely cannot do without.
2. `bitrouter skills add` is a shipped surface, frozen but supported (D5), and
   the documented install path for `skills/bitrouter` in CLAUDE.md and both
   plugin manifests.
3. It is the only working way to get a skill onto disk while no shipping host
   consumes MCP-served skills.

If (3) ever stops being true — a host reads `skills/list` end-to-end — then
reasons 2 and 3 weaken together and the crate is worth revisiting. Reason 1
stands regardless.

The SDK depends on `bitrouter-skills` one-way if it ever needs frontmatter
parsing host-side; it does not absorb it. Folding `git clone` and
install-to-disk into a crate consumers embed for *routing* is the wrong trade
against a feature graph (`server`, `config_file`, `mcp`, `acp`) that is sliced
precisely to avoid it.

**Re-centering, not retiring.** The crate is organized as a package manager
with a format library inside. Invert the emphasis: the format library is the
part with two consumers and a future; install-to-disk is a CLI-only leaf.

## 6. Wire types

New module `crates/bitrouter-sdk/src/mcp/skills.rs`. Serde types only — no I/O.

```rust
/// One `{uri, digest}` pair from a skill entry's `resources` array.
pub struct SkillResource { pub uri: String, pub digest: String }

/// A skill entry — identical in `skills/list` and `skills/get`.
pub struct SkillEntry {
    pub uri: String,
    /// Verbatim SKILL.md frontmatter as JSON. Not a curated subset.
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    /// Complete file enumeration. `None` only for dynamically generated skills.
    pub resources: Option<Vec<SkillResource>>,
    /// Unmodelled *top-level* entry fields, preserved verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub struct ListSkillsResult { pub skills: Vec<SkillEntry> }
pub struct GetSkillParams { pub uri: String }
pub struct GetSkillResult { pub skill: SkillEntry }
```

As-built, `ListSkillsResult` carries no `next_cursor` and no SEP-2549
`ttl_ms` / `cache_scope`: the origin server returns one page, and cache hints
are read off the raw upstream result by `extract_cache_hint` in the caching
layer, which is the only place that consumes them. Fields nothing produces
would be dead surface.

Four constraints the types must not lose:

- `frontmatter` is **verbatim**, not a struct with `name`/`description` fields.
  A curated subset forces a spec amendment every time Agent Skills grows a
  field. Keep it a map.
- `resources: Option<…>` — `None` is meaningful (dynamically generated skill),
  distinct from an empty vector (a skill with no files, which is invalid).
- `extra` is flattened, not dropped. A gateway that silently discards fields it
  does not model degrades what an upstream published; the SEP is a draft over a
  spec that evolves separately, so unmodelled fields are expected.
- The method names and `skill://` are **constants here and nowhere else**
  (`SKILLS_LIST_METHOD`, `SKILLS_GET_METHOD`, `SKILL_SCHEME`, …). The relay
  allowlist, the caching layer's method table, the aggregate dispatch arms, and
  the origin server's handler all reference them rather than re-spelling the
  strings.

**Port** (tier 2, in `bitrouter-mcp`):

```rust
#[async_trait]
pub trait SkillCatalog: Send + Sync {
    async fn list(&self) -> Result<ListSkillsResult, ToolError>;
    async fn get(&self, uri: &str) -> Result<GetSkillResult, ToolError>;
    async fn read(&self, uri: &str) -> Result<SkillFile, ToolError>;
}
```

Per CLAUDE.md #2, `mcp/mod.rs` gains `pub mod skills;` and no re-exports;
consumers reach `mcp::skills::SkillEntry` directly.

## 7. Origin server — local skills

`bitrouter-mcp` gains a `SkillCatalog` capability alongside the existing
`SkillsQuery`. `apps/bitrouter` implements it over the installed-skills root
using `bitrouter_skills::frontmatter::discover_all_skills`, on the blocking
pool, exactly as `InstalledSkills` does today.

- **URIs.** `skill://<name>/SKILL.md` for a skill installed as
  `.claude/skills/<name>`. The invariant (final segment == `frontmatter.name`)
  holds by construction, because `install.rs` already validates the directory
  name against the frontmatter name.
- **Digests.** SHA-256 over raw bytes of every file in the skill directory,
  `sha256:{64 lowercase hex}`. Cached by `(path, mtime, len)`; a miss re-hashes.
  Cheap for a local tree.
- **Capability declaration.** The origin server knows its own catalog at
  startup, so it declares honestly — including `directoryRead: true`, which is
  trivial over a filesystem walk. This is a real asymmetry with the gateway
  (§8, D1).
- **`resources/read`.** Serves any file under a skill directory, rejecting
  traversal outside it.

**`skills_search` / `skills_get` stay.** They are not superseded. The SEP
methods require the *host* to implement registry merging, progressive
disclosure, and content-bound approval; the tool form works with every client
today because it is a tool call. As of the PR discussion, Claude Code
> loads skills from the filesystem/plugins only and treats MCP resources as
> user-mention attachments rather than model-driven reads

so the SEP surface currently has no shipping consumer. Removing the working
surface to add a spec-correct one with no consumers is a net loss. See D4.

## 8. Gateway — remote skills

The gateway merges N upstream skill namespaces into one. This is the hard part
of the design, and it is entirely about URI rewriting.

### 8.1 Namespacing

Each member gets its BitRouter-configured `server_name` as its **host-assigned
label** — which is what SEP-2640 requires (*"identifying servers by a
host-assigned label, not the self-reported `serverInfo.name`"*). Rewrite:

```
upstream   skill://refunds/SKILL.md          (member "acme")
gateway    skill://acme/refunds/SKILL.md
```

This is legal under the SEP without qualification: preceding segments are *"a
server-chosen organizational prefix"*. The invariants survive:

- final segment is still `refunds` == `frontmatter.name` ✓
- `frontmatter` is passed through untouched ✓
- digests are over content bytes, not URIs, so rewriting does not invalidate
  them ✓

Every `resources[].uri` in the entry is rewritten with the same prefix.

### 8.2 Routing back

Inbound `resources/read` on a `skill://<label>/…` URI strips the label,
resolves `<label>` to a member, and dispatches to that member alone. **This
replaces `try_each` for skill URIs** and closes B3: a skill served by member A
can never cause a read against member B.

The mapping is a pure function of `(label, uri)` in both directions, so the
gateway stays stateless per request — no session table, no cached mapping.
That statelessness is why §8.4 restricts v1 to `skill://`.

### 8.3 Method behaviour

| Inbound | Aggregate behaviour |
|---|---|
| `skills/list` | fan out, rewrite every `uri` and `resources[].uri`, concat; partial failures under `_bitrouterErrors` per existing convention |
| `skills/get` | strip label from `params.uri`, dispatch to owning member, rewrite the returned entry |
| `resources/read` on `skill://` | strip label, dispatch to owning member (no `try_each`) |
| `resources/directory/read` | deferred — see D1 |

Pagination across a fan-out is genuinely awkward (N upstream cursors, one
outbound cursor). v1 aggregates every page eagerly, reusing the
`list_all_preserving!` pattern already in `rmcp_executor.rs`, and returns no
`nextCursor`. Acceptable while catalogs are small; revisit if a member's
catalog is large enough to matter.

### 8.4 Scheme restriction (v1)

Only `skill://` skills are aggregated. A member serving skills under another
scheme (SEP-2640 permits e.g. `github://owner/repo/…`) is reachable through its
**direct route** `POST /mcp/{name}`, unrewritten, where no collision is
possible because there is one member.

Rationale: reversing a rewrite of an arbitrary scheme requires remembering the
original scheme per skill, which is state, which breaks §8.2's statelessness.
Encoding the scheme into the rewritten path is possible but ugly. Deferring is
honest and cheap. See D2.

### 8.5 What the gateway does not do

The gateway **passes digests through**; it does not re-compute or re-sign them.
SEP-2640 is explicit that this is not a security boundary:

> Any intermediary on the path, such as a gateway, can rewrite both the listing
> and the content together. Hosts MUST NOT treat a digest match as a security
> boundary.

BitRouter *is* that intermediary. Documentation must say so plainly rather than
implying the gateway adds integrity it does not add.

## 9. Remote transport and tenancy

No transport work is needed (§3). Two properties do need attention.

**Cache hints are mandatory, not optional.** `skills/list` carries SEP-2549
`ttlMs` / `cacheScope` on 2026-07-28+. The response cache is keyed by
`{server_name, method, params_hash}` with **no caller identity**, and the code
already documents the consequence:

> a cached entry is visible to *every* downstream caller — which is exactly
> what `private` forbids. We therefore decline to cache those results

That safety net only fires if `skills/list` results are routed through
`extract_cache_hint`. A passthrough that treats `skills/list` as an opaque
unknown method **bypasses the check and can leak a private catalog across
tenants.** This is a v1 correctness requirement, not a nicety.

**Per-caller upstream credentials are a real gap.** `McpTransport::Http {
headers }` is static `bitrouter.yaml` config and the connection pool is keyed
by server name, so every caller of the daemon shares one upstream credential. A
private catalog that serves different skills to different users cannot be
expressed — everyone gets one tenant's view. `CallerContext` exists to make
that distinction but does not reach the upstream transport. Out of scope for
v1; see D7. Until then, **document that remote skills catalogs are
daemon-scoped, not caller-scoped.**

## 10. Phases

Each phase is independently shippable and independently valuable.

**Phase 0 — prerequisites (closes B1, B3).** Advertise `resources` in the
gateway's `initialize`. Replace `try_each` with origin-addressed routing. Both
are correct independent of skills; skills make them urgent.

**Phase 1 — method passthrough (closes B2).** Relay unknown methods via
`ClientRequest::CustomRequest` / `CustomResult`, with an allowlist so the
gateway does not become an arbitrary JSON-RPC tunnel. Route list-shaped results
through `extract_cache_hint` (§9).

**Phase 2 — SDK wire types.** `mcp::skills` module, `SkillCatalog` port. No
behaviour change; pure addition.

**Phase 3 — origin server.** `skills/list` + `skills/get` over installed
skills, filesystem `SkillCatalog` impl in `apps/bitrouter`, honest capability
declaration including `directoryRead`. `skills_search` / `skills_get` retained.

**Phase 4 — gateway aggregation.** URI namespacing (§8), label-routed
`resources/read`, eager pagination.

**Phase 5 — decided against, not deferred.** Registry fronting,
`resources/directory/read`, and per-caller credentials are each closed with
reasons in §12 (D7, D8). Nothing here is queued work.

**Retirement — withdrawn.** The plan scheduled removal of the
`bitrouter_skills` stdio injection from
[`gateways.rs:47`](../apps/bitrouter/src/gateways.rs#L47); the step is
withdrawn because its rationale did not survive contact with the tree. The
module doc in `gateways.rs` carries the standing warning, and the plan's Task 9
carries the evidence. The precondition for revisiting is not [#749] landing —
it is the daemon gaining its own skills surface over HTTP.

## 11. Open decisions

**D1 — capability declaration timing.** The gateway answers `initialize`
synchronously but discovers upstream capabilities lazily on first connect, so
it cannot know whether any member serves skills at declaration time.
*Proposed:* declare `extensions: {"io.modelcontextprotocol/skills": {}}`
optimistically; members that do not support it contribute nothing. Cost is one
wasted round trip when no upstream serves skills. Rejected alternative: eager
probing at daemon start, which spawns every stdio child at boot.
`directoryRead: false` in v1 — the SEP forbids clients calling it undeclared,
so false is the safe default and degrades gracefully.

**D2 — non-`skill://` aggregation.** *Proposed:* direct routes only in v1
(§8.4). Needs a call on whether any target upstream actually serves skills
under another scheme; if none do, this stays deferred indefinitely.

**D3 — scope of the `try_each` change. RESOLVED: wholesale** (2026-08-03).
`try_each` is deleted, not narrowed to skill URIs. Aggregate `resources/read`
resolves exactly one owning member or errors; it never silently picks. A URI
that no member lists becomes an error on the aggregate endpoint, directing the
caller to the direct route `POST /mcp/{name}` — a deliberate behaviour change,
accepted because the current behaviour is a silent misroute rather than a
feature. Resolution mechanics and the rejected alternative (label-rewriting
*every* resource URI) are in
[the implementation plan](2026-08-03-skills-over-mcp-plan.md), Task 1.

**D4 — fate of `skills_search` / `skills_get`.** *Proposed:* keep indefinitely
as the compatibility surface (§7). Revisit only when a host we care about can
consume SEP skills end-to-end.

**D5 — `bitrouter skills add`.** *Proposed:* freeze — keep working and
supported, stop adding surface. It is the bridge until hosts can read
MCP-served skills, and it is the documented install path for `skills/bitrouter`
per CLAUDE.md and both plugin manifests. Deprecating it while the SEP has no
consumers leaves zero working surfaces.

**D6 — `_meta` prefix.** SEP-2640 reserves `io.modelcontextprotocol.skills/`
but defines no keys, and an implementer has flagged the unspecified namespace
as an interop hazard. *Proposed:* emit nothing under `_meta` in v1.

**D7 — per-caller upstream credentials. RESOLVED: leave daemon-scoped**
(2026-08-03). Upstream credentials stay static per server in `bitrouter.yaml`
and are shared by every caller of the daemon; the limitation is documented
user-facing in `skills/bitrouter/references/mcp-server.md`. Private *per-user*
remote catalogs are not supported.

Recorded because it is not obvious from the config surface: fixing this is not
just a schema change. `RmcpExecutor` pools upstream connections **keyed by
server name alone**, so per-caller headers would be silently ignored — the
second caller would reuse the first caller's authenticated connection. Any
future attempt must re-key the pool by `(server, credential)` and accept the
resulting change in pool size and connection lifetime. Rejected alternatives:
forwarding the inbound `Authorization` (leaks the caller's BitRouter credential
to a third-party server), and mapping caller → named secret (correct, but wants
config schema plus secret storage on top of the pool work).

**D8 — registry fronting. RESOLVED: do not build** (2026-08-03). See §12.

## 12. Non-goals and decided-against

Everything here is a decision, not a backlog. Reopening one should start from
the reason it was closed.

- **Becoming a host.** Not in this spec, not later. §2 R1/R2.
- **Registry fronting** (D8). Serving `marketplace.json` entries as skills
  needs complete per-file digests, which for an `owner/repo` source means
  clone-and-hash per skill. That would make an inbound MCP request trigger a
  `git clone` and a disk write on the daemon — a request-driven network-fetch
  surface on a local, typically unauthenticated listener. The capability is not
  worth that while SEP-2640 is still draft and no host consumes it. Registry
  browsing stays the CLI's job (`bitrouter skills find` / `add`), where fetching
  is human-initiated, which is also what keeps §2 R1 honest. A pre-warmed
  content-addressed cache (CLI populates, daemon only reads) was considered as
  a middle path and deferred with the rest.
- **Archive distribution.** Removed from the SEP during review over unpacking
  attack surface. Do not implement.
- **`resources/directory/read`, anywhere.** Not on the gateway (D1), and not on
  the origin server either — a skill entry's `resources` already enumerates
  every file, so a host holding the entry can filter by prefix. The optional
  method has nothing to add for a consumer that does not exist yet.
- **Retiring the `bitrouter_skills` stdio injection.** Decided against
  2026-08-03; the original rationale was factually wrong. See the plan's Task 9.
- **Signed or attested skills.** Raised in SEP review, not in the spec text.

## 13. Risk register

| Risk | Assessment |
|---|---|
| SEP-2640 changes before acceptance | Real. Draft since 2026-04-23; sponsor said "2-3 weeks" on 2026-05-11 and it has been quiet since. Mitigated by building only against the settled core (`uri`, `frontmatter`, `resources[]`), which has survived every revision. |
| Contested areas move | Archive distribution was added then removed. Single-skill-multiple-representations is unresolved (moved to Discord 2026-05-27). `_meta` namespace unspecified. All are avoided by §12 and D6. |
| No shipping host consumer | Claude Code cannot consume MCP-served skills end-to-end as of the PR discussion. Mitigated by D4/D5 — keep the surfaces that work. |
| FastMCP `SkillsProvider` divergence | Reconciliation is an unresolved WG priority. If we aggregate a FastMCP upstream before that lands, its URI structure will not match §8's assumptions. Detect and skip rather than mis-rewrite. |
| Phase 0 is a behaviour change | Advertising `resources` makes clients probe upstreams that may not implement them — the original reason for not advertising. Empty lists are the correct answer; verify against a non-resource upstream. |

## 14. Acceptance

- [ ] `resources` advertised; a spec-compliant client completes `initialize` →
      `resources/list` → `resources/read` through `POST /mcp`.
- [ ] Aggregate `resources/read` routes by origin; a test with two members
      serving the same URI asserts the correct member answers, not the first.
- [ ] `skills/list` and `skills/get` traverse the gateway to a stdio upstream
      and an HTTP upstream.
- [ ] `skills/list` results pass through `extract_cache_hint`; a test asserts a
      `cacheScope: private` catalog is not cached.
- [ ] Origin server answers `skills/list` / `skills/get` over installed skills
      with correct `sha256:` digests and verbatim frontmatter.
- [ ] Aggregation rewrites `uri` and every `resources[].uri`; a test asserts the
      final path segment still equals `frontmatter.name` after rewriting.
- [ ] Digests survive rewriting unchanged (content-addressed, asserted).
- [ ] `skills_search` / `skills_get` still work.
- [ ] No daemon path reaches `install::*` (R1 — assert by `rg` in review).
- [ ] `skills/bitrouter/` updated in the same change, per CLAUDE.md, for any
      harness-wiring or CLI change — specifically the `gateways.rs` retirement.
- [ ] `cargo nextest run --all-features`, `cargo clippy --all-features`,
      `cargo fmt -- --check` all clean. No `#[allow]`, no `unwrap`/`expect`/
      `panic!` in non-test code, no dead code, no re-exports from public mods.

[#749]: https://github.com/bitrouter/bitrouter/issues/749

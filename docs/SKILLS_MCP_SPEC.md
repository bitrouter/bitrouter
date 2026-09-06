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
   `bitrouter-observe`, now `bitrouter-telemetry`, not the SDK) and the edge is not a
   cycle; and
   "raw JSON preserves unknown fields" was true only of the type *as written* —
   a `#[serde(flatten)]` catchall gives verbatim passthrough **and** typed
   access. The split also produced a duplicated `SKILL_SCHEME` constant in two
   crates, which is the exact drift the single-source rule exists to prevent.
   The line now falls in three tiers, per §5.
2. **Invalid filesystem skills are not repaired into valid-looking URIs.** The
   Agent Skills specification requires `frontmatter.name` to match the parent
   directory and constrains the name and description. The origin catalog
   validates those rules and skips invalid entries. It also retains the entire
   YAML mapping as JSON so optional and future frontmatter fields pass through
   verbatim. Same-named valid skills in distinct conventional roots remain
   addressable: `.claude/skills/refunds` uses
   `skill://refunds/SKILL.md`, while `skills/refunds` uses the permitted
   organizational prefix `skill://skills/refunds/SKILL.md`.
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

Before this change BitRouter's skills surface combined a package manager with a
pair of custom MCP tools (`skills_search` / `skills_get`). This change removes
the package manager and leaves installation to ecosystem tooling while adding
the SEP methods beside the compatibility tools.

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
3. **"Remote skill" no longer means "clone to disk."** Under SEP-2640 it is a
   `resources/read` against an MCP session. Removing the former install step
   also removes BitRouter from the host's filesystem.

The SEP's own rationale names this shape as a motivating server type:

> a skill gateway fronting an external index (unbounded)

BitRouter deliberately implements the bounded configured-upstream gateway,
not registry fronting; D8 records why request-driven index fetching was
declined along with the former package manager.

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
| Installs skills onto disk | package-manager code | Removed. Ecosystem tools populate the directories BitRouter reads. |
| Puts SKILL.md text into a model's context | `skill_body` | Exists **only** in [`skills_query.rs:85`](../apps/bitrouter/src/skills_query.rs#L85), reached when a model calls the `skills_get` tool. Model-driven, not host-injected. |
| Loads skills into a harness | [`gateways.rs:47`](../apps/bitrouter/src/gateways.rs#L47) | Injects an **MCP server** into the harness. The harness (Claude Code, Codex) is the host; BitRouter is its server. |

Installation is handled by tools such as `npx skills add` and agent plugin
marketplaces. The host is whatever later reads the installed skill or consumes
the MCP-served entry; BitRouter does neither host-side decision.

**Two rules keep it that way, and they are the normative content of this
section:**

- **R1.** BitRouter contains no skill fetching or install-to-disk path. Its
  filesystem side is read-only.
- **R2.** Gateway-sourced skill content must never be written to a filesystem
  skill discovery path. SEP-2640 requires that a host caching MCP-served
  content place it somewhere *excluded from every filesystem-skill discovery
  path*. BitRouter has no gateway content-write path and must not add one.

## 3. Verified pre-change starting state

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
| Package-manager code was isolated to the CLI | `rg` over the pre-change `apps/`, `crates/` |

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

Three tiers. The protocol tier lives in the SDK; the port stays with its
siblings; filesystem parsing and catalog implementation stay in the app.

| Tier | Concern | Home | Rationale |
|---|---|---|---|
| **1 — protocol** | `SKILL_SCHEME`, the `skills/*` method names, `SkillEntry`, `SkillResource`, `ListSkillsResult`, `GetSkillParams/Result`, URI namespacing | `bitrouter-sdk::mcp::skills` | Both halves of the gateway speak these, and so must anything that reasons about skills in flight. Pure `serde_json` — ungated, pulls no rmcp, so a consumer gets it at `default-features = false`. |
| **2 — port** | `SkillCatalog`, `SkillFile`, `SkillFileBody` | `bitrouter-mcp::capabilities::skill_catalog` | A port is "what the app implements for the origin server", not protocol. Its sibling `SkillsQuery` lives here too; moving one to the SDK would be the inconsistency. (The routing port, `RoutingQuery` when this was written, has since become `actions::route::RouteQuery` — ports whose action is unified onto a shared report move next to that report; see `ACTIONS_SPEC.md`.) |
| **3 — filesystem** | frontmatter parsing, discovery, filesystem `SkillCatalog` impl | `apps/bitrouter/src/skills/` and `skills_catalog.rs` | These are binary-local read concerns shared by the SEP catalog, compatibility tools, and surviving `skills list` / `skills init` commands. |

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

The former `bitrouter-skills` crate mixed the read-only format layer with git,
registry, and installation concerns. The package-manager pieces and crate are
removed. The small parser moved into the binary, which keeps the routing SDK
free of distribution concerns while avoiding a dead standalone crate.

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
using the binary-local `skills::format::discover_all_skills`, on the blocking
pool, exactly as `InstalledSkills` does today.

- **URIs.** `skill://<name>/SKILL.md` for a skill installed as
  `.claude/skills/<name>`; `skill://skills/<name>/SKILL.md` for a same-named
  bundled skill under `skills/<name>`. The catalog validates Agent Skills name,
  description, and parent-directory rules before publishing an entry. Every
  filesystem path segment is percent-encoded with RFC 3986 unreserved bytes
  left literal, and reads resolve by re-enumerating that same mapping rather
  than decoding URI text into a path. A symlinked discovery path, a path that
  canonicalizes outside the configured root, or a non-UTF-8 filename rejects
  the skill instead of publishing an incomplete or escaped manifest.
- **Frontmatter.** The parser retains a complete JSON rendering of the YAML
  mapping beside its typed fields. The entry therefore includes `license`,
  `compatibility`, `allowed-tools`, and unknown future fields rather than a
  curated subset.
- **Digests.** SHA-256 over raw bytes of every file in the skill directory,
  `sha256:{64 lowercase hex}`. The MCP listing cache avoids repeated hashing;
  the filesystem catalog itself stays stateless.
- **Capability declaration.** The origin server knows its own catalog at
  startup, so it declares honestly. `directoryRead` remains false: the entry's
  complete `resources` manifest already supports scoped navigation.
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
Configuration rejects labels that are not one non-empty **lowercase** ASCII
URI segment (letters, digits, `-._~`). The lowercase rule matters because a
URI authority is case-insensitive: allowing both `A` and `a` would create two
configuration labels that clients may normalize to the same origin. Before
rewriting, the gateway validates that the top-level URI ends in
`<frontmatter.name>/SKILL.md`; a present `resources` manifest is non-empty,
contains that top-level URI exactly once, has no duplicate URIs, keeps every
resource within the skill directory, and gives every resource a
`sha256:{64 lowercase hex}` digest. Malformed upstream entries are skipped and
reported rather than published under a valid-looking BitRouter namespace.

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
| `skills/get` | strip label from `params.uri`, dispatch to owning member, require the returned URI to equal the requested upstream URI, then rewrite the returned entry |
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
are correct independent of skills; skills make them urgent. If any member's
ownership enumeration fails, routing is indeterminate and fails rather than
silently choosing among the members that happened to answer.

**Phase 1 — method passthrough (closes B2).** Relay unknown methods via
`ClientRequest::CustomRequest` / `CustomResult`, with an allowlist so the
gateway does not become an arbitrary JSON-RPC tunnel. Route list-shaped results
through `extract_cache_hint` (§9).

**Phase 2 — SDK wire types.** `mcp::skills` module, `SkillCatalog` port. No
behaviour change; pure addition.

**Phase 3 — origin server.** `skills/list` + `skills/get` over installed
skills, filesystem `SkillCatalog` impl in `apps/bitrouter`, honest capability
declaration with `directoryRead` absent/false. `skills_search` / `skills_get`
retained.

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

**D5 — package-manager surface. RESOLVED: remove.** BitRouter handles serving
and transport, not distribution. `skills add|remove|find|update`, the
`bitrouter-skills` crate, registry/source resolution, and install-to-disk logic
are removed. `skills list` and `skills init` remain; ecosystem installers and
plugin marketplaces populate the filesystem.

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
- **Registry fronting** (D8). Serving former registry-index entries as skills
  needs complete per-file digests, which for an `owner/repo` source means
  clone-and-hash per skill. That would make an inbound MCP request trigger a
  `git clone` and a disk write on the daemon — a request-driven network-fetch
  surface on a local, typically unauthenticated listener. The capability is not
  worth that while SEP-2640 is still draft and no host consumes it. Registry
  browsing and installation stay with ecosystem tools. A pre-warmed
  content-addressed cache was considered as a middle path and declined with
  the rest.
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
| No shipping host consumer | Claude Code cannot consume MCP-served skills end-to-end as of the PR discussion. Mitigated by D4 (`skills_search` / `skills_get`) and ecosystem filesystem installers. |
| FastMCP `SkillsProvider` divergence | Reconciliation is an unresolved WG priority. If we aggregate a FastMCP upstream before that lands, its URI structure will not match §8's assumptions. Detect and skip rather than mis-rewrite. |
| Phase 0 is a behaviour change | Advertising `resources` makes clients probe upstreams that may not implement them — the original reason for not advertising. Empty lists are the correct answer; verify against a non-resource upstream. |

## 14. Acceptance

- [x] `resources` advertised; a spec-compliant client completes `initialize` →
      `resources/list` → `resources/read` through `POST /mcp`.
- [x] Aggregate `resources/read` routes by origin; a test with two members
      serving the same URI asserts the correct member answers, not the first.
- [x] `skills/list` and `skills/get` traverse the gateway to a stdio upstream
      and an HTTP upstream.
- [x] `skills/list` results pass through `extract_cache_hint`; a test asserts a
      `cacheScope: private` catalog is not cached.
- [x] Origin server answers `skills/list` / `skills/get` over installed skills
      with correct `sha256:` digests and verbatim frontmatter.
- [x] Aggregation rewrites `uri` and every `resources[].uri`; a test asserts the
      final path segment still equals `frontmatter.name` after rewriting.
- [x] Malformed upstream entries and unsafe origin labels cannot enter the
      aggregate namespace; cursor pages are exhausted without weakening cache
      hints.
- [x] Resource manifests are non-empty when present, include `SKILL.md`
      exactly once, contain no duplicate URI, and use valid SHA-256 digests.
- [x] Filesystem discovery rejects symlink escapes and non-UTF-8 resource
      names; reserved and Unicode filename bytes have a bijective
      percent-encoded URI mapping.
- [x] `skills/get` rejects an upstream response for any URI other than the one
      requested.
- [x] Unknown skill/resource URIs remain JSON-RPC `-32602` across origin,
      relay, aggregate, and HTTP error mapping.
- [x] Digests survive rewriting unchanged (content-addressed, asserted).
- [x] `skills_search` / `skills_get` still work.
- [x] Package-manager/install code is absent from the runtime and workspace.
- [x] `skills/bitrouter/` updated in the same change for the changed CLI.
- [x] `cargo nextest run --all-features`, `cargo clippy --all-features`,
      `cargo fmt -- --check` all clean. No `#[allow]`, no `unwrap`/`expect`/
      `panic!` in non-test code, no dead code, no re-exports from public mods.

[#749]: https://github.com/bitrouter/bitrouter/issues/749

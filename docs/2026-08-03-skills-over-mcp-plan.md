# Skills over MCP Implementation Plan

> **Status: Tasks 1–9 implemented (2026-08-03).** All 2223 workspace tests
> pass; clippy and fmt clean. Deviations discovered during implementation are
> recorded in the spec's implementation note — read that before treating any
> task body below as the as-built description.
>
> **Historical execution record.** Post-review hardening superseded several
> task details below: the package-manager crate was removed, format parsing is
> app-local, Agent Skills directory/name rules are strict, `directoryRead` is
> false, and the gateway exhausts raw `skills/list` cursor pages. The code and
> `SKILLS_MCP_SPEC.md` are authoritative; do not execute this plan literally.
>
> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax for tracking.
> Tasks are ordered by dependency; each is independently shippable and leaves
> the workspace green. Implement task-by-task, RED → GREEN → REFACTOR.

**Design spec:** [`SKILLS_MCP_SPEC.md`](SKILLS_MCP_SPEC.md). This plan is the
executable form of it; where they disagree, the spec is the intent and this
plan is wrong.

**Goal:** BitRouter serves the skills it holds and proxies the skills upstream
MCP servers hold, per [SEP-2640](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640)
(`io.modelcontextprotocol/skills`), over stdio and Streamable HTTP alike —
without becoming a skills host.

**As-built architecture:** Skill wire types and routing live in
`bitrouter-sdk::mcp::skills` behind the existing `mcp` feature; read-only
SKILL.md parsing and the filesystem catalog live in `apps/bitrouter`. The
former package-manager crate and install surface are absent. The gateway
namespaces upstream skills under a validated host-assigned label carried in
the URI prefix, and resolves every aggregate `resources/read` to exactly one
owning member or fails when ownership is ambiguous or indeterminate.

**Tech stack:** Rust, serde, `rmcp` 3.1.0 (already a dependency — no bump),
axum, `sha2`, cargo nextest, Clippy, rustfmt.

## Global constraints

- Follow `CLAUDE.md`: no `#[allow(...)]`, no `.unwrap`/`.expect`/`panic!` in
  production code, no re-exports from public modules, no dead code,
  conventional commits with titles under 60 characters.
- **R1 — BitRouter has no skill-fetching or install-to-disk path.** Ecosystem
  tools populate the read-only discovery roots.
- **R2 — gateway-sourced skill content must never be written to a filesystem
  skill discovery path.** The gateway has no content-write path.
- The gateway is an intermediary, not a security boundary. It passes digests
  through unchanged and never re-computes or re-signs them.
- Every behaviour change follows RED → GREEN → REFACTOR. Tests assert observable
  behaviour and hand-derived values, not source text.
- `skills/bitrouter/` is updated in the same change as any CLI flag, port, env
  var, default config, or harness-wiring change (`CLAUDE.md` Agent Skill rule).
- No new runtime dependency beyond `sha2`.

---

## Task 1: Replace `try_each` with deterministic owner resolution

Closes spec **B3**. Resolves **D3 wholesale**: `try_each` is deleted, not
narrowed to skill URIs.

**Files:**
- Modify: `crates/bitrouter-sdk/src/mcp/aggregating_executor.rs`
- Test: unit tests in the file above

**Problem.** `try_each` returns the first member that answers a
`resources/read`. Two members serving the same URI means config order silently
decides, which SEP-2640 names as an impersonation surface. It is a latent
misroute for any colliding resource URI today; skills make it reachable.

**Resolution.** Aggregate `resources/read` resolves exactly one owning member
or errors. Never a silent pick.

1. **Label-prefixed skill URIs** (`skill://<label>/…`, produced by Task 8's
   rewriting): strip the leading label segment, resolve it to a member. Pure
   function, no index, no upstream calls.
2. **Every other URI:** build an ownership index by fanning out
   `resources/list` and `resources/templates/list` across members.
   - exactly one member lists the URI → dispatch there
   - two or more → error naming **every** candidate member
   - zero → error naming the direct route as the remedy

**Cost.** The composition is `AggregatingExecutor → CachingExecutor →
RmcpExecutor` ([`assemble.rs:399`](../apps/bitrouter/src/assemble.rs#L399)), so
index lookups are cache hits when `mcp.cache.enabled` is true. With caching
disabled, an aggregate `resources/read` costs N list calls plus the read; memoize
the index for the duration of the single `dispatch_aggregate` call so one request
never fans out twice.

**Template matching (v1 limitation).** `resources/templates/list` returns RFC
6570 `uriTemplate`s. Full template matching is out of scope: v1 matches on the
literal prefix preceding the first `{`. A URI matching several members'
templates is a collision and errors. Document this in the module doc comment.

**Rejected alternative.** Rewriting *every* resource URI with a label prefix,
mirroring the skill treatment. Rejected because arbitrary schemes are not
reversibly rewritable without per-skill state (which would break the gateway's
statelessness), and because authorities in schemes like `https://` are
meaningful and cannot absorb a label.

**Accepted behaviour change.** An unlisted resource on the aggregate endpoint
becomes an error where it previously might have been served. The direct route
`POST /mcp/{name}` is unaffected and is the documented remedy; the error text
must say so.

**Steps:**

- [ ] RED: test — two members both serving `x://shared`; assert the error names
      both members and that neither is dispatched to.
- [ ] RED: test — exactly one member lists `x://owned`; assert it is dispatched
      to even when it is not first in member order.
- [ ] RED: test — no member lists `x://ghost`; assert the error mentions the
      direct route.
- [ ] RED: test — `skill://acme/refunds/SKILL.md` with member label `acme`
      dispatches to `acme` without any `resources/list` call (assert call count
      on the canned inner executor).
- [ ] GREEN: add `resolve_owner(&self, members, uri) -> Result<&AggregateMember>`
      with the two-tier resolution above; memoize the index per dispatch call.
- [ ] GREEN: replace the `"resources/read" => self.try_each(...)` arm with
      `resolve_owner` + single dispatch. Delete `try_each` entirely.
- [ ] REFACTOR: update the module doc table (the `resources/read` row currently
      reads "try each member, first success wins") and document the template
      limitation.
- [ ] Verify: `cargo nextest run -p bitrouter-sdk --all-features`.

---

## Task 2: Advertise `resources` on the gateway

Closes spec **B1**.

**Files:**
- Modify: `crates/bitrouter-sdk/src/server.rs`
- Test: unit tests in the file above; `apps/bitrouter/tests/e2e.rs`

**Problem.** [`server.rs:334`](../crates/bitrouter-sdk/src/server.rs#L334)
answers `initialize` with `"capabilities": { "tools": {} }`. Skills ride
entirely on `resources/read`, so a spec-compliant client never issues one. The
existing comment's rationale — avoid probing upstreams that may not implement
resources — stops holding once skills are in scope; an upstream without
resources contributes an empty list, which is the correct answer.

**Steps:**

- [ ] RED: test — `initialize` response advertises `resources`.
- [ ] RED: test — aggregate `resources/list` with a member that does not
      implement resources returns an empty array plus a `_bitrouterErrors`
      entry, not a hard failure.
- [ ] GREEN: add `"resources": {}` to the advertised capabilities; rewrite the
      stale comment to explain the empty-list contract.
- [ ] Verify: a real MCP client completes `initialize` → `resources/list` →
      `resources/read` against `POST /mcp`. Record the client used.

---

## Task 3: Extension-method passthrough

Closes spec **B2**, and implements the §9 cache-hint requirement.

**Files:**
- Modify: `crates/bitrouter-sdk/src/mcp/rmcp_executor.rs`
- Modify: `crates/bitrouter-sdk/src/mcp/caching_executor.rs`
- Test: unit tests in the files above

**Problem.** Both executors close their method tables; `rmcp_executor.rs:825`
returns "not supported by v1.0 RmcpExecutor" for anything outside the base
catalogue. `rmcp` 3.1.0 already carries `ClientRequest::CustomRequest` and
`CustomResult` (`rmcp-3.1.0/src/model.rs:948`, `:4461`), so passthrough needs no
dependency change.

**Security requirement — not optional.** `skills/list` carries SEP-2549
`ttlMs` / `cacheScope`. The response cache key is
`{server_name, method, params_hash}` with **no caller identity**
([`caching_executor.rs:102`](../crates/bitrouter-sdk/src/mcp/caching_executor.rs#L102)),
and the existing code declines to cache `cacheScope: private` precisely because
a cached entry is visible to every downstream caller. A passthrough that treats
`skills/list` as an opaque unknown method **bypasses that check and can leak a
private catalog across tenants.** Passed-through list-shaped results must route
through `extract_cache_hint`.

**Allowlist, not a tunnel.** Relay only methods on an explicit allowlist —
initially `skills/list`, `skills/get`, `resources/directory/read`. An
unrecognized method keeps returning `-32601`. The gateway must not become an
arbitrary JSON-RPC tunnel to upstreams.

**Steps:**

- [ ] RED: test — `skills/list` reaches the upstream peer and its result is
      returned verbatim.
- [ ] RED: test — an off-allowlist method still returns `-32601`.
- [ ] RED: test — a passed-through result carrying `cacheScope: "private"` is
      **not** cached (assert a second call hits the upstream again).
- [ ] RED: test — a passed-through result carrying `ttlMs` **is** cached for
      that duration.
- [ ] GREEN: add an `EXTENSION_METHODS` allowlist and a `CustomRequest` arm to
      `dispatch`, mapping `CustomResult` back to `serde_json::Value`.
- [ ] GREEN: extend `CachingExecutor`'s cacheable-method set to the list-shaped
      allowlist entries, reusing `extract_cache_hint` unchanged.
- [ ] REFACTOR: update the `rmcp_executor.rs` module doc — "the spec catalogue
      is closed" is no longer true; state the allowlist policy instead.
- [ ] Verify: `cargo nextest run -p bitrouter-sdk --all-features`.

---

## Task 4: Skill wire types and the `SkillCatalog` port

Spec §6. Pure addition, no behaviour change.

**Files:**
- Create: `crates/bitrouter-sdk/src/mcp/skills.rs`
- Modify: `crates/bitrouter-sdk/src/mcp/mod.rs` (add `pub mod skills;` — no
  re-exports, per `CLAUDE.md` #2)
- Test: unit tests in the new file

**Interfaces:**

```rust
pub struct SkillResource { pub uri: String, pub digest: String }

pub struct SkillEntry {
    pub uri: String,
    /// Verbatim SKILL.md frontmatter as JSON — never a curated subset.
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    /// Complete file enumeration. `None` only for dynamically generated
    /// skills; distinct from `Some(vec![])`, which is invalid.
    pub resources: Option<Vec<SkillResource>>,
}

pub struct ListSkillsParams { pub cursor: Option<String> }
pub struct ListSkillsResult {
    pub skills: Vec<SkillEntry>,
    pub next_cursor: Option<String>,
    pub ttl_ms: Option<i64>,
    pub cache_scope: Option<String>,
}
pub struct GetSkillParams { pub uri: String }
pub struct GetSkillResult { pub skill: SkillEntry }

#[async_trait]
pub trait SkillCatalog: Send + Sync {
    async fn list(&self, cursor: Option<&str>) -> Result<ListSkillsResult>;
    async fn get(&self, uri: &str) -> Result<GetSkillResult>;
}
```

Field naming is `camelCase` on the wire (`nextCursor`, `ttlMs`, `cacheScope`)
via serde rename, matching the base protocol.

**Steps:**

- [ ] RED: test — a `skills/list` result from the SEP text deserializes and
      re-serializes byte-identically (round-trip on the spec's own example).
- [ ] RED: test — `resources: None` omits the key entirely on serialize;
      `Some(vec![])` emits `[]`. The distinction must survive a round trip.
- [ ] RED: test — unknown frontmatter fields (`license`, `metadata`, a
      future field) survive round-tripping untouched.
- [ ] GREEN: implement the types and the port.
- [ ] Verify: `cargo nextest run -p bitrouter-sdk --all-features`.

---

## Task 5: Digest computation over a skill directory

Spec §7. Extracted from Task 6 because it is the only part needing a new
dependency and it is independently testable.

**Files:**
- Modify: `apps/bitrouter/Cargo.toml` (add `sha2`)
- Modify: `Cargo.toml` (workspace dep)
- Create: `apps/bitrouter/src/skills_catalog.rs`
- Test: unit tests in the new file

**Contract.** SHA-256 over raw file bytes, formatted `sha256:{hex}` with 64
lowercase hex characters. `resources` enumerates every file in the skill
directory, each exactly once, including `SKILL.md` itself. Cached by
`(path, mtime, len)`; a miss re-hashes. All filesystem work on the blocking
pool, matching `InstalledSkills`.

**Steps:**

- [ ] RED: test — digest of a known byte string matches a hand-computed
      SHA-256, formatted with the `sha256:` prefix and lowercase hex.
- [ ] RED: test — a skill with `SKILL.md`, `references/GUIDE.md`, and
      `scripts/x.py` enumerates exactly three resources, `SKILL.md` included.
- [ ] RED: test — touching a file changes its digest; touching nothing leaves
      the cached digest and does not re-read.
- [ ] RED: test — a symlink pointing outside the skill directory is refused,
      not followed.
- [ ] GREEN: implement `digest_file`, `enumerate_skill_resources`, and the
      mtime cache.
- [ ] Verify: `cargo nextest run -p bitrouter --all-features`.

---

## Task 6: Origin server — `skills/list` and `skills/get`

Spec §7.

**Files:**
- Create: `crates/bitrouter-mcp/src/capabilities/skill_catalog.rs`
- Modify: `crates/bitrouter-mcp/src/capabilities/mod.rs`
- Modify: `crates/bitrouter-mcp/src/server.rs`
- Modify: `apps/bitrouter/src/skills_catalog.rs` (implement `SkillCatalog`)
- Test: unit tests in the files above

**Behaviour.**
- **URI derivation — the invariant does not hold by construction.**
  `install_as` lets the installed directory name differ from the frontmatter
  name: `install_name = install_as.unwrap_or(&fm.name)`
  ([`install.rs:196`](../crates/bitrouter-skills/src/install.rs#L196), and the
  `install_as_overrides_directory_name` test pins the behaviour). A skill
  installed as `pinned-dir/` with `name: upstream-name` would produce
  `skill://pinned-dir/SKILL.md`, whose final segment is **not**
  `frontmatter.name` — a SEP violation. The URI is therefore derived from the
  **frontmatter name**, never the directory name:

  | Case | URI |
  |---|---|
  | directory name == frontmatter name (common) | `skill://<name>/SKILL.md` |
  | they differ (`install_as` was used) | `skill://<dir>/<name>/SKILL.md` |

  Both are legal: in the second form `<dir>` is the "server-chosen
  organizational prefix" the SEP permits, and the final segment is still the
  name. The second form also disambiguates two installs that share a
  frontmatter name. If two installed skills would still collide on the full
  URI, that is a genuine conflict — report both in `_bitrouterErrors` rather
  than silently dropping one.
- `skills/get` on a URI this server does not serve returns `-32602`
  (Invalid params) — the code SEP-2640 mandates, matching `resources/read`.
- `resources/read` serves any file under a skill directory and refuses
  traversal outside it.
- Capability declaration is **honest here** — the origin server knows its own
  catalog at startup, so it declares `directoryRead: true` and implements
  `resources/directory/read` over a filesystem walk. This is the deliberate
  asymmetry with the gateway (Task 7, D1).

**`skills_search` / `skills_get` are retained** (D4). They are the surface that
works with clients today; the SEP methods have no shipping host consumer yet.
Do not remove them, and do not reimplement them in terms of `SkillCatalog`
unless the tests stay green unchanged.

**Steps:**

- [ ] RED: test — `skills/list` over a temp root with two installed skills
      returns two entries with correct URIs, verbatim frontmatter, and complete
      `resources`.
- [ ] RED: test — the final path segment of every returned `uri` equals that
      entry's `frontmatter["name"]`.
- [ ] RED: test — a skill installed via `install_as` (directory name differs
      from frontmatter name) yields `skill://<dir>/<name>/SKILL.md`, and the
      final segment still equals `frontmatter["name"]`.
- [ ] RED: test — two installed skills sharing a frontmatter name both appear,
      distinguished by their directory prefix; neither is dropped.
- [ ] RED: test — `skills/get` on an unknown URI returns `-32602`.
- [ ] RED: test — `resources/read` on `skill://x/../../etc/passwd` is refused.
- [ ] RED: test — `skills_search` / `skills_get` still return what they
      returned before (existing tests must pass unmodified).
- [ ] GREEN: implement the capability, the router slice, and the app-side
      `SkillCatalog`.
- [ ] GREEN: declare the extension with `directoryRead: true` and implement
      `resources/directory/read`.
- [ ] Verify: `cargo nextest run --all-features`.

---

## Task 7: Gateway capability declaration

Spec §8, decision **D1**.

**Files:**
- Modify: `crates/bitrouter-sdk/src/server.rs`
- Test: unit tests in the file above

**Decision.** Declare
`extensions: { "io.modelcontextprotocol/skills": { "directoryRead": false } }`
optimistically. The gateway answers `initialize` synchronously but discovers
upstream capabilities lazily, so it cannot know at declaration time whether any
member serves skills. Members that do not contribute nothing; the cost is one
wasted round trip when no upstream serves skills. The rejected alternative —
eager probing at daemon start — spawns every stdio child at boot.

`directoryRead: false` because SEP-2640 forbids clients calling it undeclared;
false is the safe default and degrades gracefully.

**Steps:**

- [ ] RED: test — `initialize` advertises the extension with
      `directoryRead: false`.
- [ ] RED: test — `skills/list` against an aggregate whose members serve no
      skills returns `{"skills": []}`, not an error.
- [ ] GREEN: implement.
- [ ] Verify: `cargo nextest run -p bitrouter-sdk --all-features`.

---

## Task 8: Gateway aggregation with URI namespacing

Spec §8. The hard task; do not start it before Tasks 1, 3, and 4 are green.

**Files:**
- Modify: `crates/bitrouter-sdk/src/mcp/aggregating_executor.rs`
- Modify: `crates/bitrouter-sdk/src/mcp/skills.rs` (rewriting helpers)
- Test: unit tests in the files above

**Rewriting.** Each member's BitRouter-configured `server_name` is its
host-assigned label — which is what SEP-2640 requires ("a host-assigned label,
not the self-reported `serverInfo.name`").

```
upstream   skill://refunds/SKILL.md          (member "acme")
gateway    skill://acme/refunds/SKILL.md
```

Legal without qualification: preceding segments are "a server-chosen
organizational prefix". Invariants that must be asserted by test:

- the final segment still equals `frontmatter.name`
- `frontmatter` passes through byte-identical
- digests pass through unchanged — they are over content bytes, not URIs

Every `resources[].uri` in an entry is rewritten with the same prefix.

**Dispatch:**

| Inbound | Behaviour |
|---|---|
| `skills/list` | fan out, rewrite `uri` and every `resources[].uri`, concat; partial failures under `_bitrouterErrors` per existing convention |
| `skills/get` | strip label from `params.uri`, dispatch to the owning member, rewrite the returned entry |
| `resources/read` on `skill://<label>/…` | Task 1 tier 1 — label-routed |
| `resources/directory/read` | not relayed on the aggregate (D1); direct route only |

**Pagination.** v1 aggregates every upstream page eagerly, reusing the
`list_all_preserving!` pattern from `rmcp_executor.rs`, and returns no
`nextCursor`. Acceptable while catalogs are small; revisit when a member's
catalog is large enough to matter.

**Scheme restriction (D2).** Only `skill://` skills are aggregated. A member
serving skills under another scheme (SEP-2640 permits e.g. `github://…`) is
reachable through its direct route `POST /mcp/{name}`, unrewritten, where no
collision is possible. Reversing a rewrite of an arbitrary scheme would require
per-skill state, which breaks statelessness. **Detect and skip** non-`skill://`
entries during aggregation rather than mis-rewriting them — this is also the
mitigation for FastMCP's divergent URI structure (spec §13).

**Steps:**

- [ ] RED: test — two members both serving `skill://refunds/SKILL.md` produce
      two distinct entries, `skill://a/refunds/…` and `skill://b/refunds/…`,
      with neither shadowing the other.
- [ ] RED: test — after rewriting, every entry's final path segment still
      equals its `frontmatter["name"]`.
- [ ] RED: test — digests are byte-identical before and after rewriting.
- [ ] RED: test — `resources[].uri` entries are rewritten with the same prefix
      as the parent `uri`.
- [ ] RED: test — `skills/get` on `skill://a/refunds/SKILL.md` dispatches to
      member `a` with the label stripped from `params.uri`.
- [ ] RED: test — a member serving `github://o/r/skills/x/SKILL.md` is skipped
      with a `_bitrouterErrors` note, not mis-rewritten.
- [ ] RED: test — one member failing `skills/list` does not fail the aggregate.
- [ ] GREEN: implement rewriting helpers in `skills.rs` (pure functions, both
      directions) and the dispatch arms in `aggregating_executor.rs`.
- [ ] REFACTOR: extend the module doc dispatch table with the skills rows.
- [ ] Verify: `cargo nextest run --all-features`; end-to-end against one stdio
      and one HTTP upstream serving skills.

---

## Task 9: Documentation, invariant enforcement, and retirement

**Files:**
- Modify: `skills/bitrouter/SKILL.md` and/or `skills/bitrouter/references/`
- Modify: `docs/CLI.md` if any CLI surface changed
- Modify: `apps/bitrouter/src/gateways.rs` (retirement — see gating below)
- Modify: `CHANGELOG.md`

**Steps:**

- [ ] Assert R1 in review: `rg 'install::(install|remove|list_installed)'`
      returns hits only under `apps/bitrouter/src/commands.rs` and
      `crates/bitrouter-skills/`.
- [ ] Assert R2 in review: no Task 5/8 code path writes under `.claude/skills`.
- [ ] Document that remote skills catalogs are **daemon-scoped, not
      caller-scoped** (spec §9, D7) — one upstream credential is shared by every
      caller of the daemon. This is a documentation obligation, not a bug fix.
- [ ] Document that the gateway is an intermediary and that a digest match is
      not a security boundary (spec §8.5).
- [ ] Update `skills/bitrouter/` for the new MCP surface, per the `CLAUDE.md`
      lockstep rule.
- [x] ~~**Gated on [#749]:** remove the `bitrouter_skills` stdio injection…~~
      **Withdrawn 2026-08-03 — the rationale was wrong.** Do not do this.

### Why the retirement was withdrawn

This step claimed the injection's "only consumer is the orchestrator being
dissolved". That is false, and it was my error when writing this plan. Verified
against the tree:

| Claim | Reality |
|---|---|
| Only the TUI consumes it | Two consumers: `tui/mod.rs` (3 sites) **and** [`main.rs:2071`](../apps/bitrouter/src/main.rs#L2071), inside `if backend == Some(McpBackend::Fleet)` — the fleet backend handing gateway servers to spawned ACP subagents |
| The orchestrator is being dissolved | [#749] and #748 are still **open**; nothing has moved |
| "The HTTP surface exists before the stdio one goes" | It does not. `InstalledSkillCatalog` is wired only into the stdio `--backend skills` path; `assemble.rs` never references it, and the aggregate `/mcp` proxies configured upstreams only — **there is no HTTP path to the daemon's own installed skills** |

Removing the injection today would therefore be a straight capability loss for
both TUI-launched harnesses and ACP subagents, with nothing to replace it.

Retirement would first require making the daemon serve its own installed skills
through the aggregate — a reserved in-process member — which needs a new
executor seam, because `McpTarget::Direct` assumes a dialable transport and a
daemon serving itself has none. That is real work in service of removing
something that currently functions correctly. Not worth it; the stdio
subprocess is a clean way to serve origin content.

**If you revisit this:** the trigger is not #749 landing. It is the daemon
gaining its own skills surface over HTTP. Until then, leave `gateways.rs` alone.

---

## Out of scope

Decided against per spec §12 — these are closed decisions, not a backlog.

- **Registry fronting** — would make an inbound MCP request trigger a `git
  clone` and disk write on the daemon (D8). Registry browsing stays with the
  CLI, where fetching is human-initiated.
- **Archive distribution** — removed from the SEP during review over unpacking
  attack surface.
- **`resources/directory/read`** — not on the gateway (D1) and not on the origin
  server either; an entry's `resources` already enumerates every file.
- **Per-caller upstream credentials** — daemon-scoped and documented (D7). Note
  that fixing it also requires re-keying `RmcpExecutor`'s connection pool, which
  today keys by server name alone.
- **Signed or attested skills.**
- **Deprecating `bitrouter skills add`** — **frozen, not removed** (D5). It
  stays working and supported; stop adding surface to it.

## Acceptance

The spec's §14 checklist is the acceptance gate. Additionally, for this plan:

- [ ] `cargo nextest run --all-features` green.
- [ ] `cargo clippy --all-features` clean.
- [ ] `cargo fmt -- --check` clean.
- [ ] No `#[allow]`, no `.unwrap`/`.expect`/`panic!` outside tests, no dead
      code, no public-module re-exports.

[#749]: https://github.com/bitrouter/bitrouter/issues/749

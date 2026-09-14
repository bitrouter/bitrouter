# Skills over MCP native-conformance implementation plan

> **Status: implementation complete; external interoperability gate pending.**
> The baseline was commit `9b5e581dbb95f0cd3b888303433d722b87f0bc01`
> (2026-09-13). Phases 0–5 are implemented in this worktree; Phase 6 records
> what still requires external tools or a named host/client.
>
> This is a follow-up to the completed historical
> [`2026-08-03-skills-over-mcp-plan.md`](2026-08-03-skills-over-mcp-plan.md) and
> [`SKILLS_MCP_SPEC.md`](SKILLS_MCP_SPEC.md). It does not reopen their server /
> gateway / host boundary. It closes gaps found by comparing the as-built wire
> behavior with the now-stable
> [MCP Skills extension](https://github.com/modelcontextprotocol/ext-skills/blob/main/specification/stable/skills.mdx).

## Outcome

After this plan, BitRouter can accurately claim:

> BitRouter is a conformant MCP Skills server for its local stdio catalog and a
> conformant Skills gateway for configured upstream MCP servers. It preserves
> skill identity and cache semantics across aggregation. Skill approval,
> verification at load time, and activation remain responsibilities of the MCP
> host.

The claim remains role- and transport-specific. This plan does not claim that
every MCP client activates skills, or that BitRouter itself is a skill host.
The official
[Skills client-support matrix](https://modelcontextprotocol.io/extensions/skills/overview#client-support)
remains the source of truth for host support.

## Verified baseline

| Surface | Current behavior | Gap |
|---|---|---|
| Local origin | `bro mcp serve --backend skills` advertises Resources and `io.modelcontextprotocol/skills`; implements `skills/list`, `skills/get`, and `resources/read` | Skills results omit required `resultType`, `ttlMs`, and `cacheScope` |
| Skill resources | Complete manifests include raw frontmatter, SHA-256 digest, byte size, nested files, and traversal-safe reads | `resources/list` does not use the skill name and description for the `SKILL.md` resource; read/list cache hints are absent |
| Direct upstream route | Relays allowlisted Skills methods and `resources/read`; `skills/list` exhausts upstream pages and conservatively merges hints that are present | Missing required hints can still reach the downstream response |
| Aggregate route | Namespaces `skill://` URIs by configured server label and routes reads back to one owner | Rebuilds bare results and discards required result/cache envelopes |
| Downstream HTTP lifecycle | Implements legacy `initialize`, notification acknowledgement, and `ping` manually | Does not implement the MCP `2026-07-28` stateless lifecycle or `server/discover` |
| Upstream lifecycle | `mcp.upstream_protocol: "2026-07-28"` uses modern discovery; default `latest` uses legacy `2025-11-25` | Native Skills discovery is not the default path even though the extension is specified against `2026-07-28+` |
| Compatibility tools | `skills_search` and `skills_get` work as ordinary MCP tools | Useful fallback, but not native host activation |

External baseline evidence:

- `cargo test -p bitrouter-mcp skills -- --nocapture` and
  `cargo test -p bitrouter-sdk mcp::skills --all-features -- --nocapture`:
  29 targeted tests passed.
- Current MCP Inspector CLI against the built `bro` binary verified 3 skills
  and 29 files with no manifest, frontmatter, size, or digest errors.
- That Inspector run negotiated `2025-11-25`; its successful manifest check is
  evidence for skill-entry integrity, not proof of the missing `2026-07-28`
  lifecycle and result envelopes.

## `rmcp` review and upgrade decision

### Pre-change dependency snapshot

- Workspace requirement: `rmcp = "3.0"` (caret range).
- Lockfile: `rmcp 3.1.0` and `rmcp-macros 3.1.0`.
- Current published release checked on 2026-09-13: `3.3.0`.
- `cargo update --dry-run -p rmcp --precise 3.3.0` changes only `rmcp`,
  `rmcp-macros`, and `process-wrap` (`9.1.0` to `10.0.0`) in this lockfile.
- BitRouter's MSRV is Rust 1.93; `rmcp 3.3.0` requires Rust 1.88, so the upgrade
  does not raise the workspace MSRV.

**As built:** the workspace requirement is `rmcp = "3.3"`; the single
lockfile resolution is `rmcp 3.3.0` / `rmcp-macros 3.3.0`, with
`process-wrap 10.0.0`.

Relevant upstream changes since 3.1.0, from the official
[`rmcp` releases](https://github.com/modelcontextprotocol/rust-sdk/releases):

| Release | Relevant change | BitRouter consequence |
|---|---|---|
| 3.1.1 | Handler macros emit cache hints correctly; MRTR fixes | Reduces special-case risk, but does not model Skills methods |
| 3.1.2–3.1.4 | SSE/auth hardening, discovery timeout and outcome classification, pre-init metadata errors | Improves failure behavior for origin and upstream connections |
| 3.2.0 | Concurrent Streamable HTTP requests; fallback after sessionless discovery rejection; legacy `initialize` correction | Material reason to upgrade before changing lifecycle defaults |
| 3.3.0 | `ServerHandler::negotiate_initialize`; SSE reconnect overflow fix; auth additions | Lets custom handlers reuse official negotiation rather than restating it |

`rmcp 3.3.0` still deliberately defines `ProtocolVersion::LATEST` as
`2025-11-25`, while separately supporting `V_2026_07_28`. It also does not ship
typed `skills/list`, `skills/get`, or `Skill` models. Therefore:

1. Upgrade before the protocol work, and raise the manifest floor to `3.3` if
   BitRouter uses the new API.
2. Keep BitRouter's extension wire types in `bitrouter-sdk::mcp::skills`.
3. Do not describe the dependency bump alone as Skills conformance.

## Invariants and non-goals

- BitRouter remains a **server and gateway, not a host**. It does not load MCP
  skill instructions into model context, approve `allowed-tools`, or implement
  a host's consent and activation window.
- Origin server identity plus skill URI remains the cross-server identity.
  Aggregate URI namespacing must stay reversible and collision-safe.
- No MCP skill content is written into filesystem discovery roots.
- The gateway never recomputes or attests to an upstream digest. It preserves
  the upstream manifest while rewriting only the URI namespace it owns.
- Compatibility tools remain available and are documented as a fallback.
- `resources/directory/read` remains out of scope while `directoryRead` is
  false.
- Public HTTP serving of the user's installed local skills is a separate
  product/security decision; it is not smuggled into lifecycle conformance.
- No CLI, port, environment variable, harness wiring, or agent-plugin command
  changes are planned. If review expands the scope to any of those, update
  `skills/bitrouter/` and all agent-plugin manifests in the same change.

## Implementation sequence

Each phase is independently reviewable and must leave the workspace green.
Use RED -> GREEN -> REFACTOR for behavior changes.

### Phase 0 — upgrade `rmcp` and freeze the baseline

**Files**

- `Cargo.toml`
- `Cargo.lock`
- Compile-fix sites only if required by 3.3.0

**Tasks**

- [x] Change the workspace minimum from `3.0` to `3.3`; resolve the lockfile to
      `rmcp 3.3.0`, `rmcp-macros 3.3.0`, and `process-wrap 10.0.0`.
- [ ] Run the existing MCP lifecycle, multitenant HTTP, stdio smoke, gateway
      round-trip, and full workspace suites before changing wire behavior.
- [x] Record any behavior difference separately from compiler fixes. Do not
      fold a lifecycle-default change into the dependency PR.
- [x] Add or retain a test proving `rmcp::ProtocolVersion::LATEST` remains
      `2025-11-25`; do not infer modern protocol selection from the crate
      version.

**Acceptance**

- Lockfile contains one `rmcp`/`rmcp-macros` version, both `3.3.0`.
- No feature flags are broadened.
- Existing legacy and modern lifecycle tests pass unchanged.

Implementation note: the dependency-only `cargo check --workspace
--all-features` passed before wire edits. The full pre-change test run was not
captured, so that sequencing checkbox remains open. rmcp 3.3 intentionally
changed one asserted behavior: asking for `2026-07-28` through legacy
`initialize` now negotiates `2025-11-25`, because the modern revision uses the
stateless lifecycle. The fixture now records that behavior explicitly.

### Phase 1 — make Skills results conformant at the origin

The stable extension defines `ListSkillsResult` as `PaginatedResult +
CacheableResult` and `GetSkillResult` as `CacheableResult`. Both require
`resultType: "complete"`, `ttlMs`, and `cacheScope`.

**Files**

- `crates/bitrouter-sdk/src/mcp/skills.rs`
- `crates/bitrouter-mcp/src/server.rs`
- `crates/bitrouter-mcp/tests/stdio_smoke.rs`
- `crates/bitrouter-mcp/tests/gateway_roundtrip.rs`

**Tasks**

- [x] Extend the pure-serde Skills result types with the required result and
      cache fields. Preserve `#[serde(flatten)]` passthrough for unknown fields.
- [x] Emit the full stable envelope on every Skills extension response. The
      extension has no pre-2026 result contract, so do not remove these fields
      merely because a partial client used a legacy handshake.
- [x] Origin `skills/list`: emit `resultType: "complete"`, `ttlMs: 60000`,
      `cacheScope: "public"`. This matches the existing 60-second local listing
      cache and the catalog is fixed for every caller of one server instance.
- [x] Origin `skills/get`: emit `resultType: "complete"`, `ttlMs: 0`,
      `cacheScope: "public"`. Zero avoids treating a point-in-time manifest as
      fresh after files change while accurately describing caller visibility.
- [x] Preserve `nextCursor` if pagination is later introduced, but do not add a
      producerless cursor field solely for symmetry.
- [x] Keep unknown skill and unknown file errors at JSON-RPC `-32602`.

**Tests**

- Exact wire assertions for list/get contain all three required fields.
- Unknown frontmatter and top-level skill fields survive round trips.
- Inspector still verifies every declared digest and size.
- A partial client using a legacy handshake can ignore the additive extension
  fields and still read the catalog.

### Phase 2 — complete the origin Resources contract

**Files**

- `crates/bitrouter-mcp/src/server.rs`
- `apps/bitrouter/src/skills_catalog.rs` only if the catalog port needs richer
  metadata
- Origin server unit and stdio tests

**Tasks**

- [x] For each skill's `SKILL.md` resource, populate `name` and `description`
      from its verified frontmatter and keep `mimeType: text/markdown`.
- [x] For `2026-07-28` peers, return complete cacheable envelopes on
      `resources/list` and `resources/read`.
- [x] Use `ttlMs: 60000`, `cacheScope: "public"` for the resource listing.
- [x] Use `ttlMs: 0`, `cacheScope: "public"` for file reads so an entry refresh
      cannot be paired with stale content.
- [x] Preserve rmcp's legacy result-type stripping and explicitly suppress
      cache fields for base-protocol `2025-11-25` peers.

**Acceptance**

- The Resources metadata SHOULDs in the Skills extension are met.
- Modern wire fixtures contain the required cache fields; legacy fixtures do
  not gain fields outside their revision.
- Binary content stays base64 and text content stays byte-for-byte equivalent
  to the manifest digest input.

### Phase 3 — preserve and merge envelopes through aggregation

**Files**

- `crates/bitrouter-sdk/src/mcp/aggregating_executor.rs`
- `crates/bitrouter-sdk/src/mcp/caching_executor.rs`
- `crates/bitrouter-sdk/src/mcp/rmcp_executor.rs`
- Unit and real-transport gateway round-trip tests

**Policy**

The direct route preserves one upstream origin and does not rewrite its skill
identity, but BitRouter may consolidate its pages and owns the downstream
response envelope. The aggregate route additionally rewrites URI namespaces
and combines origins. Both routes must therefore produce a conformant
downstream envelope even when an upstream is older or incomplete; neither may
alter frontmatter, resource digests, sizes, or file bytes.

For aggregate cache hints:

- shortest `ttlMs` wins;
- `private` or an unknown non-public scope dominates `public`;
- a failed member, missing hint, or malformed hint contributes the conservative
  fallback `ttlMs: 0`, `cacheScope: "private"`;
- partial member errors remain visible under `_bitrouterErrors` and the
  aggregate operation still has `resultType: "complete"`.

**Tasks**

- [x] Extract one tested pure helper for result-type validation and conservative
      cache-hint merging. Reuse it for every aggregate list, not Skills alone.
- [x] `skills/list`: namespace entries as today, merge page/member hints, then
      emit the full `ListSkillsResult` envelope.
- [x] `skills/get`: copy the upstream envelope, replace only the validated skill
      entry with the namespaced entry, and conservatively fill missing cache
      fields.
- [x] Direct `skills/list` / `skills/get`: preserve the upstream entry and
      origin, but fill a missing result/cache envelope with `complete`, `0`, and
      `private`. Keep the existing conservative merge when list pages disagree.
- [x] Empty aggregate catalog: return complete, `ttlMs: 0`, and
      `cacheScope: "public"`.
- [x] Apply the same no-envelope-loss rule to `tools/list`, `resources/list`,
      `resources/templates/list`, `prompts/list`, and `resources/read` before
      advertising `2026-07-28` downstream.
- [x] Keep private results out of BitRouter's cross-caller cache. Add an
      end-to-end assertion, not only a helper-unit test.

**Acceptance**

- Two upstream pages and two upstream members demonstrate minimum TTL and most
  restrictive scope.
- Same-name skills from two servers remain distinct after listing, get, and
  read.
- A malformed/nonconformant member cannot cause a public cache of a private or
  unknown catalog.
- Direct-route skill entries and resource content remain byte-equivalent to the
  upstream data; only pagination consolidation and the downstream result
  envelope may differ.

### Phase 4 — implement the modern downstream HTTP lifecycle

The aggregate/direct HTTP routes currently implement a legacy JSON-RPC subset
without depending on rmcp. Preserve that executor-agnostic SDK property rather
than making the `server` feature require the optional rmcp-backed client.

**Files**

- `crates/bitrouter-sdk/src/server.rs`
- `crates/bitrouter-sdk/src/mcp/` for a pure lifecycle/envelope helper if
  extraction keeps `server.rs` focused
- `apps/bitrouter/tests/e2e.rs`

**Tasks**

- [x] Add `2026-07-28` to the downstream supported-version set only in the same
      commit that implements all required behavior.
- [x] Implement `server/discover`, advertising the gateway identity, Resources,
      and `io.modelcontextprotocol/skills` with `directoryRead: false`.
- [x] Validate the required self-contained request metadata:
      `io.modelcontextprotocol/protocolVersion` and
      `io.modelcontextprotocol/clientCapabilities`; accept optional
      `io.modelcontextprotocol/clientInfo`.
- [x] Enforce the `MCP-Protocol-Version` HTTP header and metadata agreement for
      modern requests, while preserving Origin/Host protections.
- [x] Treat modern HTTP as stateless: no `Mcp-Session-Id`, standalone GET/DELETE
      stream, or connection-scoped initialization assumption.
- [x] Strip downstream hop lifecycle identity fields before upstream dispatch.
      The upstream rmcp client must generate BitRouter's own client identity and
      capabilities; it must not forward a downstream client's identity as if
      that client directly owned the upstream connection.
- [x] Keep legacy `initialize`, initialized/cancelled notification handling, and
      `ping` for older clients.
- [x] Keep rmcp-backed origin handlers on the SDK's negotiation path rather
      than duplicating it. Cover the pure gateway negotiation with matching
      fixtures so the two surfaces cannot silently disagree.

**Tests**

- `server/discover` over real HTTP returns `2026-07-28`, BitRouter server info,
  Resources, and Skills capabilities.
- Handshake-free `skills/list`, `skills/get`, and `resources/read` with complete
  `_meta` succeed.
- Missing/malformed metadata returns `-32602`; unsupported versions return HTTP
  400 as required by the transport.
- Legacy `2025-11-25` initialize and subsequent requests remain green.
- Downstream client identity is observable at the BitRouter hook boundary but
  is not replayed as upstream client identity.

### Phase 5 — make upstream modern discovery usable by default

This is a behavior decision, not a mechanical SDK consequence. A conforming
Skills server may expose the extension only on MCP `2026-07-28+`, so leaving all
unconfigured upstreams on legacy initialization makes native discovery an
expert-only feature.

**Recommended configuration shape**

- Add `mcp.upstream_protocol: auto`.
- Make `auto` the default after rmcp 3.3 interoperability tests pass.
- `auto` prefers `server/discover` at `2026-07-28` and falls back to a legacy
  `initialize` at `2025-11-25` using rmcp's current lifecycle implementation.
- Retain `latest` as a backwards-compatible alias for today's explicit legacy
  behavior, but document that the name means rmcp's legacy `LATEST`, currently
  `2025-11-25`.
- Retain explicit `"2026-07-28"` with its current prefer-modern/fallback
  behavior. It remains useful as a version-pinned spelling; `auto` is the
  forward-compatible default spelling.

**Files**

- `crates/bitrouter-sdk/src/config/mod.rs`
- `crates/bitrouter-sdk/src/mcp/mod.rs`
- `crates/bitrouter-sdk/src/mcp/rmcp_executor.rs`
- Config schema and tests
- Internal development docs that describe the setting

**Acceptance**

- Modern HTTP and stdio upstreams select `2026-07-28` without configuration.
- Legacy servers still connect through the tested fallback.
- A malformed modern server does not silently downgrade after a security or
  metadata failure. With pinned rmcp 3.3, fallback occurs after the discovery
  timeout or a complete, correlated non-modern JSON-RPC error; transport,
  correlation, malformed-response, version-negotiation, and modern rejection
  failures remain errors.
- Reload classification for this setting remains honest.

Implementation note: `auto` is now the serde/default-config value and maps to
rmcp's `ClientLifecycleMode::Auto` at `2026-07-28`; explicit `latest` continues
to map to rmcp `LATEST` (`2025-11-25`). The generated config schema and the
shippable BitRouter skill were updated with the default.

### Reviewer hardening — 2026-09-14

- [x] Register the same virtual-key `AuthHook` on both the language-model and
      MCP pipelines; an auth-enabled daemon rejects MCP before dialing an
      upstream.
- [x] Namespace member-originated `skill://` values in aggregate
      `resources/list` and `resources/templates/list`, so a path segment that
      equals another member label cannot misroute a later read.
- [x] Validate modern `Mcp-Param-*` headers against statically reachable
      primitive `x-mcp-header` annotations before `tools/call` dispatch,
      returning HTTP 400 / JSON-RPC `-32020` on mismatch.
- [x] Normalize every typed pagination page independently before merging cache
      hints; a page with missing policy contributes `ttlMs: 0` and
      `cacheScope: private` in either page order.
- [x] Age cache TTLs across pagination, aggregate fan-out, and both buffered
      and streaming cache hits; returned TTL is remaining freshness, never the
      upstream's or cache entry's original allowance.

### Phase 6 — external interoperability and claim gate

- [x] Run MCP Inspector `skills/list --verify` against the built local stdio
      server and record the version negotiated plus the result envelope.
- [ ] Run the official date-versioned MCP conformance suite against the origin
      HTTP server and aggregate HTTP endpoint for base-protocol behavior.
- [x] Add a Skills-specific wire suite because rmcp's core conformance score
      does not certify extension methods.
- [ ] Test at least one host currently listed as supporting Skills. Record
      whether it discovers, approves, verifies, and loads—or only imports a
      static snapshot.
- [ ] For ChatGPT plugin import, treat a public authenticated Streamable HTTP
      skill origin as a separate deliverable. The current local stdio server is
      not that transport.
- [x] Update `SKILLS_MCP_SPEC.md` from “complete” to a dated acceptance ledger
      that distinguishes local origin, direct relay, aggregate gateway,
      downstream lifecycle, and host interoperability.
- [ ] Update product wording only after every required row below is green.

## Acceptance ledger

| Requirement | Local stdio origin | Direct gateway | Aggregate HTTP gateway | Host/client |
|---|---:|---:|---:|---:|
| Advertises Resources + Skills | required | upstream-owned | required | observes declaration |
| `skills/list` complete/cacheable envelope | required | required/normalized | required/merged | consumes |
| `skills/get` complete/cacheable envelope | required | required/normalized | required/normalized | consumes |
| Manifest completeness, digest, size | required | preserved | preserved | verifies |
| Server identity + URI separation | one origin | one configured origin | reversible namespace | preserves both |
| `resources/read` content integrity | bytes match manifest | preserved | routed to one owner | verifies before use |
| MCP `2026-07-28` lifecycle | required | selected upstream | required downstream | client-dependent |
| Consent and activation | not owned | not owned | not owned | host-owned |

No “native Skills over MCP” claim ships while a required BitRouter-owned cell is
red. Client-dependent cells must be reported by named client and version rather
than generalized across the ecosystem.

### 2026-09-14 external run record

- MCP Inspector 2.6.0 used legacy `initialize` and negotiated `2025-11-25`.
  Its `skills/list --verify` run checked 3 skills and 29 files with zero
  conformance, frontmatter, size, or digest errors.
- Exact stdio wire tests separately assert the stable Skills envelopes for
  both a legacy-handshake partial client and a stateless `2026-07-28` client.
  Inspector 2.6.0's formatted `skills/list` output retains only its typed
  `skills` payload, so it is not envelope evidence.
- The official date-versioned core conformance suite and a named host's
  discovery/consent/activation journey remain pending; no host-side native
  activation claim is made from the Inspector result.

### 2026-09-14 local verification record

- `cargo nextest run --all-features`: 3,419 passed, 22 skipped after reviewer
  hardening.
- `cargo clippy --all-features --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps`:
  passed.
- `cargo run -p dist-helper -- check`: schema and registry distributions are
  current (47 providers, 52 canonical models, 1 runtime, 7 agents).
- `git diff --check`: passed.
- Cargo reports one dependency-only future-incompatibility notice for
  `proc-macro-error2 2.0.1`; it is not introduced by the MCP implementation.

## Review decisions

1. **Dependency floor:** approve raising `rmcp` from `3.0` to `3.3`, rather than
   updating only the lockfile. **Recommendation: approve** because this plan's
   tested baseline depends on the 3.2 lifecycle/HTTP fixes and the 3.3 release
   contains the current negotiation and transport hardening.
2. **Gateway normalization:** when an older upstream omits required cache
   fields, normalize to `ttlMs: 0` / `cacheScope: private` on both downstream
   gateway routes while leaving skill entries and file content untouched.
   **Recommendation: approve**; this is fail-closed and keeps provenance
   visible.
3. **Default upstream lifecycle:** add `auto` and make it the default after the
   rmcp 3.3 compatibility matrix passes. **Recommendation: approve**, with an
   explicit legacy setting retained.
4. **Installed skills over HTTP:** expose the user's local skills through a new
   authenticated HTTP product surface now, or keep them stdio-only.
   **Recommendation: keep stdio-only in this plan**; public exposure needs its
   own authentication, tenancy, CLI, and threat-model review.

## Merge gates

For each implementation PR:

```text
cargo nextest run --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo run -p dist-helper -- check
git diff --check
```

The final PR additionally records Inspector, official conformance-suite, and
named-client results. Local checks and external/client gates must be reported
separately.

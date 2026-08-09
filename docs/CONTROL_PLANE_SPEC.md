# Spec v2: One control plane — contract first, client later

Status: **proposed (v2) — open decisions resolved (§0)** · Author: Claude
(with Spikel) · Date: 2026-08-09 · Supersedes v1 (same path, unmerged)

> **Scoped thesis.** BitRouter has two concrete control-plane problems: the
> management wire types are hand-mirrored across two repositories, and the
> local daemon cannot be managed from anywhere but its own host. Each has a
> narrow fix. **This spec commits to those two fixes and to the crate rename
> that makes them legible.** It explicitly *defers* the unified CLI client
> that v1 built on top, because the shared surface it would abstract over is
> currently one verb wide.

## Changes from v1

v1 was reviewed and four of its factual premises were wrong. They are
corrected here, and the correction changed the shape of the plan.

| v1 claim | Reality | Consequence |
|---|---|---|
| `settlement.rs` is "daemon→gateway plumbing, never called by the CLI" (v1 §3.1) | `workflow-state reconcile-metering` builds a `SettlementClient` directly (`main.rs:2118-2132`, `metering/reconciliation.rs:6`) | Settlement does **not** move to `bitrouter-providers` (§5). And local usage rows can be **settled**, so v1 §9.1's report-level `authority` field was the wrong granularity (§9.1). |
| `crates/bitrouter-mcp/src/backend/` is "the proof-of-concept … and should be the model" (v1 §3.5) | `LocalBackend` is an HTTP client requiring a running daemon (`backend/local.rs:13-25`) — i.e. the daemon-required design v1 §2.1 dismissed in one sentence | The precedent claim is **withdrawn**. The alternative it actually demonstrates is costed in §12.2. |
| Phase 2's exposure story fits in a table row ("virtual key unless `skip_auth`") | Code-default listen is `0.0.0.0:4356` (`config/mod.rs:773`) and `bitrouter init` writes `skip_auth: true`; the SDK server has no TLS support at all | Exposure is now a **designed section with a default-deny mechanism** (§7.3), not a table cell. This is the single biggest change from v1. |
| Deleting the control socket "deletes the hand-rolled `#[cfg(windows)]` module" (v1 §7.1) while §11.3 recommended keeping it | Self-contradictory; and the wire format has documented back-compat affordances (`daemon.rs:57-64`, test at `daemon.rs:1025`) | The NDJSON control socket **stays** (§7.2). No control-protocol migration in this spec. |

Also corrected: v1's "Phase 0–1 are pure refactoring" was false for Phase 1 —
the trait's types *are* the wire types, so designing it presupposes the
semantic decisions v1 scheduled three phases later. v1's Phase 5 (schema
ownership) is therefore promoted to **Phase 1** here, and v1's Phases 1/3/4
become §8, deferred behind stated entry conditions.

## 0. Decisions taken

Resolved 2026-08-09 by Spikel. Each is folded into the body; this section is
the index.

| # | Decision | Lands in |
|---|---|---|
| D1 | **Rename the routing-lock surface to `bitrouter routing`**, freeing `policy` for access control. Deprecation alias for one minor version. | §4.1, §10 |
| D2 | **Clean break on `bitrouter-cloud-sdk`** — no final deprecated re-export release. We are at `1.0.0-alpha.27`. | §5.1 |
| D3 | **Management authorization defaults to deny, and server-side auth beyond `brvk_` is not an OSS concern.** OSS ships the minimum gate; scoped/tenanted authorization is a BitRouter Cloud and BitRouter Enterprise concern, supplied through the existing hook seam. | §7.3, §7.5 |
| D4 | **§12.1 (stop after Phase 1) is the accepted fallback** if Phase 2's exposure design does not survive review. | §12.1 |

D3 is not new policy — it restates a boundary already written into the code.
`apps/bitrouter/src/auth/mod.rs:1-6`: *"OSS auth — `brvk_` virtual-key
authentication… Not a shared library plugin: the OSS binary owns this
implementation end-to-end. A closed cloud product writes its own
`PreRequestHook` against its own auth model."* §7.5 makes that boundary
explicit for the management surface, where v2's first draft had been drifting
across it.

## Contents

- [0. Decisions taken](#0-decisions-taken)
- [1. The two problems](#1-the-two-problems)
- [2. Position and scope](#2-position-and-scope)
- [3. Verified starting state](#3-verified-starting-state)
- [4. How thin the shared surface really is](#4-how-thin-the-shared-surface-really-is)
- [5. Crate decomposition](#5-crate-decomposition)
- [6. Contract ownership and conformance vectors](#6-contract-ownership-and-conformance-vectors)
- [7. Daemon-served management](#7-daemon-served-management)
  - [7.5 The OSS / Cloud / Enterprise boundary](#75-the-oss--cloud--enterprise-boundary)
- [8. Deferred: the unified client](#8-deferred-the-unified-client)
- [9. Semantics that must not be flattened](#9-semantics-that-must-not-be-flattened)
- [10. Phases](#10-phases)
- [11. Open decisions](#11-open-decisions)
- [12. Non-goals, and alternatives costed](#12-non-goals-and-alternatives-costed)
- [13. Risk register](#13-risk-register)
- [14. Acceptance](#14-acceptance)

## 1. The two problems

Stated narrowly, because the fixes should be narrow.

**P1 — the management wire types are hand-mirrored across two repositories.**
`crates/bitrouter-cloud-sdk/src/management/types.rs:15` defines `PolicyKind`,
and its own doc comment says it "mirrors `bitrouter_cloud::policy::spec`."
`BudgetWindow` likewise (`types.rs:38-40`). Two repos, two release cadences,
one hand-copied enum: this is a drift bug that has not been filed yet.

**P2 — the local daemon is not manageable off-host.** The data plane already
works remotely: any harness can point `ANTHROPIC_BASE_URL` at any BitRouter.
The control plane cannot. `status`, `route`, `reload`, `stop`, and
`observe status` are reachable only over a mode-`0600` Unix socket on the same
machine (`daemon.rs:338-367`). A self-hosted BitRouter on a bastion, or a
daemon inside a sandbox container (#735, "one base_url seam"), is
unmanageable by the CLI that shipped it.

**A third, cosmetic problem** worth fixing while adjacent: `bitrouter-cloud-sdk`
is named after a *deployment*, not a *concern*. Two of its five modules are
client identity (usable against any self-hosted target) and one is settlement
reconciliation driven from the CLI. The name misdescribes the contents, which
is how v1 got its facts wrong in the first place.

## 2. Position and scope

**This spec commits to:**

1. **Contract ownership.** This repository owns the management wire types.
   The hosted gateway depends on the crate rather than mirroring it. Shared
   behavior is pinned by conformance vectors, not by types alone. (§6)
2. **Daemon-served management.** The local daemon serves the management
   surface over HTTP, behind an explicit, default-deny exposure model. (§7)
3. **Crate decomposition.** Split `bitrouter-cloud-sdk` on concern. (§5)

**This spec explicitly defers**, with entry conditions stated in §8:

- A unified `Management` trait with in-process and HTTP implementations.
- A `--target` flag on the CLI.
- Folding `bitrouter cloud …` into the top-level command tree.
- `/v1/capabilities` negotiation.

**Rationale for the deferral.** v1 justified the client abstraction by a
"middle band" of nouns served identically by both sides. Measured honestly
(§4), that band is **one verb wide today** (`models`), with `policies` a
plausible second *after* the §9.4 structure decision lands (the naming half
is resolved — D1, §4.1).
An abstraction over a one-verb intersection is not yet earning its cost, and
building it first would force the semantic decisions to be made implicitly, in
a trait signature, rather than explicitly.

The ordering principle: **agree on the contract, prove it with tests, then
consider abstracting over it.** Not the reverse.

### 2.1 Non-position

**Not "always HTTP."** `key sign`, `policy create`, and `trajectory inspect`
work today with no daemon running. §7 adds an HTTP path; it does not remove
the in-process one. The daemon-required alternative is genuinely attractive
and is costed in §12.2 rather than dismissed — it loses on first-run
bootstrap, not on principle.

**Not a control-protocol migration.** The NDJSON control socket stays (§7.2).

## 3. Verified starting state

Measured on `main` at `bbca8eca`.

### 3.1 `crates/bitrouter-cloud-sdk` — 5,919 lines

| Module | Lines | What it is | Called by |
|---|---:|---|---|
| `auth/` | 2,159 | Client identity: RFC 8628 device flow, RFC 6749 §6 refresh, RFC 7009 revocation, `0600` credential store | CLI; also `acp_cli.rs:271` |
| `management/` | 2,208 | Typed namespace-scoped HTTP client | CLI (`cloud/cli.rs`) |
| `api.rs` | 458 | Origin-confined raw HTTP escape hatch | `bitrouter cloud api` |
| `provider/` | 755 | `AuthApplier` for the `bitrouter:` provider | **daemon routing path only** |
| `settlement.rs` | 287 | Request-scoped settlement receipts | **daemon *and* CLI** — `main.rs:2118`, `metering/reconciliation.rs:6` |
| `lib.rs` | 52 | | |

Downstream: `crates/bitrouter-mcp` depends on the crate for one wire type
(`backend/cloud.rs:6`, `BalanceResponse`). The crate carries no
`publish = false` and inherits `version.workspace = true` (`1.0.0-alpha.27`),
so it is a published artifact.

`apps/bitrouter/src/cloud/` adds 3,263 lines of CLI surface.

### 3.2 Cloud management endpoints

Namespace-scoped under `/v1/namespaces/{nsid}`, resolved from the credential
and falling back to the literal `me` for API-key credentials
(`management/mod.rs:199-206`):

```
/keys  /keys/{id}
/usage  /requests
/policies  /policies/{id}  /policies/{id}/{bind,enable,disable,bindings}
/policies/{id}/bind/{binding_id}  /policies/effective
/budgets  /budgets/{id}      # typed sugar over /policies
/presets  /presets/{id}      # typed sugar over /policies
/oauth/clients
```

User-level: `/v1/namespaces`, `/v1/billing/balance`,
`/v1/billing/checkout/sessions`, `/v1/byok/keys[/{provider}]`.

### 3.3 The local daemon's two surfaces

**Control** — `daemon.rs`: NDJSON over a `0600` Unix socket or a Windows named
pipe, one command per connection. `Stop` · `Reload { env }` · `Status` ·
`Route { model }` · `ObserveStatus`. Consumers include `stop`, `restart`,
`reload`, `status`, `route`, `observe status`, `policy reload`,
`policy publish`, `spawn.rs:633`, `optimization/runner.rs:385`,
`routing_preview.rs:94`.

**HTTP** — `bitrouter-sdk/src/server.rs:269-282`: `/v1/messages`,
`/v1/chat/completions`, `/v1/responses`, `/v1beta/models/{action}`,
`/v1/models`, `/mcp/{server}`, optional aggregate MCP route, `/metrics`,
`/health`. **No management routes. No TLS.**

### 3.4 Facts that constrain the design

- Code-default listen is **`0.0.0.0:4356`** (`config/mod.rs:773`). Code-default
  `skip_auth` is `false`, but **`bitrouter init` writes `true`**
  (`config/mod.rs:765-767`).
- `db::connect` sets neither WAL mode nor `busy_timeout`
  (`db/mod.rs:34-43`). Concurrent writers are not configured for.
- `key_sign` runs migrations unconditionally (`commands.rs:154-161`).
- Metering rows already carry per-row provenance: `reconciliation_status`
  (`metering/db.rs:23`) and `authoritative_receipt`
  (`metering/store.rs:101-105`).
- `reconcile_authoritative_requests` writes settled charges into local rows
  (`metering/reconciliation.rs:57`).
- axum 0.8 implements `Listener` for `tokio::net::UnixListener`
  (`axum-0.8.9/src/serve/listener.rs:46`, `#[cfg(unix)]`). **No such impl for
  Windows named pipes.**
- A remote-target spelling already exists: `--base-url` on `launch`/`spawn`
  (`acp_cli.rs:66`, `main.rs:406`), with its own local-vs-remote credential
  logic (`acp_cli.rs:241-290`).

## 4. How thin the shared surface really is

| Noun | Local today | Cloud today | Shared surface? |
|---|---|---|---|
| `models` | `GET /v1/models` | `GET /v1/models` | **yes — the only one** |
| `policies` (access control) | flat `Policy` struct in `./policies/*.yaml` (`policy/policy.rs:21`) | typed registry: kinds + bindings + `effective` | **candidate**, blocked on §9.4 |
| `route` | control-socket `Route` | — | local-only; cloud endpoint does not exist |
| `status` | control-socket `Status` | `/billing/balance` | verb only; payloads share nothing |
| `observe` | control-socket `ObserveStatus` | — | local-only (per-process exporter) |
| `keys` | `brvk_` DB rows, **user-scoped**, "v1 does not sign a JWT" (`main.rs:977`) | `brk_`, **namespace-scoped**, no user param | name-only (§9.2) |
| `usage` | metering DB, mixed estimated/settled rows | settlement-backed | name-only (§9.1) |
| `budgets` | a field inside a policy | policy kind over a credit balance | name-only (§9.3) |
| `byok` | *is* the local model (`providers:` in YAML) | uploads keys to the gateway | inverted |
| `billing`, `namespaces`, `oauth clients` | — | yes | cloud-only |
| `policy lock` (`compile`/`diff`/`publish`/`evolve`/`verify`) | the self-improving routing loop | — | local-only; **name collision** (§4.1) |
| `trajectory`, `workflow-state`, `optimize`, `eval`, `skills`, `tools`, `agents` | local | — | local-only |

**One verb.** That number is the reason §8 defers the client.

### 4.1 Resolved (D1): `policy` meant two different things

`bitrouter policy` today is overwhelmingly the **routing policy lock** —
`init`, `check`, `verify --evidence`, `compile`, `diff`, `publish`, `evolve`,
`status`, `reload` (`main.rs:993-1224`). Deterministic artifacts, evidence
roots, lineage validation: the self-improving routing loop.

`bitrouter cloud policy` is a **typed access-control registry** — CRUD over
`Budget`/`RateLimit`/`Guardrail`/`Preset`, plus `bind`, `effective`,
`for-principal`.

They share a word and nothing else. Local *also* has `policy create`
(`main.rs:995`), which writes an access-control file and **is** the cloud
concept.

This blocked §6's contract work, not merely a future CLI merge: the contract
cannot name a resource `policy` while the CLI means something else by it.

**D1 — the routing lock moves to `bitrouter routing`.**

```
bitrouter policy {init,check,verify,compile,diff,publish,evolve,status,reload}
  →  bitrouter routing {init,check,verify,compile,diff,publish,evolve,status,reload}

bitrouter policy create        →  bitrouter policy create   (unchanged — this
                                   was always the access-control concept)
```

The routing lock is the more specific concept and takes the more specific
word; `policy` as access control is the industry default and what the hosted
gateway already ships. `bitrouter policy <lock-verb>` remains as a hidden
deprecation alias for one minor version, emitting a notice to stderr and
resolving to the `routing` implementation — no duplicated logic.

Consequences to carry through the change: `docs/CLI.md`, `skills/bitrouter/`
(the skill must never describe a CLI that does not exist — `CLAUDE.md`), the
`policy reload` / `policy publish` control-socket callers
(`main.rs:1224`, `policy_lock.rs`), and any `bitrouter.yaml` prose referencing
the verbs. The `policies:` config key and the `./policies` directory are
**unchanged** — they are the access-control concept and keep the name.

## 5. Crate decomposition

Split on concern. **Corrected from v1**: settlement stays with its callers.

```
crates/bitrouter-cloud-sdk/              →  (deleted)
  auth/                     2,159 lines  →  crates/bitrouter-auth/
  management/ + api.rs      2,666 lines  →  crates/bitrouter-management/
  settlement.rs               287 lines  →  crates/bitrouter-management/settlement
  provider/                   755 lines  →  crates/bitrouter-providers/
  lib.rs                       52 lines  →  (absorbed)
```

**`bitrouter-auth`** — **client-side** identity for any target: obtaining,
storing, and refreshing a credential this machine presents to somebody else.
Nothing in it is cloud-specific: `--oauth-as <URL>` already accepts a
self-hosted authorization server, and `acp_cli.rs:271` already resolves a
bearer per base URL. The rename is most of the point.

Note the boundary this crate must not cross (D3, §7.5): it is a credential
*client*. It contains no authorization server, no scope evaluation, no
principal model. Those are Cloud/Enterprise concerns and stay out of this
workspace.

**`bitrouter-management`** — the wire types (§6) plus the HTTP client, plus
settlement. Settlement lives here because it is a *client of a hosted
BitRouter's authoritative receipts*, which is exactly what this crate is —
and because moving it to `providers` would break its CLI caller.

**`bitrouter-providers`** — gains only `BitrouterCloudAuthApplier`, which is
genuinely a provider auth applier on the routing path.

### 5.1 Phase 0 is not free — what it costs

v1 called this zero-risk. Two corrections:

- `crates/bitrouter-mcp` imports `bitrouter_cloud_sdk::management::billing::BalanceResponse`
  (`backend/cloud.rs:6`). Mechanical, but it must move in the same change.
- The crate is published (no `publish = false`). Deleting the name is a
  breaking change for any external consumer. **D2: clean break** — no final
  deprecated re-export release. At `1.0.0-alpha.27` the pre-1.0 contract
  covers it; the changelog states the three replacement crates and the
  import-path mapping, and that is the whole migration.

## 6. Contract ownership and conformance vectors

This is P1's fix, and in v2 it comes **before** anything else structural.

### 6.1 Type ownership

`bitrouter-management` exports the management wire types. The hosted gateway
depends on the crate instead of mirroring it. Precedent exists — cloud already
consumes this repo's registry artifacts (#645).

Immediate deletions: the hand-mirrored `PolicyKind` and `BudgetWindow`
(`management/types.rs:15,38`) stop being mirrors and become the definition.

### 6.2 Conformance vectors — the part types cannot cover

Shared types prevent *shape* drift. They do not prevent *behavioral* drift,
and the highest-value behavior here is already written down on exactly one
side:

`policy/policy.rs:1-13` documents the combination semantics for overlapping
policies — model deny is union, model allow is **intersection**, spend and
rate limits take the **minimum**, expiry takes the **earliest**, tool access is
union. It is implemented and tested locally (`policy/policy.rs:233-357`) and
exists nowhere on the cloud side.

**Requirement.** A `conformance/` directory in `bitrouter-management` holding
input/expected-output vectors for every shared verb, starting with
`policies/effective` derived from that table. Both servers run them in CI.
A verb without vectors is not in the shared contract.

This is the mechanism that makes "two servers of one spec" mean something. It
lands before any route is mounted.

### 6.3 Contract versioning

Two repositories, two release cadences. The contract carries a semver string,
and both servers expose it. Compatibility rule: a client refuses a server whose
major differs; a server accepts a client at or below its own minor.

This is a designed mechanism, not an open question — v1 left it as an open
decision, which was the wrong weight for the hardest operational problem in
the design. What remains open is who arbitrates bumps (§11.5).

## 7. Daemon-served management

P2's fix. Scoped: **add** an HTTP management surface; **change** no existing
protocol.

### 7.1 Paths

The daemon mounts the same paths the hosted gateway serves, pinning the
namespace segment to `me`:

```
GET    /v1/namespaces/me/keys          POST /v1/namespaces/me/keys
DELETE /v1/namespaces/me/keys/{id}
GET    /v1/namespaces/me/usage         GET  /v1/namespaces/me/requests
GET    /v1/namespaces/me/policies …
```

Only verbs with conformance vectors (§6.2) are mounted. Everything else waits.

### 7.2 The control socket stays

`/control/*` is **not** part of this spec. The NDJSON control socket keeps
serving `Stop`, `Reload`, `Status`, `Route`, `ObserveStatus` unchanged.

Reasons, all of which v1 got wrong:

- The daemon is precisely the component that stays running across CLI
  upgrades. Replacing its protocol strands every mixed-version pair, including
  `spawn`'s preflight and the optimize runner.
- The wire format has deliberate back-compat affordances already
  (`daemon.rs:57-64` and its test at `daemon.rs:1025`). That is a signal about
  how this protocol is expected to evolve.
- Windows has no axum `Listener` for named pipes. Keeping the socket makes the
  Windows question disappear instead of deferring it into a contradiction.

If `/control/*` over HTTP is ever wanted, it is a separate spec with a
mixed-version story.

### 7.3 Exposure — designed, not tabled

**This is the section v1 got most wrong.** The naive reading of "mount
management on the existing router" is a security regression, because:

- code-default listen is `0.0.0.0:4356` (`config/mod.rs:773`);
- `bitrouter init` writes `skip_auth: true` (`config/mod.rs:765-767`);
- the SDK server has no TLS support of any kind.

Under those three facts, mounting management on the data-plane router would
move key minting from UID-gated (a `0600` socket, DB file permissions) to
reachable by any process on the host — and, for anyone who widened `listen`
for LAN harness access, off-host. Silently, on upgrade.

**Mechanism: management is off by default and never rides the data-plane
listener's trust.**

1. **Separate opt-in.** A new `server.management` block. Absent ⇒ **no
   management routes are mounted at all**. Upgrading changes nothing.
2. **Separate listener.** `server.management.listen` defaults to
   `127.0.0.1:4357` — never inherits `server.listen`. Widening the data plane
   for harnesses cannot widen management.
3. **`skip_auth` does not apply, and the gate is deliberately dumb (D3).**
   Management authentication is independent and mandatory: a management
   request without a valid credential is refused even when the data plane is
   open under `skip_auth: true`. What OSS ships is a **single management
   secret** — generated on first `server.management` enablement, stored mode
   `0600` beside the config, presented as a bearer. Possession is the whole
   check. No scopes, no principals, no roles, no delegation. A `brvk_`
   inference key is **never** accepted on a management route (§11.7): the
   escalation from "can spend" to "can mint keys" must not exist. Anything
   richer is Cloud/Enterprise (§7.5).
4. **Non-loopback requires TLS, and TLS does not exist yet.** Binding
   management to a non-loopback address is **refused at config-validation
   time** with a message pointing at the tunnel pattern (SSH / WireGuard /
   service mesh) until TLS is specified and built. Off-host management is
   therefore delivered as "loopback + your own tunnel" in this spec, which is
   honest and immediately useful for the bastion and #735 cases.
5. **`Stop` and `Reload` are not reachable over HTTP at all** — they remain
   control-socket-only (§7.2). `Reload { env }` in particular ships API keys
   in cleartext from the CLI's environment (`daemon.rs:57-64`); it must never
   acquire a network path.

TLS termination in-process is a follow-up spec, not a table cell.

### 7.4 Handlers and consistency

Handlers read and write the same stores the CLI writes in-process. That
concurrency is new and must be configured for — v1 hand-waved it.

- **SQLite is not configured for concurrent writers.** `db::connect`
  (`db/mod.rs:34-43`) sets no `journal_mode=WAL` and no `busy_timeout`.
  Phase 2 must set both, or CLI-writes-during-daemon-serve will surface as
  `SQLITE_BUSY`.
- **Migration skew is a real failure mode.** `key_sign` runs migrations
  unconditionally (`commands.rs:154-161`), so a newer CLI can migrate the
  schema underneath an older running daemon. The daemon must record the
  schema version it assembled against and **refuse to serve management** — with
  a "restart the daemon" message — when the DB has moved past it.
- **Cache coherence is narrower than v1 assumed.** Keys need no invalidation:
  the auth hook does a per-request DB lookup (`auth/hook.rs:124-125`), so
  there is no cache. Policies are file-loaded into memory
  (`policy/store.rs:59-67`) and already have a reload verb. Only the policy
  store needs an invalidation path, and it exists.

### 7.5 The OSS / Cloud / Enterprise boundary

**D3.** Server-side authorization is not an OSS concern. This is not a new
position — `apps/bitrouter/src/auth/mod.rs:1-6` already states it: OSS owns
`brvk_` virtual-key authentication end-to-end, and *"a closed cloud product
writes its own `PreRequestHook` against its own auth model."* §7.3 applies the
same rule to the management surface, where v2's first draft had begun drifting
across it by reaching for scopes.

| Concern | Tier | Mechanism |
|---|---|---|
| Presenting a credential to a remote BitRouter | **OSS** | `bitrouter-auth` (§5) — client-side only |
| `brvk_` virtual keys for the data plane | **OSS** | `apps/bitrouter/src/auth/`, unchanged by this spec |
| Management gate: one secret, possession-checked | **OSS** | §7.3 mechanism 3 |
| Scopes, roles, principals, delegation | **Cloud / Enterprise** | own `PreRequestHook` |
| Namespaces / tenancy | **Cloud / Enterprise** | §11.2 — OSS pins `me` |
| Authorization server (issuing tokens, `/oauth/clients`) | **Cloud / Enterprise** | not in this workspace |
| Audit trail of management mutations | **Cloud / Enterprise** | OSS logs; it does not retain an audit store |

Three design consequences, and they are the reason to write this down rather
than leave it implicit:

1. **The management surface must be authorization-model-agnostic.** Handlers
   authenticate through a seam — the same shape as the existing
   `PreRequestHook` — not through an inline check. OSS supplies the
   possession-check implementation; Cloud/Enterprise substitute theirs
   without forking the handlers. If a handler ever reads a scope directly,
   the boundary has been violated.
2. **The contract (§6) carries no authorization vocabulary.** No `scope`,
   `role`, or `principal` fields in shared wire types. Where the hosted
   gateway needs them, they are *its* request-envelope concern, above the
   contract. This is what keeps a Cloud-only concept from leaking into the
   OSS daemon as dead weight — `CLAUDE.md` forbids types that no OSS feature
   uses, and a scope enum nothing in OSS evaluates would be exactly that.
3. **§8's deferred client inherits the boundary.** Any future `Management`
   trait is a client of an already-authenticated channel. It does not model
   authorization, and its error taxonomy treats `Forbidden` as opaque —
   which incidentally removes `management::Error::Forbidden { missing_scope }`
   from the list of things a unified error type must reconcile.

**Falsifiable test of the boundary:** grep the OSS workspace for `scope`,
`role`, `principal`, or `tenant` in management code paths. A hit is a
boundary violation, not a feature.

## 8. Deferred: the unified client

v1's `Management` trait, `--target` flag, capability negotiation, and
`cloud/cli.rs` deletion are **not** in this spec. They are a reasonable
destination; they are premature while §4 measures one shared verb.

**Entry conditions.** Revisit when all of these hold:

1. D1's rename has shipped (Phase 0.5) — done.
2. §9.4's local/cloud policy structure is reconciled, and
   `policies/effective` has passing conformance vectors on both servers.
3. At least three verbs are in the shared contract with vectors — enough that
   an abstraction has something to abstract.
4. §9.1's per-row authority and §9.2's key union are settled **as wire
   types**, not as trait signatures.

**Design debts to carry forward** — each surfaced in review of v1 and none
solved here:

- **Error taxonomy.** `management::Error` is HTTP-status-shaped
  (`NotSignedIn`, `Forbidden { missing_scope }`, `Conflict`); the local stores
  produce `BitrouterError`, DB, and IO errors. `NotSignedIn` is meaningless
  in-process; `SQLITE_BUSY` is meaningless over HTTP. Unifying these is a
  substantial fraction of the trait's cost.
- **Pagination.** `/requests` over any real window needs cursors. A trait
  returning `Vec<_>` forecloses them; design pagination into the *contract*
  (§6) now, so a later trait inherits it.
- **Idempotency.** `create_key` retried after a timeout mints two keys.
  Idempotency keys belong in the contract, not the client.
- **Request-side divergence.** Local key creation takes a `user_id`
  (`commands.rs:150`; keys are rows owned by local `users`, `auth/db.rs:20-37`);
  cloud minting is namespace-scoped with no user parameter. A shared
  `CreateKey` has to answer this, and v1's trait sketch omitted exactly the
  field where it strains.
- **Trait-object mechanics.** `Arc<dyn Management>` cannot be queried for a
  cloud-only extension trait — Rust has no cross-trait downcast — so the CLI
  would hold a second optional handle constructed by a branch on target. v1's
  "never branches on target again" was not achievable as written.
- **`--target local` has no HTTP surface**, so v1's generalized
  `bitrouter api --target <t>` is undefined for the default target.
- **A competing spelling already ships.** `--base-url` on `launch`/`spawn`
  (`acp_cli.rs:66`) with its own credential resolution. Any future `--target`
  must subsume or align with it, not sit beside it.

## 9. Semantics that must not be flattened

The failure mode is cosmetic convergence: two things sharing a noun, merged
because the CLI looks tidier. These constrain §6's contract even though §8 is
deferred.

### 9.1 `usage` — authority is per row, not per report

v1 put an `authority: local_estimate | settled` field on the report. That is
wrong, because reconciliation exists: `reconcile_authoritative_requests`
(`metering/reconciliation.rs:57`) writes settled charges into **local** rows.
A local report can legitimately hold a mix, and no report-level label is
honest about it.

The row already carries what is needed — `reconciliation_status`
(`metering/db.rs:23`: `Pending`/`Computed`/`NotCharged`/`Unknown`) and
`authoritative_receipt` (`store.rs:101-105`). **The fix is to surface an
existing field, not invent one.**

Contract requirement: every usage row carries its provenance; aggregates carry
both a settled total and an estimated total rather than one blended number.
Human output shows the estimated portion with a visible qualifier. A number
that looks like a bill and is not one is a correctness bug.

### 9.2 `keys` — different security models *and* different request shapes

Local `brvk_` are DB rows with a hashed secret, owned by a local `users` row,
and `key sign`'s own help says "v1 does not sign a JWT" (`main.rs:977`). Cloud
`brk_` are namespace-scoped gateway credentials with no user parameter. The
contract carries a `kind`, and the request shapes are reconciled explicitly
before any shared `create` verb is mounted — this is an entry condition in §8,
not an implementation detail.

### 9.3 `budgets` — different enforcement points

A cloud budget gates a credit balance. A local BYOK daemon has no balance; a
budget there means "stop routing at $N of *estimated* spend," which §9.1 says
is a different quantity. Budgets stay cloud-only until a local enforcement
point exists.

### 9.4 `policies` — same information, different structure

Local `Policy` (`policy/policy.rs:21`) is one flat struct carrying every
dimension — `allowed_models`, `denied_models`, `max_spend_micro_usd`,
`expires_at`, `allowed_tools`, `max_requests_per_minute` — with documented
combination semantics. Cloud is a discriminated union of
`Budget`/`RateLimit`/`Guardrail`/`Preset` with principal bindings.

The information is genuinely the same: the flat struct maps onto
`Guardrail` + `Budget` + `RateLimit`, and `Preset` is cloud's bundle. Converging
means the local file format grows a `kind` — a **format migration for existing
users**, which needs a compatibility window and is why this is a blocking
decision rather than a mapping exercise.

The combination table is the crown jewel and the reason §6.2 exists.

## 10. Phases

**Phase 0 — decompose the crate.** §5, with §5.1's costs acknowledged. Three
crates; settlement stays with `bitrouter-management`; `bitrouter-mcp`'s import
moves in the same change; the crate name is deleted outright (D2).
*Exit: workspace builds, all tests pass, no crate is named after a deployment.*

**Phase 0.5 — the `routing` rename (D1).** §4.1. Mechanical and independently
shippable; it gates Phase 1 because the contract names resources. Ships with
the hidden `policy` alias, `docs/CLI.md`, and `skills/bitrouter/` in the same
change.
*Exit: `bitrouter routing …` is the documented surface; `bitrouter policy
<lock-verb>` warns and still works; the skill describes the CLI that exists.*

**Phase 1 — the contract.** §6. Types move to `bitrouter-management` and stop
being mirrors; `conformance/` lands with vectors for `models` and
`policies/effective` derived from `policy/policy.rs:1-13`; the contract version
mechanism (§6.3) ships. No authorization vocabulary enters the contract
(§7.5.2).
*Exit: hosted gateway depends on the crate; no "mirrors `bitrouter_cloud::…`"
comments remain; both servers run the vectors in CI.*

**Phase 2 — daemon-served management.** §7. `server.management` opt-in block,
separate listener, mandatory possession-checked secret behind an
authorization seam (§7.5.1), non-loopback refused at validation, WAL +
`busy_timeout`, schema-skew refusal. Only vector-backed verbs mounted. Control
socket untouched.
*Exit: a `curl` against an opted-in daemon returns contract-conformant
payloads; a default-config daemon serves no management routes at all; the
bastion / #735 case works over a tunnel.*

**Later — §8**, behind its stated entry conditions.

Phase 0 is worth doing regardless of the rest. Phase 1 is worth doing even if
Phase 2 never ships — it fixes P1 by itself.

### 10.1 Ordering dependencies (explicit)

```
D1 rename (Phase 0.5) ──▶ Phase 1 (contract names resources)
§9.4 policy structure ──▶ Phase 1 vectors for policies/effective
Phase 1 ──▶ Phase 2 (only vector-backed verbs mount)
D3 boundary (§7.5) ──▶ Phase 1 (no authz vocabulary in the contract)
                   └──▶ Phase 2 (handlers authenticate through a seam)
Phase 1 + §9.1 + §9.2 ──▶ §8 (trait types are contract types)
```

v1 had these dependencies inverted, which is why its "pure refactoring" claim
for Phase 1 was false.

## 11. Open decisions

**11.1 — the `policy` name collision. RESOLVED (D1)** — rename the routing
lock to `bitrouter routing`, hidden `policy` alias for one minor version.
Detail in §4.1; sequencing in Phase 0.5.

**11.2 — does the local daemon grow namespaces? RESOLVED by D3** — no. Pin
`me`, keep single-tenancy. Tenancy is a Cloud/Enterprise concern (§7.5).

**11.3 — `policies` format migration window (§9.4).** How long does the local
flat format keep working after `kind` is introduced? Proposal: accept both for
two minor versions, warn on the old shape from the first.

**11.4 — does cloud implement `route`?** A routing preview against the hosted
table is useful and the endpoint does not exist. Proposal: specify it in the
contract as optional, ship local-only first.

**11.5 — who arbitrates contract version bumps** across the two repos, and
what is the compatibility window in releases (not versions)?

**11.6 — published-crate deletion. RESOLVED (D2)** — clean break at alpha, no
deprecated re-export release. §5.1.

**11.7 — management authentication credential. RESOLVED (D3)** — a `brvk_`
inference key is never accepted on a management route; management uses a
separate possession-checked secret, default deny. The originally-proposed
"management scope" is **rejected** as a boundary violation: a scope enum
implies a scope model, and OSS does not own one (§7.5). Cloud/Enterprise
supply scoped authorization through the seam.

### 11.8 — still open

**11.8 — where does the management secret live?** Beside the config as its own
`0600` file, or a `server.management.token_file` pointer, or an env var for
container deployments (#735 will want this)? Leaning: generated file by
default, `token_file` override, env var read if set — decide during Phase 2.

## 12. Non-goals, and alternatives costed

**Non-goals.** Location transparency for the whole CLI (trajectory,
worktrees, PTYs, stdio MCP, skills and agent catalogs stay local, and this
spec claims nothing about them); a web console; merging the data plane
(already converged); shrinking OAuth (it moves and is renamed, not reduced);
a remote TUI (per #745–749 the TUI is being dissolved and BitRouter rescoped
to router-not-orchestrator — it justifies nothing here); making cloud
implement the daemon's control protocol.

Alternatives that v1 failed to consider or dismissed too fast:

**12.1 — schema crate only (Phase 0 + 0.5 + 1, stop). ACCEPTED AS FALLBACK
(D4).** Fixes P1 at roughly a tenth of the total cost and leaves P2
unaddressed. If Phase 2's exposure design (§7.3) does not survive review, we
stop here rather than shipping a weakened version of it — a management surface
with a compromised exposure story is worse than no management surface. Phases
0, 0.5, and 1 each stand alone, so this fallback costs nothing already spent.

**12.2 — daemon-required management.** Drop the in-process path entirely; every
management command is an HTTP call to a daemon, auto-started if absent (the
`docker`/`systemctl` model). Genuinely attractive: it makes "one code path"
literally true, it is what `crates/bitrouter-mcp/src/backend/local.rs:13-25`
already chose, and it deletes §7.4's concurrency problem outright — only the
daemon touches the DB.
**Rejected on first-run bootstrap.** `key sign` and `policy create` are partly
bootstrap operations, and requiring a running daemon to configure the thing
that is not yet configured is a worse first-run story than
`bitrouter init` currently delivers. It also makes every management command
fail on a host where the daemon cannot bind. Worth revisiting if §7.4's
concurrency work proves expensive — the trade is real and this rejection is a
judgment call, not a finding.

**12.3 — shared server-handler crate.** Both servers import one set of axum
handlers over a `Store` trait, making behavioral drift structurally impossible
rather than test-enforced. Stronger than §6 on correctness; requires the
hosted gateway to accept this repo's handler implementation and request
extractors, which is a much larger coupling between a public repo and a
private service than type-sharing. **Deferred, not rejected** — if §6.2's
vectors prove hard to keep green, this is the escalation.

## 13. Risk register

**13.1 — behavioral drift between two servers.** *Mitigation:* §6.2 vectors in
both CIs, §6.3 versioning. *Escalation:* §12.3. *Residual:* verbs without
vectors are not in the contract, so drift there is unbounded — hence §7.1's
"only vector-backed verbs mount."

**13.2 — management exposure regression.** The v1 design would have widened
key minting to every local process by default. *Mitigation:* §7.3's five
mechanisms, default-off. *Test:* a default-config daemon must 404 every
management path; a non-loopback `management.listen` must fail config
validation.

**13.3 — concurrent-writer and migration-skew failures.** *Mitigation:* §7.4 —
WAL, `busy_timeout`, schema-version refusal. *Test:* CLI write against a
serving daemon under contention; older daemon + newer-migrated DB refuses
management with a restart message.

**13.4 — privilege escalation via inference keys.** *Mitigation (D3):*
`brvk_` keys are never accepted on management routes; management uses a
separate secret, default deny. *Test:* a valid `brvk_` key must be rejected on
every management path.

**13.9 — authorization vocabulary leaks into OSS.** A scope/role/principal
field added "just for cloud" becomes dead weight OSS must maintain and
`CLAUDE.md` forbids. *Mitigation:* §7.5's grep test in CI.

**13.5 — `policies` format migration breaks existing users.** §9.4, §11.3.
*Mitigation:* dual-accept window with warnings.

**13.6 — published-crate deletion breaks an external consumer.** Accepted
risk (D2): clean break at alpha, mapping in the changelog. §5.1.

**13.7 — offline management regression.** *Mitigation:* the in-process path is
untouched by this spec. *Test:* every local management command passes with the
daemon stopped.

**13.8 — scope creep back toward v1.** *Mitigation:* §8's entry conditions are
falsifiable; if the shared contract is still one verb wide, the client does
not get built.

## 14. Acceptance

1. No crate is named after a deployment. `bitrouter-cloud-sdk` is gone with no
   deprecated re-export (D2); `bitrouter-auth` and `bitrouter-management`
   exist; settlement is with its callers; `bitrouter-mcp` builds; the
   changelog carries the import-path mapping.
2. `bitrouter routing …` is the routing-lock surface (D1); `bitrouter policy
   <lock-verb>` warns and resolves to it with no duplicated logic; the
   `policies:` config key and `./policies` directory are unchanged.
3. The management wire types are defined once, in this repository, and the
   hosted gateway depends on them. No "mirrors `bitrouter_cloud::…`" comment
   remains.
4. `conformance/` holds vectors for every verb in the shared contract,
   including `policies/effective` derived from `policy/policy.rs:1-13`, and
   both servers run them in CI.
5. The contract carries no authorization vocabulary (§7.5.2).
6. A daemon with no `server.management` block serves **404 on every management
   path**. Upgrading an existing install changes no exposure.
7. Management binds its own listener, requires authentication regardless of
   `skip_auth`, and refuses a non-loopback bind at config-validation time.
8. A valid `brvk_` inference key is **rejected** on every management path
   (D3). Management handlers authenticate through a substitutable seam, and a
   grep for `scope` / `role` / `principal` / `tenant` in OSS management code
   returns nothing (§7.5).
9. `Stop` and `Reload` have no network path. The NDJSON control socket is
   byte-compatible with the pre-change protocol, and its back-compat test
   (`daemon.rs:1025`) still passes.
10. SQLite runs in WAL with a `busy_timeout`; a schema-skewed daemon refuses
    management with an actionable message.
11. Every local management command passes with the daemon stopped.
12. Usage rows carry per-row provenance; aggregates report settled and
    estimated separately; the human renderer visibly qualifies the estimated
    portion.
13. `docs/CLI.md` and `skills/bitrouter/` are updated in the same change as
    any flag, port, listen-address, or subcommand alteration (per `CLAUDE.md`)
    — covering both the `routing` rename and the new `server.management`
    block. The agent-plugin manifests (`.claude-plugin/`, `.codex-plugin/`,
    `.agents/plugins/marketplace.json`) are checked for any reference to a
    renamed verb.
14. `cargo nextest run --all-features`, `cargo clippy --all-features`, and
    `cargo fmt -- --check` are clean.

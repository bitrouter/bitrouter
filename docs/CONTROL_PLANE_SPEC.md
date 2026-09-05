# Spec: the control plane — the daemon as owner of config, policy, keys, and history

Status: **proposed; the seven §18 decisions are taken — single-tenant OSS
ratified, editions line in §22** · Author: Claude (with Spikel) ·
Date: 2026-09-04

This document specifies how the BitRouter daemon becomes the owner of
configuration, policy, keys, and history, and how one action surface is then
served to a local CLI and to a remote one over HTTP.

It exists because BitRouter today has **no action surface at all** — it has
four disjoint ones, and most of the CLI uses none of them. Of 103 distinct leaf
subcommands, 66 reach something that exists only on the machine the command
runs on. The CLI and the daemon are not client and server; they are two peers
coupled through a shared filesystem, both writing the same YAML, the same lock
file, and the same SQLite database. That works on one machine and is exactly
what "operate a proxy remotely" destroys.

It is **not** a proposal to make the daemon a general management server. §5
draws the line that stops it becoming one, §22 draws the line between the
Apache-2.0 build and the closed enterprise product, and §21 states the
strongest case for not doing any of this at all.

Companion reading: [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) §11
(`_bitrouter/route/*`, the one control surface that already exists) and
[`DEVELOPMENT.md`](DEVELOPMENT.md) (the workspace boundaries this must not
break).

---

## 1. Executive decision

**Custodied source, served state.** The daemon becomes the single online
authority over all four domains, but it *authors* only three of them.

- **Policy, keys, history** become **daemon-authored**: the daemon generates
  their durable bytes and is the sole online writer.
- **Config** becomes **daemon-custodied**: the daemon stores, validates,
  versions, and compare-and-swaps bytes it never generates. Config writes are
  byte passthrough under `If-Match`. There is no `Serialize` for `Config`,
  now or ever.

`bitrouter.yaml` and `policy-lock.yaml` stay authoritative on disk, in their
exact current formats, at the paths the existing resolution chain finds.
Nothing moves on disk in any phase. Zero-config still writes nothing.

Two rules govern, and they answer different questions. §4 (the *mechanism*
rule) decides **how** the daemon may own a domain. §5 (the *scope* rule)
decides **what** it may own at all.

---

## 2. Why this is the boundary

Three facts, each verified in the tree, force the shape:

1. **`Config` cannot be serialized.** It derives `Deserialize` and
   `schemars::JsonSchema` and nothing else
   ([`config/mod.rs:44`](../crates/bitrouter-sdk/src/config/mod.rs:44)), as do
   ~24 nested types; there is no `impl Serialize for Config` anywhere in the
   workspace. `PatternMap` — behind every provider's `api_protocol` and
   `rate_limits` — cannot express its wire shape from memory at all. Parsing is
   destructive by construction: `${VAR}` substitution happens before
   deserialization, `resolve_derivations` clears `child.derives`, and the
   registry merge fabricates providers. A serializing daemon would emit a
   different document than the operator wrote, with every `api_key` rendered as
   **resolved plaintext** into a file they may commit.
2. **`PolicyLock` can be.** It derives `Serialize, Deserialize` with
   `deny_unknown_fields`
   ([`policy_lock.rs:57`](../apps/bitrouter/src/policy_lock.rs:57)) and its
   identity is defined over the parsed model, not the bytes. It is machine-
   generated already.
3. **The atomic writer already exists.** `write_text_atomic_unlocked`
   ([`policy_lock.rs:2464`](../apps/bitrouter/src/policy_lock.rs:2464)) takes
   an expected-content parameter — a compare-and-swap primitive, shipped.

Ownership is therefore not one decision applied four times. It is a property
each domain either has or does not, and the difference is legible in the
derives.

---

## 3. Goals and non-goals

### Goals

- One authority over each domain, so two processes never write one store.
- One action surface, served to a local client over a trusted socket and to a
  remote client over HTTP, with identical semantics.
- `git diff`, `git revert`, and `policy diff ACTIVE CANDIDATE` keep meaning
  exactly what they mean today.
- Every step in §16 is independently shippable and independently valuable.

### Non-goals

- **Management verbs in the chat pane.** See §14.
- **Remote compile, verify, evolve, optimize.** These paths are local-**file**-
  bound, not local-database-bound: `load_for_config` requires a file-backed
  `bitrouter.yaml` and `policy-lock.yaml`, and evolve and optimize write both
  files plus the history directory. The self-improving loop is machine-local by
  construction and stays so under every transport design examined.

  > The earlier statement of this non-goal — that `readonly_database_url` and
  > its optimize twin refuse a non-SQLite URL — is **false**; see §17.1. The
  > non-goal survives, on different evidence.
- **Organizations, teams, roles, RBAC, a management audit UI, a web console.**
  A capability belongs here only if its specification requires an **isolation
  boundary between groups of callers** (§22). A **scope** narrows what one
  credential may do to one daemon's state; a **role** assigns a person a
  capability over a partition — a role without a group is just a scope.
  Grouping constructs that are complete given one daemon (a shared budget
  across a set of keys, a reporting label) are **OSS**; only the visibility
  boundary is not. The append-only config change log (§10.4) is the only audit
  surface added here; the **compliance** audit log — retention, export,
  tamper-evidence, SIEM delivery — is the enterprise product's.
- **Scopes on `brvk_`.** See §8.1.
- **A watch / subscribe / event channel.** Nothing exists today and a subscribe
  surface with no consumer is dead code under this repo's own rule. Clients
  poll.
- **Widening the reload seam.** Auth appliers, protocol dispatch, `skip_auth`,
  the DB connection, MCP, server-tools, pricing, guardrails, eval/trajectory,
  and OTel stay process-start-only. The plan classifies them honestly (§10.3)
  instead of making them swappable.
- **TLS inside the daemon.** There is no TLS dependency in the workspace. The
  posture is an operator-supplied terminator, enforced at the bind (§13.2).
- **Multi-config multiplexing.** One daemon still serves one config. This is
  what keeps the daemon a data plane a control plane can sit in front of
  (§7.4, §22).
- **A serializer for `Config`, in any form.**

---

## 4. The mechanism rule — the Serialize Line

> The daemon may **author** a domain only if it can regenerate that domain's
> durable bytes from memory without losing what a human put there.

A domain that passes is daemon-authored. A domain that fails is
daemon-**custodied**: the daemon still validates, versions, gates, and
compare-and-swaps every write, but the bytes are the operator's and pass
through untouched.

| domain | passes? | consequence |
|---|---|---|
| policy | yes | daemon-authored |
| keys | yes | daemon-authored |
| history | yes | daemon-authored |
| config | **no, permanently** | daemon-custodied |

The rule is a **mechanism** rule. It is a serialization audit, and it must not
be mistaken for a scope rule: any store the daemon invents in future
round-trips trivially, so on its own it auto-approves every future management
surface and can only ever refuse the one legacy artifact that predates it.

---

## 5. The scope rule — route relevance

> The daemon may own state that **changes a route or explains one**. It may not
> own state whose only purpose is administering the host.

Transposed from [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §§318–323, which draws the
same line for the TUI: BitRouter *forwards* what the harness knows and *adds
what only the router knows*.

**Enforcement is mechanical, not prose.** A `const ADMIN_DOMAINS:
&[DomainSpec]` table names the domains; a guard test asserts that every mounted
`/v1/admin/` prefix is a member of that table and that every member is mounted.
A fifth domain requires editing the table, and editing the table is the review
gate.

This is deliberate. Every boundary that survived in this workspace is one that
fails the build; the only boundary that was ever prose is the one #786 deleted
~11.5k lines to unwind. The cost is that adding a domain is awkward. That is
the intended cost.

The four members are `config`, `policy`, `keys`, `history`. **None of them is
a tenancy domain**, so the same guard test is the review gate for the editions
ceiling (§22) as well as for scope: a fifth member cannot be added without the
conversation both rules exist to force.

---

## 6. Ownership model

### 6.1 Config — custodied

The human authors; the daemon is custodian. The file stays authoritative on
disk, byte-identical, at the path the existing five-step chain resolves. Every
programmatic write is byte passthrough under `If-Match` through
`write_text_atomic_unlocked`, never a serialization.

A **host-pinned field set** is rejected with `409 host_pinned_field` on every
write path, diffed against the daemon's launch pins: `server.*`,
`database.url`, `policy.path`, `plugins.bitrouter-policy.policy_dir`, and every
`mcp_servers.*.command|args` and `agents.*.command|args`.

That last group is the line between a management API and remote code
execution: an `mcp_servers` stdio `command` is a real process spawn, and the
starter template's own examples are `uvx` and `npx`.

**What this costs:** a client can *ship* a config document but cannot *compose*
one field by field. There is no `PATCH /config/providers/openai`, so a
management UI can offer edit-and-push, not a generic form. The host-pinned half
is never remotely writable at all — "the same actions locally or remotely" is
simply false for those fields, and the spec says so rather than hiding it.

### 6.2 Policy — daemon-authored, sole *online* writer

The daemon takes `acquire_publication_lock` itself and runs prepare → write →
commit inside one request, via the `PolicyRuntime::prepare_for_config` /
`commit` two-phase primitive that already exists. Files, digests, exact-byte
history, and `promotions.jsonl` are unchanged.

`reload_published_policy_or_restore`'s compensating-write dance is **deleted**,
not ported. Today the CLI writes the file, asks the daemon to reload the world,
and on rejection writes the file back and reloads again — with a reachable
terminal state where the compensating write itself fails and disk and daemon
disagree about what is in force.

The **advisory sibling flock is retained**, not replaced by an in-process
mutex, so the offline CLI writer still excludes the daemon. Requiring a running
daemon to publish a policy would be a worse product than the duplication.

**What this costs:** "the daemon owns policy" honestly reduces to "the daemon
is the sole *online* writer of policy". Two writers still exist for the offline
case. The class is bounded and made explicit, not eliminated.

### 6.3 Keys — two disjoint credential classes

`brvk_` is **untouched**: same shape, same unsalted SHA-256, same four-row
`skip_auth` truth table, same `api_principal` derivation, and deliberately no
scopes column.

A new `admin_keys` table carries `bradm_<key_id>.<secret>` — Cloud's shape, so
the row is found by id *before* the secret is verified, which is what permits a
per-row salt — with scopes, `key_prefix`, expiry, and revocation.

Provider secrets live in the already-shipped OAuth credential store (0600,
`create_new` + atomic rename, redacting `Debug`), extended by a generic
API-key applier (§16, step 2), with **no read-back at any path**.

Rationale in §8.1.

### 6.4 History — daemon owns writes, not reads

The daemon owns every write and is the **sole migrator**. Reads stay
daemon-preferred with one named, preserved fallback:
`metering::reader::open_readonly` at `?mode=ro`.

`Mode` grows from three states to four: `Live`, `HistoryOnly`, `Empty`,
`Unavailable`.

Ownership of writes deliberately does not imply ownership of reads. The
no-daemon read is asserted in a code comment, in
[`CLI.md`](CLI.md), in the shipped agent skill, in a design spec, and in a
named test — and *the moment you most want this view is right after the daemon
died*.

**What this costs:** "the daemon owns history" honestly means "owns writes" — a
weaker claim than the decision states. The CLI keeps a `sea-orm` dependency and
a second read implementation that must stay semantically identical to the
endpoint.

---

## 7. Control transport

**One axum `Router`, two listeners.**

| | address | authentication |
|---|---|---|
| local | a **new** `<home>/bitrouter.admin.sock`, mode 0600 | the connection *is* the root credential |
| remote | `server.admin.listen`, **default unset** | `bradm_` bearer + scopes |

### 7.1 Why a new socket, not the existing one

`bitrouter start` detaches a long-lived daemon, so new-CLI-against-old-daemon
is ordinary rather than exceptional. `stop()` has no pid fallback, and
`restart` requires a `Status` response to authenticate the pid — so a protocol
mismatch on the existing socket leaves `kill` as the only exit.

The legacy newline-JSON control socket is therefore **left untouched for at
least one release**, and keeps `Stop` / `start` / `restart` permanently:
filesystem ownership is the better authenticator for daemon lifecycle.

### 7.2 Why not merged into the inference router

Rejected on three grounds, all verified:

- The host wrapper runs *after*
  `.layer(DefaultBodyLimit::max(MAX_BODY_BYTES)).with_state(state)`
  ([`server.rs:283`](../crates/bitrouter-sdk/src/server.rs:283)), and
  `router_wrapper` is where `main.rs` merges the eval router
  ([`main.rs:2735`](../apps/bitrouter/src/main.rs:2735)) — so merged routes
  silently lose the body limit.
- `Router::merge` panics on a duplicate path.
- The safe `listen` / `skip_auth` pairing is a zero-config accident, not a
  type-level property, and must not govern administration.

### 7.3 Long-running operations

Compile, verify, and optimize are `202` + `GET /operations/{id}`. Operation
state is in-memory and lost on restart; the capability manifest says so.

### 7.4 Capability manifest

`GET /v1/admin/capabilities` is **generated from the same const table that
mounts the routes** (§5). A new CLI against an old daemon receives no manifest,
resolves every capability to absent, and behaves exactly as it does today.
That is what makes "no flag day" a property rather than an aspiration.

Capabilities are omitted — not present-and-failing — when the daemon cannot
serve them: config-write when the config parent is not writable (§9.4),
compile/optimize when the database is not an on-disk SQLite file, and every
machine-bound verb on a remote manifest (§13.3).

The manifest describes **this build's four domains and nothing else**. The
enterprise control plane (§22) advertises its own capabilities over its own
surface; it never extends this one. The manifest is also how a build declares
what it cannot serve *without shipping dead code*, which is what lets the
daemon stay a data plane that a separate control plane can sit in front of.

---

## 8. Authentication and authorization

### 8.1 Why `brvk_` gains nothing

Two independent reasons, either sufficient:

1. **It cannot be salted.** `brvk_`'s unsalted digest *is* the ACP
   `api_principal` used for route-lease namespacing. Salting it is a wire break
   on every live lease.
2. **It leaks by design.** It is the credential templated into harness
   environments, shell profiles, and process listings. Elevating it to
   control-plane authority makes an inference-key leak an admin-key leak.

A disjoint class also dissolves the legacy-key-authority question entirely: no
existing key gains or loses anything on upgrade.

### 8.2 The scope vocabulary

Scopes live on `admin_keys` only, with RFC 6749 §3.3 subsetting at mint.

- `config:read` — redacted effective config only.
- `config:read:source` — raw bytes; **not in any default grant**.
- `config:write`, `policy:read`, `policy:publish`, `keys:read`, `keys:write`,
  `history:read`.
- `daemon:lifecycle` — withheld from every default grant and from the bootstrap
  principal.

**This vocabulary is closed.** It contains no tenant×verb member. Cloud's
`namespace:*`, `user:write` and `billing:*` are permanently out of scope for
the OSS build (§18.2, §22). Admin-plane OIDC/OAuth *authentication* for one
daemon is OSS; only cross-tenant federation, SSO **enforcement policy**, and
SCIM are enterprise.

### 8.3 Hardening

- Startup **refuses** `skip_auth: true` on a non-loopback `server.listen`.
- `ensure_admin_bind` hard-refuses a non-loopback admin bind without an
  explicit TLS-terminator acknowledgement, modelled on `ensure_loopback_bind`
  ([`crates/bitrouter-mcp/src/server.rs:671`](../crates/bitrouter-mcp/src/server.rs:671)).
- A required non-simple header plus a **Host allowlist**, modelled on the
  shipped MCP transport validator but **without** its `null`-Origin acceptance.
- Remote-vs-local is classified from an **explicit configured trust boundary,
  never from the peer address** — behind the reverse proxy this design's
  no-TLS posture mandates, every remote caller presents as loopback. The tree's
  own `listen_is_local` helper classifies `0.0.0.0` and `::` as local and is
  unsafe for this purpose.
- `api_principal` is removed from every ACP payload and derived server-side
  from the verified credential.
- Write-only provider credentials: no read-back at any path.
- `409 secret_in_document` on a literal `api_key` in a config write.

---

## 9. Bootstrap, repair, and disaster recovery

### 9.1 The repair listener

`serve()` calls `build_app_with_path` before the control socket exists, with at
least seven config-reachable `?` points between them (`db::connect`,
`run_migrations`, `build_auth_appliers`, guardrail compilation, trajectory
retention arithmetic, the correlation key, `PolicyRuntime::new`). A bad config
therefore leaves **no channel through which to fix it**.

A `RepairState` binds immediately after `paths::load_config` and **before**
assembly, serving `GET /status` with the assembly error verbatim,
`GET|PUT /config/source`, `POST /config:validate`, `POST /db/migrate`, and
policy rollback. On assembly failure the daemon stays **alive and degraded**
rather than returning from `serve()`.

`bitrouter serve` gains `--exit-on-assembly-failure` for CI, which depends on
the old exit-code contract.

### 9.2 Daemons not owned by a human

"The 0600 socket is the root credential" has no answer for systemd
`User=bitrouter` or a container that drops privileges. Two mechanisms:

- an `SO_PEERCRED` uid/gid allowlist — **required**, not opt-in, for any daemon
  whose config may be written remotely;
- a documented `sudo -u <daemon-user> bitrouter key sign` recipe.

The pre-assembly `RepairState` (§9.1) is a **permanently unscoped, root-
authority, single-tenant surface**: it carries config-write and db-migrate
authority, and nothing that arrives during assembly can scope it. Its exposure
is an operator responsibility and must be documented as such. A fleet control
plane repairs a daemon by rewriting its config and restarting it — never by
reaching the repair socket.

### 9.3 Migrations must be survivable

- Every new migration existence-guards its DDL.
- The SQLite file is copied to `<home>/db-backup-<version>.sqlite` before
  anything pending is applied. (`sea-orm` wraps migrations in a transaction
  only on Postgres.)
- `bitrouter db repair --mark-applied <version>` ships with step 1.
- **The phases are not reversible, and the spec says so.** `Migrator::up` hard-
  errors when a previously-applied migration's file is missing, and
  `run_migrations` is called unconditionally
  ([`assemble.rs:224`](../apps/bitrouter/src/assemble.rs:224)) — so reinstalling
  the previous binary prevents startup entirely. Each migration-bearing step
  publishes its real downgrade procedure.

### 9.4 Writability

`write_text_atomic_unlocked` renames a sibling temp file, which fails on a
bind-mounted file (`EBUSY`), a ConfigMap volume (`EROFS`, and the target is a
symlink), and a read-only Nix store — that is, in every shape a remote daemon
actually deploys in. A startup probe omits config-write from the manifest when
temp-file-plus-rename cannot work.

### 9.5 One database, one daemon

An exclusive advisory lock is taken on the resolved database after
`db::connect` and before `run_migrations`, refusing to start with the holding
pid and socket named. `start`'s existing guard is per-socket-path, so two
project configs sharing one absolute `database.url` today yield two daemons,
two migrators, and one unlocked database.

**The invariant is stated per backend, because the mechanism differs.**

- **SQLite** — an exclusive advisory lock on the resolved file. §9.3's
  pre-migration backup is a file copy of that file.
- **Postgres** — `pg_advisory_lock`, specified as a **named exception** to the
  migration module's no-hand-written-SQL rule (it is a session lock call, not
  DDL). §9.3's file-copy backup has **no Postgres equivalent**; the operator's
  own backup is the answer, and the docs must say so. Acceptance #4 is restated
  per backend accordingly. This matters most in exactly the shape Postgres is
  kept for: a rolling update starts the new pod before the old terminates.

The sole-writer rule has **exactly one bounded exception**: an offline
`key sign` against a local SQLite file the daemon is not serving (§16 step 1).

### 9.6 Recovery asymmetry

Losing `bitrouter.db` is survivable: the daemon recreates it and the operator
remints over the UDS. A corrupt `policy-lock.yaml` under a preset binding is
**startup-fatal**. The retained offline `policy rollback` / `show` is the
documented DR procedure, and the failed-lock startup error prints the available
history digests and the exact rollback command.

---

## 10. Config custody — the write contract

### 10.1 The write

`PUT /v1/admin/config/source`. Body is the operator's YAML text. `If-Match` on
the source digest. Straight through `write_text_atomic_unlocked`. Never a
serialization.

### 10.2 Drift

The response to `GET /config` carries `source_digest`, `loaded_digest`, and a
`config_drifted` flag; `PUT` refuses on drift with `409 config_drifted`.

**Correction to the earlier rationale.** This was argued from policy already
reporting a `disk_digest` while config reported nothing. That is wrong:
`disk_digest` appears exactly once in the repository — in that sentence.
`PolicyRuntime::status` does compute a live policy digest, but its only callers
are two test assertions in `reload.rs`. The real starting point is that
**neither domain reports anything**, so both are new work.

**The reload/`PUT` asymmetry is intentional, and analogous to git**: a
non-fast-forward push is refused, while any local commit is accepted. A hand
edit plus `bitrouter reload` stays fully permissive (§18.7); only the API path
refuses on drift. This is not an inconsistency to be resolved later.

### 10.3 Applicability

A `Hot | RestartRequired | Rejected` table, with a guard test enumerating every
top-level `Config` field exactly once.

- **`Hot`** — what `DaemonCommand::Reload` actually swaps.
- **`RestartRequired`** — accepted only after a **sandboxed dry
  `build_app_with_path`**; otherwise `409 would_not_start`. This is not
  hypothetical: `trajectory: {retention_days: 2592000000}` passes
  `validate_trajectory_config` today and then kills startup at a
  `checked_sub_signed`, while the API would be telling the operator to restart.
  The sandbox must suppress MCP child-process spawning and use a temp database,
  or validating a config becomes a side effect.
- **`Rejected`** — a change adding a provider that needs a new auth applier or
  protocol adapter. `HttpExecutor.auth_appliers` and `dispatch` are immutable
  fields, so accepting yields a routable provider with no credential applier —
  worse than a no-op.

`Rejected` means the API is **not a superset of the file**. That is documented,
not discovered.

Parse-time bounds land on `trajectory.retention_days` and
`continuation.retention_days`, with a guard test asserting every
config-reachable fallible call in `build_app_with_path` has a corresponding
parse-time validator.

### 10.4 The change log

An append-only `config_changes(seq, digest, actor, reason, applied_at)`, with
`GET /config/history` and `bitrouter config log`. The log **stays** — a
single-tenant daemon still has principals: §6.3 and §8.2 create `admin_keys`
bearing `bradm_<key_id>.<secret>` with per-key scopes and expiry, and §16 step
5 adds `display_name`. A remote `PUT /config/source` is therefore attributable
to a named key.

- `actor` is the authenticated `bradm_` key id, or `NULL` (rendered as the
  literal `socket`) on the 0600-socket path.
- `reason` is **optional**. It cannot be required: the reload path — which
  every document recommends, and which §18.7 keeps permissive — writes no row
  at all, so a required field would bind only the API path while implying it
  bound every change. Acceptance #9 is worded accordingly.

This is a **change log, not a compliance audit log**. Retention policy, export,
tamper-evidence and SIEM delivery are the enterprise product's (§22).

### 10.5 Materialization

Zero-config may be materialized to a file only with an **explicit target
path**, and is reversible via `POST /config:dematerialize` — it otherwise
creates a user-global `~/.bitrouter/bitrouter.yaml` that permanently changes
resolution for every future invocation.

One correction to the received rationale: env re-sensing **survives**
materialization. `reload.rs`'s `File` arm runs `apply_builtin_defaults`,
`enable_if_logged_in`, `merge_registry_into`, and
`activate_stored_credential_providers` identically to the `Default` arm. The
only `Default`-only step is `cloud::enable_in_zero_config`, so materializing
silently drops a signed-in user's gateway provider. Fix: call it on the `File`
path too.

---

## 11. Policy publication

`POST /v1/admin/policy/publications` and `/rollbacks`.

- The daemon takes `acquire_publication_lock` and runs prepare → write → commit
  under `reload_lock`.
- The sibling advisory flock is retained (§6.2).
- Compile / verify / optimize are `202` + operations, present in the manifest
  **only** where the daemon holds a persistent on-disk SQLite database.
- The admin plane gets its **own read-only `sea-orm` pool**, in the same PR —
  `sqlx-sqlite` forces a single connection for a file URL, and that connection
  also serves the per-request spend-cap read, so minutes-long analytical work
  in the serving process would otherwise add latency to inference.

Five additions, all of which protect the differentiator rather than the
control plane.

1. **The commons rule, stated at the entry points.** A single-tenant daemon
   compiles from **every admitted caller's** evidence. The eval exchange's
   owner scoping is per-caller *submission integrity*, not a partition. An
   operator who wants separation runs a second daemon. State this as a doc
   comment on `EvalEvidenceSnapshot::load`, `materialize_snapshot`, and
   `persist_snapshot` — today `optimize` passes owner `None` while
   `snapshot_by_root_for_owner` sits unused beside the accessor actually
   called, which reads as undecided rather than decided (§18.4).
2. **Commons settles partition, not attribution.** Either surface contributing
   owners in the snapshot manifest, or state plainly that evidence provenance
   is unattributed in v1. The tree already records per-caller admission
   attribution in `eval_admission_events.authority_id` / `reason`.
3. **A writer class on `PromotionRecord`**, distinguishing operator, optimizer,
   and daemon-recovery — **before** any remote `policy:publish` ships. Today an
   operator's rollback and the optimizer's compensating rollback write
   byte-identical records.
4. **A warn-only lineage check on load**, scoped to locks this daemon
   published, and **never firing on `parent_digest: None`**. A hard check would
   fire on every git-deployed host and on `templates/auto-router/policy-lock.yaml`,
   which carries `evidence_root`, `source_snapshot_time_unix_ms` and `compiler`
   but no `parent_digest` at all.
5. **`policy diff` must cover every field of the documents it compares** before
   it becomes an HTTP endpoint. It omits `default_tier`, `tool_use_tier`,
   `key_strategy`, `adequacy`, `predictor`, certificates, and artifact lineage
   — so `git diff` currently beats the product's own diff on the
   reviewer-facing surface of the feature the README sells.

`/v1/admin/access-policies` is **out of v1** until the two `policy` nouns are
renamed and `policy create --dir ./policies` versus
`plugins.bitrouter-policy.policy_dir` is reconciled — today the default
invocation writes a file the daemon never loads.

---

## 12. History

Routed through the daemon: `trajectory prune`, `eval subject put`,
`eval result submit`, `eval snapshot freeze`, `reconcile-metering`,
`apply-reward-feedback`, and `optimize run`'s `persist_snapshot`.

Each **retains an explicit `--database-url` offline mode that runs its own
migrator** — they are declared out of scope for daemon ownership, so the
sole-migrator invariant does not bind them — and fails with
``run `bitrouter migrate --database-url <url>` `` when the schema is absent.
Headless benchmark pipelines use these today with no daemon and no migrate
step; shipping without the offline mode is a silent break of pipelines nobody
in this repo can see.

A loopback auto-start bridge covers local mutations, gated by
`spawn::listen_is_local`, with `--no-daemon-autostart`. This means
`bitrouter policy publish` in a fresh clone can silently start a server and
bind a port, which the docs must say out loud.

`/v1/evals/*` **stays on the inference listener** and remains per-caller
owner-scoped. The owner-namespace split must be resolved before
`eval result submit` is daemon-routed: `insert_result_owned` resolves
`subject_for_owner` and bails `unknown eval subject` when the subject is not
that owner's, while `local` is a reserved id no caller can hold — so an
admin-plane submit against a caller-owned subject would fail with a misleading
error while compilation merges both namespaces anyway. Either the daemon-routed
submit carries an explicit owner, or it is dropped from the daemon-routed set.

Also: an index on `requests(created_at)`, and pagination plus time windows on
the eval and trajectory read paths — without which those reads cannot become
HTTP handlers at all.

---

## 13. Remote operation

### 13.1 Addressing

`--endpoint` / `$BITROUTER_ENDPOINT`, with a per-endpoint 0600 credential file,
and **the resolved endpoint echoed in every JSON envelope** so a destructive
command can never be silently aimed at the wrong daemon.

### 13.2 Posture

The daemon terminates no TLS. The honest posture is "terminate in front of it",
enforced at the bind rather than asserted in docs (§8.3). An operator who sets
the insecure acknowledgement is putting a never-expiring bearer on a plaintext
socket, and the flag name should say so.

### 13.3 What is absent remotely

Machine-bound verbs are **absent from the remote manifest**, not
present-and-failing: `reload{env}`, compile/verify/optimize, `start`/`restart`,
pid, and log tail.

This follows the picker rule from
[`crates/bitrouter-tui/src/picker.rs`](../crates/bitrouter-tui/src/picker.rs):
a control that cannot act is worse than an absent one, because absence is
legible and a dead control is a lie.

---

## 14. The chat pane

**Management verbs are cut from v1.** The pane keeps `/commands` and `/route`
and gains nothing.

`crates/bitrouter-tui` is session-scoped by charter — *"nothing daemon-wide is
reachable, so nothing daemon-wide can be drawn"* — and
[`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §8.3's violation trigger is literally
"renders daemon-wide data in the TUI", a rule ratified by deleting 19,955
lines.

Passing a typed admin handle in from the launch half would pass the existing
guard **by construction**: it scans two files for nine literal strings, none of
which is a control-client name. A design that justifies itself by "the guard
would still pass" has found a hole in the guard, not a permission.

So instead: **the guard is widened** to every file under `chat/` and to the
control-client module path and type names.

**CLI/TUI parity is therefore claimed at the binary level and explicitly denied
at the pane level.** Both drive one control client; only one of them exposes
it. Changing that requires retiring `ACP_TUI_SPEC.md` §§463–465 and
`crates/bitrouter-tui/src/lib.rs` §§51–54 in their own change, with the #749
record amended — see §18.1.

If BitRouter commands ever do land in the pane, they must be namespaced under
one prefix. `submit` checks BitRouter's names and returns early before
`Effect::Prompt`, so each claimed name is permanently removed from the agent's
namespace while still being drawn from its `AvailableCommandsUpdate` list.
This is already a latent bug for `/commands` and `/route`.

---

## 15. Migration and compatibility

- **Nothing moves on disk.** Formats, paths, and digests are unchanged in every
  phase.
- **No existing key changes authority.** `brvk_` is untouched.
- **An old daemon serves no manifest**, so a new CLI resolves every capability
  to absent and behaves as today.
- **The legacy control socket is untouched** for at least one release.
- **The `api_principal` derivation change (§8.3) is a coordinated wire break**
  that resets live route leases. Tolerable because they are in-memory with a
  12-hour crash-backstop TTL, but it must ship in one release with its clients.
- **`bitrouter key sign --db` survives as an explicitly named offline tool**,
  which is an acknowledged, permanent hole in the sole-writer rule (§18.5).

---

## 16. Delivery sequence

Each step is a PR someone can land, in dependency order, and each is
independently valuable — if the project stops after step *n*, step *n* must
still have been worth shipping.

**The ordering proposed during earlier discussion is revised on the evidence:**
scopes do not go on `brvk_` and are not step one; the router is not mounted the
way `eval/api.rs` is; and no `send_command` transport trait is built.

### Step 1 — Agree on the database file

One home-anchoring implementation (`db::anchor_url`), with the differing
missing-file and mode semantics of the three private re-derivations passed as
parameters rather than reimplemented. `local_eval_service` fixed to anchor,
with the regression test the trajectory path has and the eval path lacks.
`bitrouter migrate` and `bitrouter db repair --mark-applied`. `Migrator::up`
removed from the five CLI sites, leaving `assemble.rs` the sole caller, pinned
by a source-scanning guard test — **including the call in `key_sign`**, which
acceptance #3 already forbids. The database advisory lock (§9.5) and the
pre-migration backup (§9.3).

`key sign --db` anchored, narrowed, and reclassified as a declared offline
tool (§18.5): it **refuses `postgres://` and `mysql://`** with an error naming
`POST /v1/admin/keys` and `bitrouter migrate`, and it **takes the same advisory
lock**, failing fast with a message naming the key-creation endpoint when a
daemon holds it. This last part is the hazard the step is named for, on the
default path: `--db` defaults to `sqlite://./bitrouter.db` and `database.url`'s
default is the same string, so after anchoring the everyday no-flag invocation
targets the live daemon's file by construction, takes no lock, and runs a
second migrator.

Also in this step, because they are the same class of defect: the **non-SQLite
refusal added to both `readonly_database_url` functions**
([`policy_lock.rs:2348`](../apps/bitrouter/src/policy_lock.rs:2348),
[`controller.rs:779`](../apps/bitrouter/src/optimization/controller.rs:779)),
with the first tests either has ever had — today both return the URL unchanged
for a server backend, silently dropping the `mode=ro` pin that is their entire
purpose (§17.1). And `sqlx-mysql` dropped from the workspace `sea-orm` feature
lists (§18.6), updating `error_report.rs`, `CLI.md`, `main.rs` and
`db/mod.rs` in the same change.

*Why first:* everything downstream assumes the CLI and the daemon agree which
file they mean and that exactly one process migrates it. Today they
demonstrably do not.

*Value alone:* fixes a live anchoring bug on the eval path; closes the hazard
where a stray CLI invocation in the wrong directory creates a second database
holding `api_key` hashes at default umask; makes the WAL invariant true by
guaranteeing the daemon is the first writer. No API, no wire change.

*Risk:* the lock refuses a start that previously succeeded when two configs
share one `database.url`. That is the point, and will be reported as a
regression, so the error must name the holding pid and socket.

### Step 2 — Make the credential store work for the whole catalog

A generic `ApiKeyCredentialApplier` for every provider whose catalog auth kind
is an API key, resolving `CredentialStore::get(provider, label)` and falling
through to inline `provider.api_key`, with store-wins precedence pinned by a
test. `activate_stored_credential_providers` refuses to activate a provider
whose stored credential no registered applier can consume.
`cloud::enable_in_zero_config` called on the `File` reload arm.

*Why second:* appliers exist for exactly **seven** providers
([`assemble.rs:1061`](../apps/bitrouter/src/assemble.rs:1061) — `bitrouter`,
`github-copilot`, `anthropic`, `claude-code`, `openai-codex`, `supergrok`,
`google-ai`). For `openai`, `openrouter`, `groq`, and every registry BYOK
entry there is none — so remote key rotation is a silent no-op and activation
marks a provider routable with a stale key. Building the control plane first
would ship a keys plane that lies.

*Value alone:* `bitrouter providers login <any api-key provider>` starts
working. No control plane required.

*Risk:* a visible precedence change, and some currently-"active" providers
become inactive on upgrade — correctly, but it needs a startup warning naming
each one.

### Step 3 — Admin router: read-only, own socket, repair binding

`apps/bitrouter/src/admin/` with `ADMIN_DOMAINS` and its guard test (§5). The
router on the new 0600 socket (§7). The `RepairState` pre-assembly binding
(§9.1). Read routes only: `/status`, `/config` (with digests, drift flag, and
a redacted `effective` rendering `api_key` as `{present, origin}`),
`/config/source`, `/policy`, `/keys`, `/requests` (the existing
`RequestsReport` verbatim), `/spend`, `/route`, `/observe`. Cloud's error
envelope adopted — **after** correcting `eval/api.rs`, which emits
`{"error": <human message>}` where Cloud's shape uses the same field name for a
*code*. `DaemonResponse::Error` gains a machine-readable code and the
string-prefix classifier in `acp_cli.rs` is deleted. The chat guard widened
(§14).

*Why third:* mechanism before policy. UDS 0600 is exactly today's trust
boundary, so this has a provable zero security delta — and it is the last point
at which the surface can be shaped before anything depends on it.

*Value alone:* the daemon's **live** policy digest becomes observable for the
first time (today `policy status` reports what is on *disk*, which may differ);
live-vs-disk config drift becomes observable at all; and a daemon that fails to
assemble becomes diagnosable instead of reading as "stopped".

*Risk:* the repair listener is new lifecycle code in the startup path, and
`start`'s readiness poll must now distinguish degraded from ready.

### Step 4 — Config custody: the write path, local only

Everything in §10.

*Why fourth:* config is the domain the decision is about, and every later
remote capability is a projection of it. It lands before policy because
`policy publish` rewrites `bitrouter.yaml` through the same line editor.

*Value alone:* a safe programmatic config write with a CAS that refuses rather
than clobbers a concurrent human edit; an honest hot-vs-restart-vs-rejected
answer; and a change log — the first time "who changed routing and why" is
answerable without git archaeology.

*Risk:* the dry-assembly sandbox is the most intricate new machinery in the
plan.

> **This is the defensible place to stop.** Steps 1–4 fix two live bugs, add
> the change log, and require no new credential class, no TCP listener, and no
> scope vocabulary.

#### Hygiene landing alongside these steps

Small, independent, and each already required by a rule this repo enforces:

- **Reword the three tenancy-claiming surfaces** to *multi-caller*:
  `commands.rs:34`, `db/mod.rs:10`, `providers/apply.rs:31` (§18.2).
- **Delete `freeze_snapshot`** (`eval/store.rs:383`) — zero production callers,
  CLAUDE.md rule 4.
- **Remove `#[allow(clippy::module_inception)]`** at `policy/mod.rs:11` —
  CLAUDE.md rule 1 forbids `#[allow]`.
- **Move the enterprise-facing handle off the rule-2-violating
  `pub use assemble::{...}`** in `lib.rs`.
- **Wire `substitute_env` into `config::load`** and **make `set_env_overrides`
  merge** rather than replace (§18.7).
- **Document `.bitrouter-policy-history/` and `promotions.jsonl`** in `CLI.md`
  and the shipped skill, and decide whether they are committed artifacts.
- **Add an "Editions and your contribution" section to `CONTRIBUTING.md`**
  once §22.5's timing allows.
- **Keep `skills/bitrouter/` in lockstep** with every CLI change in these steps
  — CLAUDE.md requires it in the same change.

### Step 5 — Key lifecycle and the admin credential class

Migration adding `display_name`, `key_prefix`, `revoked_at`, `last_used_at` to
`api_keys` — and deliberately **no** `scopes` column. The `admin_keys` table
and the `bradm_` shape. The scope vocabulary of §8.2.
`POST/GET/DELETE /v1/admin/keys`, carrying forward the reserved-id refusal for
`local` and `anonymous`. Write-only provider credential endpoints.
`bitrouter key list` and `bitrouter key revoke`.

**`POST /v1/admin/users` is deleted** — `users` is `(id, created_at)` with
nothing to manage, so user creation folds into `POST /keys`, which is what
`key_sign` already does. `GET /keys`, `/requests` and `/spend` are
**machine-wide by design** in a single-tenant daemon; that is already what
`recent_requests` and `get_total_rate` do, and single-tenancy makes it correct
rather than accidental.

Decide `api_keys.spend_limit_micro_usd` and `rpm_limit` in this step: **enforce
them or delete them.** They are read into `ApiKeyRecord` on every authenticated
request and enforced by nothing — dead surface under CLAUDE.md rule 4, and a
competitive gap against LiteLLM's and Bifrost's free per-key budgets and rate
limits.

*Value alone:* `key revoke` lands. Today `active` and `expires_at` are read and
denied correctly on every request and have **no production writer at all** —
the only `active = 0` write in the tree is a unit test. Lookup is uncached, so
a revoke takes effect on the next request with no reload.

*Risk:* a second credential class is permanent surface and needs its ceiling
stated in the same change (§18.2).

### Step 6 — Policy publication inside the daemon

Everything in §11.

*Why sixth:* publish rewrites `bitrouter.yaml`, so it depends on config custody.
This is the one real dependency edge between domains.

*Value alone:* removes the reachable terminal state where the compensating
write itself fails and disk and daemon disagree about what is in force.

### Step 7 — Remote: TCP listener, addressing, transport hardening

Everything in §13, plus §8.3's hardening, `Mode::Unavailable`, and §9.2's
bootstrap allowlist.

*Value alone:* **this is the decision's actual deliverable** — a remote CLI can
read status, config, policy, and history; publish and roll back policy; and
mint and revoke keys against a daemon on another host.

### Step 8 — History writes behind the daemon, and the offline-tool contract

Everything in §12.

*Why last:* history is the only domain where the daemon is already the sole
writer on the hot path; the remaining hazard is second-process writers, not
ownership. It also depends on step 7 to know what a remote
`status --requests` means.

*Risk:* the CI story is the whole risk. The retained offline mode is the
mitigation.

---

## 17. Acceptance criteria

1. `Config` has no `Serialize` derive and no manual impl; a guard test asserts
   it.
2. Every mounted `/v1/admin/` prefix is a member of `ADMIN_DOMAINS`, and every
   member is mounted; a guard test asserts both directions.
3. Exactly one call site invokes `Migrator::up`; a source-scanning guard test
   asserts it.
4. Two daemons cannot start against one resolved database; the second refuses
   and names the first's pid and socket. Stated per backend: an advisory file
   lock on SQLite, `pg_advisory_lock` on Postgres (§9.5).
5. A config whose assembly fails leaves a daemon serving `GET /status` with the
   error verbatim and accepting a corrective `PUT /config/source`.
6. A `RestartRequired` write that would not start is refused with
   `409 would_not_start` before it is written.
7. A config write containing a literal `api_key` is refused with
   `409 secret_in_document`; a write touching a host-pinned field is refused
   with `409 host_pinned_field`.
8. A config write with a stale `If-Match`, or against drifted state, is
   refused; the file is never clobbered.
9. Every accepted config write **through the API** appears in `config_changes`
   with a resolved `actor` — a `bradm_` key id, or `socket`. `reason` is
   optional, and the criterion does not bind the reload path, which writes no
   row (§10.4).
10. `git diff` on `bitrouter.yaml` after a `PUT` shows only the operator's
    submitted bytes — no reordering, no comment loss, no resolved secret.
11. A policy publication that the daemon rejects leaves disk and daemon in
    agreement, with no compensating write.
12. Offline `policy rollback` and `policy show` work against a stopped daemon.
13. `bitrouter status --requests` works with no daemon running and labels its
    source; against an unreachable remote daemon it reports `Unavailable`.
14. Every retained offline tool succeeds against a fresh database or fails with
    the exact `bitrouter migrate` command to run.
15. `brvk_` shape, digest, `api_principal` derivation, and the four-row
    `skip_auth` truth table are byte-identical before and after.
16. `key revoke` takes effect on the next request with no reload.
17. No provider credential is readable back through any endpoint.
18. A default install opens no new TCP port.
19. A non-loopback admin bind without the TLS acknowledgement refuses to start;
    `skip_auth: true` with a non-loopback `server.listen` refuses to start.
20. Behind a reverse proxy, machine-bound verbs remain absent from the remote
    manifest.
21. A new CLI against an old daemon degrades to today's behaviour with no
    error.
22. The widened chat guard fails the build if any file under `chat/` names the
    control client.
23. The eval exchange's per-caller `owner_user_id` scoping, the continuation
    registry's HMAC owner hard-bail, and the ACP route-lease namespacing on
    `route_scope_id` are unchanged, each pinned by a named test. An
    authenticated eval submission is still refused against another caller's
    subject.
24. A policy publication records its writer class; an operator rollback is
    distinguishable from an automatic recovery rollback (§11).
25. A lock is byte-reproducible by OSS `policy verify` from its evidence root
    **regardless of which edition compiled it** — the cross-edition guarantee
    that keeps EVAL_LOCKFILE_SPEC §3.1 goal 5 ("ship one lock") true across the
    boundary in §22.
26. `cargo nextest run --all-features`, `cargo clippy --all-features`, and
    `cargo fmt -- --check` pass on the final tree.

### 17.1 Claims to re-verify before implementation

Independently confirmed while writing this spec: the `Config` derive and the
absence of any `Serialize` impl; the `PolicyLock` derive;
`write_text_atomic_unlocked`; `run_migrations` at `assemble.rs:224`;
`anchor_url`; the seven registered auth appliers; `DefaultBodyLimit` ordering
versus `router_wrapper`; and `ensure_loopback_bind`.

Taken from the design research and **not** independently re-confirmed — each
should be re-checked before the step that depends on it:

- that the only `active = 0` write in the tree is a unit test (step 5);
- the `trajectory.retention_days` startup overflow (step 4);
- `sea-orm`'s `Migrator::up` behaviour on a missing applied migration, and
  transaction-wrapping only on Postgres (step 1);
- that `sqlx-sqlite` forces a single connection for a file URL (step 6);
- the seven config-reachable `?` points between `load_config` and the control
  socket (step 3).

**Re-verified and FALSE** — `readonly_database_url`'s on-disk-SQLite
requirement (§3 non-goals, §18.6). Both functions open with:

```rust
let Some(after_scheme) = url
    .strip_prefix("sqlite://")
    .or_else(|| url.strip_prefix("sqlite:"))
else {
    return Ok(url.to_string());
};
```

so a `postgres://` or `mysql://` URL is returned **unchanged**. Compile,
verify, evolve and optimize therefore run on a server backend **today**,
against a read-write connection, silently dropping the `mode=ro` pin that is
the function's entire purpose. Two consequences: §3's non-goal survives on
different evidence (these paths are local-*file*-bound), and §18.6(a) is
**work, not documentation** — the guard has to be written (§16 step 1).

---

## 18. Decisions taken

All seven are resolved. The owner has ratified single-tenant OSS with
closed-source self-hosted multi-tenancy (§22), and that settles or reshapes
most of what follows. Each entry records the decision, the reasoning, and — where
it matters — what the decision does *not* settle.

### 18.1 Management verbs in the chat pane — **(a) keep the boundary**

Parity is true at the binary level and explicitly false at the pane level. The
chat guard is widened rather than satisfied (§14). Revisiting this requires
retiring `ACP_TUI_SPEC.md` §§463–465 and `crates/bitrouter-tui/src/lib.rs`
§§51–54 in their own change, with the #749 record amended — never as a side
effect of a control-plane change.

### 18.2 Ceiling on the authorization model — **(a) hard ceiling**

No organizations, teams, roles, RBAC, management audit UI, or console in the
OSS build. Self-hosted multi-tenancy is the closed enterprise product (§22).

Consequences, which are enumerative rather than structural because **nothing in
the tree is a tenant boundary today** — every partition key is a *caller*
(`user_id`, `api_key_id`, `route_scope_id`, `owner_user_id`) and `users` is
`(id, created_at)`:

1. **Nothing is deleted.** No proposed admin route carries an `{nsid}` segment.
   Exactly one route goes: `POST /v1/admin/users`, folded into `POST /keys`
   (§16 step 5).
2. `GET /keys`, `/requests` and `/spend` get *simpler*: machine-wide is already
   what `recent_requests` and `get_total_rate` do, and single-tenancy makes it
   correct.
3. §8.2's vocabulary is **closed** — verb×domain, no tenant×verb member.
4. `ADMIN_DOMAINS` is frozen at four, with the guard test as the review gate
   for the editions ceiling as well as for scope (§5).
5. **Three per-caller integrity properties are relabelled, never deleted**: the
   eval exchange's `owner_user_id` scoping, the continuation registry's HMAC
   owner hard-bail, and the ACP route-lease namespacing on `route_scope_id`.
   Rename the misleading test `authenticated_exchange_is_tenant_scoped`.
6. Reword the three surfaces that claim tenancy to say **multi-caller**:
   `commands.rs:34` (the starter config `bitrouter init` writes), `db/mod.rs:10`,
   `providers/apply.rs:31`. Leave `crates/bitrouter-mcp` and
   `crates/bitrouter-observe` alone, where *multi-tenant* correctly describes an
   HTTP transport or a closed host, and leave `guardrails/hooks.rs:12` alone,
   which correctly names the closed consumer.

### 18.3 `policy.mode` — **(c) leave it in `bitrouter.yaml`**

Demote it in the docs to advisory **deployment posture**, and make
`policy:publish` the real gate for the online path.

**`policy.mode` was never a guard.** `PolicyRuntimeMode::apply_to_adequacy`
([`config/mod.rs:253`](../crates/bitrouter-sdk/src/config/mod.rs:253)) opens
`let _ = self;` and sets `enabled = false` and `explore_enabled = false`
unconditionally in *both* modes; two named tests pin identical routing across
modes; and `optimize run` already flips frozen→adaptive by design, ratified in
three documents. So the diagnosis in the original §18.3 was right about the
symptom and wrong about the cause, and moving the field would trade the
differentiator for nothing.

- **(a) rejected** — it breaks EVAL_LOCKFILE_SPEC §3.1 goals 5 and 6, and makes
  the compiler's own output grant the compiler write authority.
- **(b) rejected** — it moves a routing-governance value out of the git-tracked
  file into daemon-only state that is by construction not diffable in a PR.
  Noted for the record: (b) alone would delete the §16 step-6 dependency edge
  between config custody and policy publication. That does not outweigh what it
  costs.

The real work is the three items §18.3 never named — the writer class, the
warn-only lineage check, and widening `policy diff` — all specified in §11.

### 18.4 Evidence scoping for compilation — **(a) a commons**

Single-tenant means there is no isolation boundary to scope evidence to, and
the architecture already agrees where it counts: the adequacy tables that
compile into the lock have **no owner column at all**.

Three qualifications, specified in §11: ratify the commons **in code** at the
entry points, not only in prose; state explicitly that the commons answer
settles **partition** and not **attribution** (which is what the question
actually asks); and give the OSS answer to an operator who wants caller A's
evidence out of caller B's policy — **run a second daemon**, which is free
today and is stronger isolation than a shared schema.

Also: delete `freeze_snapshot` (`eval/store.rs:383`), the fully unscoped
variant with zero production callers, under CLAUDE.md rule 4. Keep
`freeze_snapshot_for_owner` and `snapshot_by_root_for_owner`, which have real
callers.

### 18.5 `key sign --db <remote-postgres>` — **narrowed, not removed**

Keep `--db` for **local SQLite bootstrap and disaster recovery**; refuse
`postgres://` and `mysql://`. Do **not** build option (c)'s
`bitrouter admin mint --db` — new permanent surface for a use case just
declared out of product.

**The narrowing is derived from the sole-writer rule and the §9.5 lock, never
from tenancy.** Removing a documented capability with the paid tier named as
the reason, in the same change that publishes a never-gate promise (§22), would
void the promise on day one.

The hole that actually bites is not the one the original option list named —
see §16 step 1 for the default-invocation collision and the two fixes it
requires.

### 18.6 Postgres and MySQL — **(a) SQLite-first, and MySQL is dropped**

With the premise corrected: this is **work, not documentation** (§17.1).

1. Correct §3 and this section — the server-backend guard does not exist.
2. **Add** the non-SQLite refusal to both `readonly_database_url` functions,
   with the first tests either has ever had (§16 step 1).
3. **Keep Postgres**, second-class, for inference and metering, with
   compile/optimize omitted from the manifest there. The justification is
   ephemeral compute with no durable volume — a filesystem constraint, the same
   shape §9.4 already handles.
4. **Drop MySQL.** Zero tests, zero CI services, no transactional DDL, no
   backup story, no lock story; it pulls `rsa` and `num-bigint-dig` for a
   password handshake; and its case-insensitive default collation is the
   sharpest available hazard to the lock's evidence digest.
5. Implement `pg_advisory_lock` and restate acceptance #4 per backend (§9.5).

### 18.7 Hand-editing plus `reload` — **(a) fully permissive**

A hand edit plus `bitrouter reload` works exactly as it does today. Only the
API path refuses on drift, and that asymmetry is intentional (§10.2).

**(b) is rejected as worded**, on three grounds. It *under-reports*: the same
reload silently adopts a changed `policy-lock.yaml`, credential store, registry
cache, and a wholesale-replaced env override map. It *over-claims*:
`skip_auth`, `server.listen`, `database.url`, `mcp_servers` and `agents` are
baked at assembly, so `skip_auth: false` plus reload returns "reloaded" while
the daemon keeps admitting credential-less requests — reporting adoption there
converts a silent bug into a stated falsehood. And it does not fix the log. If
anything is reported, report facts: `{previous_digest, loaded_digest,
policy_digest}`.

§10.4 is **kept**, with a nullable `actor` and an optional `reason`.

Two defects to fix independently of the option chosen, both of which are holes
in routing-as-code:

- `substitute_env` has **zero call sites**, so the documented `${VAR}` rotation
  silently does not work for operator-written references.
- `set_env_overrides` **replaces rather than merges**, so which credentials are
  live depends on which terminal last ran `reload` — routing state that exists
  in no file.

---

## 19. Rejected alternatives

- **Daemon as server of record** (all four domains in the database, files as
  import/export only). Rejected: `Config` cannot round-trip, so the database
  copy would immediately diverge from the file people edit; and it discards the
  differentiator in §21.
- **Serializing `Config`.** Rejected on §2's three grounds — the emitted
  document differs from the written one, and every `api_key` renders as
  resolved plaintext.
- **The Serialize Line as the scope rule.** Rejected: it auto-approves every
  future daemon-invented store and can only refuse the one legacy artifact
  predating it. Demoted to a mechanism rule; §5 replaces it.
- **Scopes on `brvk_`.** Rejected on §8.1's two independent grounds.
- **Mounting the admin router the way `eval/api.rs` is mounted.** Rejected on
  §7.2's three verified grounds.
- **A `ControlTransport` trait behind `daemon::send_command`.** Rejected: the
  newline-JSON protocol is never carried over HTTP; one Router is served on two
  listeners and the legacy socket keeps lifecycle verbs.
- **Replacing the control-socket protocol in place.** Rejected on §7.1's
  upgrade-path grounds.
- **Connection-aware capability manifests keyed on peer address.** Rejected:
  behind a reverse proxy every caller presents as loopback (§8.3).
- **A typed admin handle passed into the chat module.** Rejected: it passes the
  guard by construction while making the rule false (§14).

Rejected **editions mechanisms** (§22), recorded here so the decision is not
relitigated by the next person who reads about GitLab's `ee/` directory:

- **An `enterprise` cargo feature.** Rejected: features are a *build*-time gate,
  the workspace's ~30 existing `#[cfg(feature)]` sites are uniformly additive,
  and `apps/bitrouter/Cargo.toml` has no `[features]` table at all.
- **An `ee/` directory.** Rejected: in Rust it cannot produce a closed tier — an
  unactivated optional git dependency still forces Cargo to resolve and fetch
  the private source, and an optional `path` dependency to an absent directory
  fails resolution identically. Every confirmed Rust open-core project publishes
  its paid source for exactly this reason, so `ee/` means *source-available*,
  not closed.
- **A `premium_user`-style license check.** Rejected as theatre. LiteLLM's own
  tracker documents `is_premium()` living in an MIT file, returning `false` with
  no cryptographic verification, alongside 19 "enterprise" features implemented
  entirely in MIT files and `organization_endpoints.py` — the tenancy surface —
  with no checks at all. Mattermost's embedded RSA check is defeated by a public
  repo that inverts one jump.
- **A `trait TenancyResolver` with a single OSS implementation.** Rejected under
  CLAUDE.md rule 4: a seam with no OSS consumer is dead code, and it would be
  defended in review by an argument no OSS test can refute — exactly the prose
  boundary §5's guard test exists to defeat.
- **An in-process enterprise daemon variant assembled from the published
  crates.** Rejected on verified facts: `async fn serve` is private inside the
  bin target, `Plugin` is `id`/`migrations`/`install` with no route
  registration, every hook trait is on the language-model and MCP pipelines, and
  `Router::merge` panics on a duplicate path — so it cannot mount an admin plane
  at all. §16 step 3 also serves `/requests` machine-wide, which a multi-tenant
  daemon must not expose.

---

## 20. Risks

| risk | mitigation |
|---|---|
| The repair listener turns a healthy start into a hang | It is new startup lifecycle code and must be the most heavily tested part of step 3; `start`'s readiness poll distinguishes degraded from ready. |
| The dry-assembly sandbox has side effects | It must suppress MCP child spawning and use a temp database, or validating a config becomes an action. |
| CI pipelines break on the sole-migrator change | The retained offline mode plus an explicit "run `bitrouter migrate`" error; step 1 stubs the contract that step 8 completes. |
| Compile in the serving process adds inference latency | The read-only pool ships in the same PR, not after. |
| The `api_principal` wire break strands live leases | Coordinated single-release change; leases are in-memory with a 12-hour TTL. |
| The scope vocabulary becomes an accidental roadmap | Answered: §8.2 declares the vocabulary closed and §22.1 states the ceiling; the `ADMIN_DOMAINS` guard test (§5) is the gate. |
| The published admin API specifies a reimplementation of the paid tier | Accepted, not solved (§22.4). OpenBao is the precedent. Mitigations are behavioural, and the never-gate promise is the insurance. |
| An enterprise tier is announced before it is buyable | §22.5 holds the public editions chapter until the product is downloadable; only the internal rule lands now. |
| Surface growth | ~15 inference-and-observability routes today; ~30 administrative ones added, plus a second listener, a credential class, a scope vocabulary, a host-pin table, an applicability table, an operations resource, and a fourth `Mode` state. Published here so growth is a decision, not a drift. |

---

## 21. The case against

Stated at full strength, because the owner is entitled to it.

**It trades the differentiator for a lane where BitRouter is weak.** The
README's competitive table sells routing policy as a lockfile in your repo —
readable, diffable, revertable, reviewed in a PR. No competitor has that. What
every competitor *does* have is a control plane: LiteLLM Proxy, Portkey,
Helicone, and Kong all ship admin APIs with teams, budgets, audit logs, and a
console. Ship ~30 admin routes and the comparison a buyer runs silently shifts
from four routers on routing rows to four gateways on management rows — and
BitRouter enters that comparison with no UI, no teams, no audit log, one daemon
per config, and a `users` table with two columns. This spec works hard to keep
the files authoritative precisely so the differentiator survives. The surface
still grows, and surface is what gets compared.

**The hazard is real but empirically quiet.** Two processes write one SQLite
file with WAL and a 5-second busy timeout between them; five CLI paths run
migrations unguarded; the policy publish transaction spans two processes with a
compensating write that can itself fail. Not one of these has a filed field
failure in the evidence gathered, and the spec's own claim about the worst case
is "brief write stalls, not corruption". This is a large, permanent, mostly
*preventive* investment against a class of bug that is theoretically live and
empirically silent.

**The goal as stated cannot be met.** The TUI half is ruled out by a boundary
that cost 19,955 lines to enforce. The self-improving loop is machine-local by
construction. What actually ships is *"a CLI can operate a remote proxy's
config, policy, keys, and history reads"* — genuinely useful and genuinely new,
but much narrower than "both front-ends expose the same actions, locally or
remotely".

**The cheapest alternative is not a strawman.** `ssh host bitrouter policy
publish` works today and requires none of this. A team with SSH and git already
has remote operation, with an authorization model (SSH keys), an audit trail
(shell history and git), and a disaster-recovery story (a shell) that all beat
a day-one admin plane.

**Opportunity cost.** Every hour here is an hour not spent on the adequacy
ledger, the compiler, and the routing quality that is the product's actual
claim. This plan makes BitRouter easier to administer. It does not make it
route better.

**One concession, named precisely.** §10.4 gives *config* changes an author and
a reason; policy — the domain the README actually sells — gets no equivalent,
and §10.4's own rationale (a daemon-written change landing in a git tree with
no author defeats the PR review gate) applies to it verbatim. That is the
specific place this design degrades the differentiator. It is fixed by §11's
writer class and an actor on publications, not by declining to ship.

**And the differentiator claim itself needs narrowing.** Kong decK already
gives declarative routing config in git with a PR-reviewed diff, so "routing as
code" alone is not unclaimed ground. What *is* unclaimed is the **compilation
step**: a machine-generated, certificate-backed policy artifact compiled from
your own admitted evidence, published by an explicit reviewed act, serving as
the sole live route authority. Add decK to the comparison set so the claim is
stated against the product that actually has the git half.

The counter to all of the above: steps 1–4 fix two live bugs and are worth
shipping on their own merits, and a management API is the seam that a GUI, a
hosted console, and a fleet-of-daemons story all eventually need — and, under
§22, the seam the enterprise product itself consumes.

---

## 22. Editions and the Tenant Line

BitRouter OSS is **single-tenant**. Self-hosted multi-tenancy is a closed-source
enterprise product. This chapter states the line, the topology, and the
commitments — it is the ceiling §18.2 ratifies.

### 22.1 The Tenant Line

> A capability belongs in the Apache-2.0 build if its specification is complete
> given **(a) one daemon** and **(b) a caller identity**. It belongs in the
> closed enterprise product only if its specification requires an **isolation
> boundary between groups of callers** — a group whose members can see each
> other's state and whose non-members cannot.

Two clarifications are part of the rule, not commentary on it.

**A scope narrows what one credential may do to one daemon's state; a role
assigns a person a capability over a partition.** `admin_keys` bearing
`config:write` is a capability-bearing credential and is OSS. "alice is an
Admin of org acme" is enterprise. RBAC is enterprise not because roles are
sophisticated but because *a role without a group is just a scope*.

**Grouping is OSS; only the visibility boundary is enterprise.** A shared budget
across a set of keys, or a label used for reporting, is complete given one
daemon and caller identities — as it is in LiteLLM (Teams, free) and Bifrost
(Customer→Team→VirtualKey budgets, Apache-2.0). Gating those would put the free
tier behind both.

**Single-tenant is not single-caller.** The OSS daemon issues N `brvk_` keys,
meters each separately, and enforces per-caller ceilings. See §18.2 for the
three per-caller integrity properties that are relabelled and never deleted.

This is the market's line, not an invention: Kong Workspaces, Vault Namespaces,
LiteLLM Organizations, Bifrost RBAC/SSO/SCIM, and Langfuse project-RBAC plus
its Org and Instance Management APIs all fall on the same side.

### 22.2 Never gated

Published as a commitment, in GitLab's stewardship form — *a feature that is
open source will not be moved to the enterprise product, whether BitRouter or a
contributor wrote it* — with one bounded carve-out: capability may be removed
for **correctness or safety**, against a stated bar (no test coverage, or an
invariant the build cannot enforce), **never for commercial reasons**. §18.5
and §18.6 are both argued that way deliberately.

The entire routing path: `policy-lock.yaml`, `deterministic_yaml`,
`semantic_digest`, the parent-digest CAS, `.bitrouter-policy-history/`,
`promotions.jsonl`, per-route certificates, `policy compile|diff|verify|publish|
rollback|evolve`, `optimize run`, the adequacy ledger, the eval exchange, and
hand-edit-plus-`reload`. Plus N `brvk_` keys with per-caller metering, ceilings
and rate limits; the whole `/v1/admin/{config,policy,keys,history}` plane for
one daemon; admin-plane OIDC/OAuth authentication; guardrails; observability;
MCP; ACP; the CLI and the TUI.

### 22.3 The seam: a closed control plane over an open data plane

The public tree gains **nothing**: no `[features]` entry, no `ee/` directory, no
`#[cfg]`, no license check, no `trait TenancyResolver`, no stub. §19 records why
each was rejected.

The enterprise product is a **separately distributed process that speaks the OSS
`/v1/admin` API to N unmodified single-tenant daemons**. It does not embed the
daemon, does not install hooks into it, and does not mount on the OSS admin
router. This is the Tyk, Kong Konnect, API7-over-APISIX, and Traefik Hub shape.

The coupling is a versioned HTTP API the OSS build needs for its own CLI, so it
is exercised by OSS tests. The tenant boundary is a **whole daemon** — one
process, one SQLite file, one config, one lock — which is stronger isolation
than a shared multi-tenant schema. There is no tenant-qualified `user_id`, so no
tenant-scoped compile, so **the compiler that produces `policy-lock.yaml` is
never forked** and an enterprise-compiled lock stays byte-reproducible by OSS
`policy verify` (acceptance #25). And there is no strict-superset free binary,
so the Apache-2.0 daemon remains the artifact everyone installs.

The SDK hook seam (`bitrouter-sdk/src/lib.rs:109-114`) is the **data**-plane
seam, with `bitrouter-cloud` as its consumer. It is unchanged and un-extended by
this decision.

If a license file is ever needed it belongs in the **control plane only** —
offline and signed, with Vault's precedence (`BITROUTER_LICENSE` →
`BITROUTER_LICENSE_PATH` → `license_path`), never a phone-home. Air-gapped
buyers are disproportionately the ones who want isolation.

### 22.4 Accepted exposure

Apache-2.0 gives no legal defence against a third party building the tenancy
layer, and under this seam BitRouter's own published admin API is that
reimplementation's specification. **OpenBao is the precedent**: its flagship
deliverable is a from-scratch, deliberately API-compatible reimplementation of
Vault's Enterprise-only Namespaces.

This is accepted rather than solved, because relicensing is the one move with a
proven fork blast radius and the Apache-2.0 badge is load-bearing for a router
whose pitch is "policy you own". The mitigations are behavioural: put the paid
value where reimplementation does not help (one identity plane across the fleet,
roles spanning daemons, one audit and retention surface, cross-daemon policy and
version governance, compliance attestation, support); note that the population
which forks is companies redistributing infrastructure, not users of a
laptop-run router; and hold the never-gate promise.

**Do not sell isolation as the headline.** A BitRouter tenant is one process and
one SQLite file, so N tenants is N daemons plus a proxy keyed on key prefix —
replicable in an afternoon, and §18.6 keeps Postgres for exactly the
orchestration substrate that makes it easy. **Meter the control plane, not the
tenant count.**

### 22.5 Publication timing

The Tenant Line (§22.1) and the never-gate promise (§22.2) cost nothing and
constrain BitRouter, so they land **now**, as an internal design-record rule.

The public editions chapter, the OSS-vs-Enterprise matrix, and the "which binary
is free" sentence **wait until the enterprise product is downloadable**:
publishing them early announces a tier nobody can buy, converts every unbuilt
feature into a deliberate withholding, and hands a reimplementation its
specification.

---

## 23. References

- [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) §11 — `_bitrouter/route/*`,
  the existing control surface and its capability-negotiation pattern.
- [`ACP_TUI_SPEC.md`](ACP_TUI_SPEC.md) §8.3 — the session-scoped rule and its
  violation trigger.
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace boundaries and the "may touch
  the terminal, may not draw on it" rule.
- [`CLI.md`](CLI.md) — the 103-leaf command surface and the output contract.
- [`crates/bitrouter-tui/src/lib.rs`](../crates/bitrouter-tui/src/lib.rs) — the
  honesty rules a control surface must obey.
- [`crates/bitrouter-mcp/src/server.rs`](../crates/bitrouter-mcp/src/server.rs)
  — `ensure_loopback_bind` and the deliberate HTTP fencing of local-only
  capability.
- `EVAL_LOCKFILE_SPEC.md` §3.1 — goals 5 and 6, "ship one lock", which §18.3
  and acceptance #25 preserve.

External, for §22 (retrieved 2026-09-04):

- GitLab stewardship and buyer-based open core —
  <https://about.gitlab.com/company/stewardship/> and
  <https://docs.gitlab.com/development/ee_features/>. Source of the never-gate
  promise's wording.
- Langfuse self-hosting licensing — <https://langfuse.com/self-hosting/license-key>.
  The clearest published OSS-vs-enterprise matrix in this market; note that SSO
  is free and only SSO *enforcement* is paid.
- LiteLLM licensing and the gate-leakage tracker —
  <https://github.com/BerriAI/litellm/blob/main/LICENSE> and issue #34241.
  Evidence for §19's rejection of a `premium_user`-style check.
- OpenBao Namespaces — the reimplementation precedent behind §22.4.
- Kong decK — the product that already has the git half of "routing as code",
  and the reason §21 narrows the differentiator claim to the compilation step.

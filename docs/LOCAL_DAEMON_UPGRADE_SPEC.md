# Local daemon handoff after a CLI upgrade

Status: **implemented and locally verified** · 2026-09-22

## Decision

When the installed `bro` binary needs capabilities the daemon serving its
selected local configuration lacks, or the daemon is an older binary, `bro` may
restart that daemon automatically **only after it proves the daemon is idle and
the installed binary can open and migrate the selected database**. The handoff
uses graceful shutdown and verifies the new process before reporting success.
If either proof is unavailable, the existing daemon keeps serving and the CLI
reports the reason and the explicit recovery action.

This is a local daemon lifecycle contract. It does not authorize a CLI to kill
an arbitrary process, edit migration history, or change a user's model route.

## 1. Problem and former behavior

An installed CLI and a resident daemon can have different binaries. The local
control socket previously returned a PID and routing status, but no daemon
binary version or protocol capability. A newer `bro` could therefore send a
command the older daemon could not decode and expose a raw `unknown variant`
error.

`bro update` delegates Homebrew, Cargo, and npm installs to their package
managers. Previously, a self-managed update restarted a running daemon only
with `--restart`, using endpoint presence as its trigger. An upgrade performed
directly with a package manager never reaches that dispatcher. `bro restart`
authenticates the daemon PID through the control socket, requests `Stop`, waits
for process and endpoint release, and launches a new daemon. The new daemon
runs pending SeaORM migrations during startup, after the old daemon has stopped.

The supervisor owns live ACP controllers. Its shutdown stops them, and its
restart ledger marks formerly live runs `Interrupted`; a running process with
an idle turn is still a live run. A transparent restart would break that run.

A second failure is possible when a development build has already applied a
migration absent from the installed release. Such a database is **ahead of that
binary**. There is no set of pending migrations the release can complete to
make the missing migration known. Removing a ledger row only hides the
incompatibility and may make a later migration fail against an existing column.

Current source anchors: [update.rs](../apps/bitrouter/src/update.rs),
[main.rs](../apps/bitrouter/src/main.rs) (`Command::Update`, `restart`),
[daemon.rs](../apps/bitrouter/src/daemon.rs) (`DaemonResponse::Status`),
[assemble.rs](../apps/bitrouter/src/assemble.rs) (migration on startup), and
[supervisor.rs](../apps/bitrouter/src/supervisor.rs) (shutdown and restart
ledger).

## 2. Scope and terms

- **Candidate binary:** the executable selected by the current invocation,
  after package manager or self-managed installation. Do not assume a package
  manager command succeeded merely because its process exited successfully;
  inspect the resolved executable and version.
- **Selected daemon:** the process authenticated through the selected config's
  local control endpoint and locator. A PID file alone is not proof of process
  identity or permission to stop it.
- **Compatible:** the candidate supports the daemon control protocol and the
  database's recorded migration lineage, and its pending migrations succeed in
  preflight.
- **Idle:** no starting, running, or stopping supervised run; no live external
  ACP controller or route lease; no accepted model request or other
  daemon-owned operation in progress; and no new work may enter between the
  idle check and shutdown. A detached or turn-idle agent process still makes
  the daemon busy. Unknown activity is not idle.
- **Automatic handoff:** the candidate itself performs the preflight, drain,
  graceful stop, start, and readiness check without a separate restart request.

The first implementation covers CLI-owned local daemons backed by a file SQLite
database. Externally supervised `bro serve` processes and PostgreSQL/MySQL
databases get a diagnostic and an explicit operator path until equivalent
ownership and migration preflight are implemented. This does not affect remote
contexts or a daemon selected with an explicit foreign socket.

## 3. User-facing contract

| Entry point | Behavior after a binary replacement |
| --- | --- |
| Bare `bro`, `bro code`, or another command that needs a compatible local daemon | Probe compatibility before sending feature commands. If the selected daemon supports safe handoff and is idle, perform it once; otherwise report why it was deferred. |
| `bro status` and other passive inspection | Never restart merely to answer a read. Report installed and daemon versions, compatibility, active-work state when known, and the next action. |
| `bro update` for a self-managed install | After installation, use the same safe handoff path by default for an already-running eligible daemon. Report binary and daemon outcomes separately. If no daemon was running, do not start one solely because an update finished. |
| `bro update` for Homebrew, Cargo, or npm | Continue to delegate installation. Do not modify or shadow package-manager files. The next compatible daemon-dependent `bro` invocation evaluates safe handoff; passive status can report it. |
| `bro code --no-start` | Do not start or restart the daemon. Return the compatibility diagnosis and explicit action. |
| Explicit `bro restart` | Preserve the existing operator-requested restart semantics, but preflight the candidate database and report live-run interruption before issuing `Stop`. This spec does not silently broaden an explicit restart into process killing. |

A deferred or blocked handoff must not print only `updated`, `running`, or
`ready`. Human output gives the daemon PID/version when available, the
candidate version, the reason, and an action. Update JSON exposes `status` for
installation, `daemon` for the handoff outcome, `daemon_reason` for a deferral,
and verified process fields when reachable. Status JSON exposes
`compatibility`. A command whose requested usable-daemon state was not achieved
exits nonzero. A delegated package-manager result remains `delegated`, not
`updated`.

## 4. Compatibility evidence

Extend the backward-compatible local `Status` response with optional daemon
binary version, a control-protocol range or capability set, daemon instance ID,
and a handoff capability. Missing fields on an older daemon mean **unknown**,
not current or idle. A version string alone is for explanation; the protocol
capability determines whether a command can safely be sent. Two development
builds may report the same package version while implementing different
commands.

Before automatic handoff, identify the exact candidate executable, selected
config identity, authenticated daemon PID, and ownership mode. The new daemon
must report the candidate version, a compatible protocol, a new instance ID,
the same selected configuration, and a ready HTTP/control endpoint. A socket
accepting connections by itself is not sufficient readiness.

An old daemon without the new idle-and-drain capability cannot be automatically
restarted. The client reports `handoff_unavailable_on_legacy_daemon` and an
explicit restart path. Neither a successful `Status` response nor OS process
inspection proves that it has no in-flight work.

## 5. Preflight and handoff sequence

1. **Serialize candidates.** Acquire one local handoff lock keyed by the
   selected configuration identity. Re-resolve the executable and daemon under
   that lock. A second CLI waits for the result rather than issuing another
   stop or racing to bind the socket.
2. **Validate configuration and migration lineage.** Resolve the same database
   URL the daemon uses. Reject unknown applied migrations, incompatible schema
   shape, unreadable database, missing candidate executable, and changed config
   identity. No migration ledger row is inserted, removed, or renamed as a
   generic compatibility fix.
3. **Test SQLite migration on a consistent snapshot.** Make a private SQLite
   backup (including WAL state), run the candidate's pending migrations on that
   copy, and check database integrity. Keep credentials and row content out of
   diagnostics. Failure leaves the old daemon serving.
4. **Enter a daemon-owned drain gate.** Atomically stop admission of new agent
   runs and HTTP work, inspect supervisor processes and in-flight operations,
   and either acquire a short-lived quiescence token or decline handoff. Work
   rejected during this brief gate receives an explicit retryable response. If
   busy or the token expires, release the gate and leave the daemon running.
5. **Recheck and back up the live database.** Under quiescence, confirm the
   authenticated PID/instance, config identity, migration state, and candidate
   executable still match preflight. Create a protected recovery backup before
   any live migration. Abort and reopen admission on change or backup failure.
6. **Graceful handoff.** Send `Stop` to that same daemon, wait for its exact PID
   and endpoint to release, then start the candidate through the normal
   CLI-owned launcher. Never use a PID-file-only signal, force kill, socket
   unlink, or overlapping bind as an automatic fallback.
7. **Verify outcome.** Require the new daemon's version, protocol, instance,
   config identity, control socket, HTTP readiness, and migration state to
   match the candidate. Report the installed binary and daemon states
   separately. No successful `restarted` report is emitted before this check.

If startup fails after `Stop`, report that service is down, the failing phase,
the log path, and the backup path. Do not automatically start the old binary
after a live migration may have changed the schema. Recovery is explicit and
depends on whether migration committed. Backup creation must not imply that an
automatic restore is safe when requests or other writers may have run since
the snapshot.

## 6. Migration lineage and the observed `000022` case

Forward migrations already belong to the new daemon's startup. The upgrade
work adds validation and a safe handoff around that mechanism; it does not
create a second migration runner that races with the daemon.

For a database recording `m20240101_000022_add_upstream_account_ref` while the
candidate ends at `000021`, preflight reports `database_ahead_of_binary`, names
the missing migration, and leaves the old daemon running. The proper release
repair is to ship the same migration lineage, or an explicitly reviewed
reconciliation that checks the existing column's shape and preserves populated
values. The earlier local removal of the `000022` ledger entry is a temporary
compatibility workaround, not precedent for updater behavior. A candidate that
tries to add the existing column must fail preflight on the snapshot before
stopping the daemon.

Migration validation and backup are backend-specific. Until equivalent
non-destructive checks exist for PostgreSQL and MySQL, those backends cannot
enter the automatic path. An operator may still use an explicit maintenance
window and the normal startup migrator.

## 7. Failure and concurrency rules

| Situation | Required result |
| --- | --- |
| Same-version compatible daemon | No handoff; continue immediately. |
| Candidate newer, daemon idle, preflight passes | One graceful handoff; verify candidate is serving. |
| Running or starting supervised agent, including a detached agent | Defer; no stop and no interruption. Show run count and explicit restart impact. |
| In-flight model request or other accepted operation | Defer or wait only within a bounded drain attempt; do not terminate it. |
| New work races with the idle check | The daemon-owned gate serializes it; either handoff is declined or the request receives a retryable response. |
| Legacy daemon cannot prove idle or accept a drain gate | No automatic restart; show compatibility failure and explicit recovery. |
| Applied migration absent from candidate, or snapshot migration fails | No stop and no ledger edit; report schema incompatibility. |
| Another CLI wins the handoff lock | Re-probe after it finishes; do not repeat the restart. |
| Candidate fails after the old process stops | Do not claim success or blindly roll back; report service down and recovery artifacts. |
| Externally managed daemon or remote target | No automatic takeover; use its owner/service manager. |

## 8. Acceptance criteria

1. Reproduce a newer client sending an unsupported `sessions` command to an
   older daemon. The user gets a version/capability diagnosis before that
   command, not Serde's `unknown variant` message.
2. With an eligible idle daemon and a migratable SQLite database, one CLI
   invocation performs one handoff and reports the new PID, version, instance,
   endpoint, and successful migration state.
3. A detached idle-turn agent, an initializing controller, a pending
   permission, an external ACP route lease, or an active model request prevents
   automatic restart. The run remains controlled and its native session is not
   silently interrupted.
4. Unknown activity, a legacy daemon, or a missing protocol field never
   becomes `idle` by default.
5. An unknown applied migration and a duplicate-column migration both fail
   preflight while the old daemon remains reachable. The preflight makes no
   writes to the live database or its migration ledger; ordinary daemon work
   may still change them while it serves.
6. Two concurrent CLI processes cannot stop two different PIDs or launch two
   daemons for the same configuration.
7. A config/socket change between preflight and quiescence aborts the attempt
   without taking over a different process.
8. A failed post-stop launch is reported as a partial upgrade with log and
   backup paths, never as `daemon: restarted`.
9. `bro status`, `bro update --check`, `--no-start`, package-manager delegation,
   and remote contexts preserve their read-only or externally owned behavior.
10. Tests cover the daemon control handshake, drain race, migration snapshot,
    restart state machine, and truthful JSON/human results. Real-process
    acceptance uses the actual old/new binary boundary and a SQLite database
    with both pending and unknown-applied migration cases.

## 9. Review decisions

1. **Default after self-managed update:** this spec selects safe automatic
   handoff when a daemon is already running. A product policy could instead
   require an explicit `--restart`; the same preflight and diagnostics apply.
2. **Busy daemon UX:** this spec defers immediately rather than waiting for a
   potentially long agent run. An explicit restart remains available with a
   clear interruption warning.
3. **First rollout limit:** legacy daemons cannot provide the required drain
   proof, so the first transition from a pre-capability release requires one
   explicit restart. Automatic handoff begins once both sides speak the new
   contract.

## 10. Local verification

- `cargo nextest run --all-features --no-fail-fast --status-level fail`: 3,528
  passed, 22 skipped.
- `cargo clippy --all-features`, `cargo fmt -- --check`, and
  `git diff --check`: passed.
- Isolated real-process E2E: unknown applied migration and active ACP recording
  left the old PID serving; two concurrent clients produced one handoff and one
  backup; a pending migration applied on replacement; a foreground externally
  managed daemon was not taken over. The old-process fixture was an ad hoc
  signed copy of the candidate with an older version tag, so release-to-release
  packaging remains a separate check.
- An installed legacy daemon lacking the handoff command returned a clear
  compatibility error while its PID remained serving; the raw Serde error was
  not shown.

Implementation lives in `daemon_handoff.rs`, `upgrade.rs`, and
`upgrade_preflight.rs`. The first transition from a daemon without the handoff
protocol still requires an explicit restart.

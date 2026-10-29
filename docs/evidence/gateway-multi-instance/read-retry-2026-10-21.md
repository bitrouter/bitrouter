# Shared record read retry follow-up

Runtime repair: `322f3d87813082085c23389976897207268cfbde`.
Status: independent source review and final local validation passed. Hosted
verification is tracked on [#990](https://github.com/bitrouter/bitrouter/pull/990).

## Hosted failure and scope

The [packaged PostgreSQL job](https://github.com/bitrouter/bitrouter/actions/runs/39567002331/job/119124878519)
at `febe10751be548ddd4e10f849ed8cfb3ce82d98c` failed while the guard campaign's
observer read a projection receipt. Its stack was `Store::read<Value>` →
`Store::get` → `guards::delivered`, with a PostgreSQL serialization abort.
The selection stopped after 7 passes and 1 failure; 19 of 27 cases were unrun.
This failed run remains evidence and is not counted as a packaged acceptance pass.

The observer had already verified successful public execution state and matching
peer facts. It does not project or acknowledge the receipt. The failure does not
establish failed provider execution or recurrence of the earlier guard-fence
defect. Public `Engine::query` already retries its complete authenticated
transaction. The missing retry was in the separate pooled `Store::get` helper,
also used by production owned-execution, attempt-interruption and authority-head
reads.

The one-line repair wraps `get`'s complete begin/read/commit operation in the
existing transaction retry helper. Each retry creates a fresh Serializable
transaction. The existing single total deadline, server timeouts and typed
PostgreSQL `40001`/`40P01` allowlist are unchanged. Connection errors, ambiguous
commit outcomes, decoding errors and other failures are not retried. Borrowed
reads do not retry someone else's transaction; no provider I/O is repeated.

## Deterministic regression

The regression creates its own PostgreSQL database and one registered pool
connection, seeds the actual migrated schema, then replaces only that database's
record relation with a view over a volatile function. The first actual `get`
SELECT raises `40001`; a sequence survives the abort and counts attempts. The
test calls `get` without a fixture retry and requires exactly two SELECTs plus
identical returned row metadata and decoded value. It also checks Serializable
isolation. This injects a real SQLSTATE; it does not claim to reproduce a
naturally scheduled SSI cycle. Ownership and bounded cleanup use `EmptyDatabase`.

- Old production: the new test failed with the intended first-SELECT abort in
  0.189s (0.208s summary), rather than a prerequisite or cleanup failure.
- Repaired production: all three Store tests passed in 0.455s, with zero retries.
  The new `get` test took 0.197s; the existing error-allowlist/deadline and late-poll
  regressions also passed.
- Independent review approved the exact two-file patch, including transaction
  scope, cancellation, retry classification and fixture cleanup. No process-test
  assertion, timeout, or whole-test retry policy was changed.

## Final validation

The immutable v10 `gateway-shared` executable has SHA-256
`13062838c63d0ce99e012c58810c99b923a95c425c89c4ddcefb4ff036bd8211`.
The complete shared selection passed **204/204** in **88.976s**. The original
guard campaign passed **3/3** fixed confirmations (2.219s, 1.848s, 2.171s), with
zero retries. Full workspace tests passed **4,939/4,939** in **662.705s**,
with 148 configured skips and 0 nextest output-handle notices. Strict workspace
Clippy and formatting passed. The owned PostgreSQL fixture was stopped before
workspace validation. [Machine-readable evidence](read-retry-2026-10-21.json)
records source, artifact identities, commands, counts and log hashes.

The prior CI's other 67 checks passed, including Linux library 3,697/3,697,
Windows 4,771/4,771 and macOS 4,939/4,939, without retry/failure/leak entries in
those logs. Those results remain attached to the failed prior head and do not
substitute for repaired final-head CI.

The production change is entirely inside `gateway::shared`, which is compiled
only with `multi-instance`. Neither local default/full nor minimum enables that
feature, and no manifest or dependency changed. The original matched local
performance evidence remains explicitly attributed to runtime `2495bf8d`;
it is not relabeled as a new measurement. Its recorded costs and follow-ups
remain applicable to that unchanged local implementation.

A rebuilt executable may embed a new source/build identity. The scope argument
above does not establish byte-identical binaries or newly measured sizes, RSS
or throughput.

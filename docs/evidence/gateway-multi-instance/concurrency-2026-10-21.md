# Shared concurrency follow-up

Production repair: `762d80a9f6299b9e0b026396fcbeff52b2e8b1af`.
Test-only helper commit: `a83c7bc0505053cd11a4ac3485ff441b3a19e214`.
Status: implementation and local acceptance complete. Hosted final-head checks
remain tracked on [#990](https://github.com/bitrouter/bitrouter/pull/990); no
hosted final-head pass is asserted here.
This record supplements the [v10 read-retry result](read-retry-2026-10-21.md).
[Machine-readable evidence](concurrency-2026-10-21.json) preserves source,
artifact, log and patch fingerprints without depending on local file links.

## Hosted continuation failure and isolated fixtures

Head `f7d8d2811e5ac3b2f53277e7c9eaf73544e4179f` finished with **67 successful
checks, eight skips and one failure**.
The [hosted run](https://github.com/bitrouter/bitrouter/actions/runs/39577338530)
passed all **27 packaged-process cases in 71.398s**. Its later non-process
PostgreSQL selection stopped at **31 passed, one failed, 42 unrun of 74**.
`close_waits_for_accepted_bind_and_prevents_late_bind` failed in 0.250s with a
serialization abort; the server recorded a pivot at COMMIT. This is separate
from the previously repaired pooled-read path and does not establish a provider
execution failure.

The continuation fixture used two pools on one shared test database, with only
namespace keys separating campaigns. Cross-campaign SSI is a source-supported
explanation; the exact historical conflict graph is unknown. One unchanged
baseline passed **74/74 in 26.541s**, so no reproduced continuation red is claimed.
Test-only commit `12f75d7df9e020f21a2892ff3f1e5feb0e89fd22` gives each campaign
its own confirmed-created database while retaining both contending pools inside
it. Bind/close operations, assertions, isolation, budgets and retry policy are
unchanged. All five cases passed in v11, then three fixed confirmations passed
**15/15** (0.855s, 0.864s, 0.845s per five-case set).

## Metering replay repair

The v11 aggregate at `12f75d7d` ran all 204 cases: **203 passed, one failed in
92.471s**. All 27 stock-process cases passed. The separate
`postgres_terminal_projection` failure (0.547s) was an event-ID unique violation.
That coordinator already owned an isolated database; unrelated baseline reuse
does not explain it.

`SharedMetering::apply_in` arbitrated only `(namespace, execution_id)`, although
the table also has unique event and request identities. Commit `762d80a9` uses
targetless `ON CONFLICT DO NOTHING`, covering every usable unique constraint.
The unchanged owner-qualified read, decoded accounting validation and exact
record/body comparison still reject reused identities, foreign owners and
changed evidence. [PostgreSQL conflict semantics](https://www.postgresql.org/docs/17/sql-insert.html#SQL-ON-CONFLICT).
No uniqueness is removed, prior row overwritten, or `23505` retry added. Existing
whole-transaction retries remain limited to typed `40001`/`40P01` aborts under
the original deadline.

Four independent writer pools exercise exact concurrent replay for unbound and
bound requests, followed by event/request collision, owner and immutable-content
rejections. The old-production run hit the actual initial-writer event-ID
`23505`, not merely a negative case's error-string mismatch. It failed in
5.747s total, **with a separate cleanup failure**: two sessions prevented DROP
of confirmed-owned `bitrouter_fixture_bae818a405a34633b1958ddca14eb8ec`.
A later check found zero sessions, and an ordinary DROP removed that exact
confirmed-owned database without forced termination. The cleanup log records
zero rows and the successful command tag; it does not echo the SQL target. This resolves
the stranded resource without relabeling the original run as clean red.

With the SQL fix and unchanged regression, the new replay, original coordinator
and existing metering campaigns passed **3/3 in 3.360s**, including ordinary
cleanup (individual times 0.318s, 1.507s and 3.345s). This is the focused green
for the source-reviewed production change, not aggregate acceptance.

## Pool-close regression

Independent source review identified a SQLx 0.8.6 close-completion gap: a return
future can pass the closed check, await, then enqueue a connection after close's
initial drain. A one-connection close can finish after acquiring the released
permit without draining that late entry. See the pinned [close/release source](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-core/src/pool/inner.rs#L97)
and [return path](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-core/src/pool/connection.rs#L275).
Commit `a83c7bc0` changes only the test helper: require zero
tracked pool connections within the existing close deadline, and add a controlled
late-return regression. It adds no DROP retry, database polling, forced backend
termination or production change. Client pool size is not a server-exit
acknowledgement. The original two sessions remain unidentified.

The controlled test with the old close helper failed at the intended assertion:
**one live pool connection**, 0.156s test / **0.175s summary**, with 3,820 cases
outside selection and no cleanup failure. Its test-only source SHA-256 is
`3bc2147159ce7359e72b7216942b33bf2d24777a42788f9de99d2b5f5c9c38b5`.
The repaired helper and three metering/projection campaigns then passed **4/4 in
3.525s**, including ordinary cleanup (close 0.192s, projection 1.609s, new replay
0.319s, existing metering 3.318s; 3,817 outside selection). These results confirm
the controlled helper interleaving; they do not identify the historical two
sessions or make the earlier metering red cleanup clean. The exact reviewed
helper source `0da43799…` is committed as `a83c7bc0`.

## Final local validation

The immutable v12 `gateway-shared` executable identifies source
`a83c7bc0505053cd11a4ac3485ff441b3a19e214` and SHA-256
`0ecce674a744b74658311d39b192997b5a38385dc31cd1ffa6386b28dec6cb0f`.
The shared selection passed **206/206 in 96.096s**, with 3,615 outside selection,
two test threads and zero retries. It includes all 27 stock-process cases, the
original continuation/projection failures, the new replay regression and the
controlled pool-close test. This completes the renewed local A04 evidence.

The production-source `762d80a9` workspace passed **4,939/4,939 in 666.384s**
(five slow, 149 skipped). Database environment variables were unset while the
owned PostgreSQL fixture remained running and idle. This was not a
PostgreSQL-absent or performance run. That completed workspace result retains
its exact source; the later helper commit changes only test code, exercised by
the v12 aggregate, so no duplicate workspace test run is claimed.

After the v12 aggregate, the owned PostgreSQL fixture was stopped normally and
database/binary fixture variables were unset. Strict Rust 1.97 workspace
Clippy (`--all-features --tests -- -D warnings`) passed in **1m09s**. Formatting
and whitespace checks passed. Existing dependency future-incompatibility and
Darwin linker notices remain in their original logs; no test retry or larger
budget was used to conceal a failure.

| Check | Source-attributed result |
| --- | --- |
| Production workspace | `762d80a9`: 4,939/4,939, 666.384s |
| Controlled helper regression | Intended size-one red with clean teardown; repaired focused selection 4/4 |
| Immutable shared artifact and aggregate | `a83c7bc0`: recorded artifact; 206/206, 96.096s |
| Strict workspace Clippy, format and whitespace | `a83c7bc0`: PASS |
| Hosted final head | Tracked on PR #990; pending final-head result |

Final evidence review is tracked separately against the frozen documentation
snapshot. Hosted status is maintained on the PR rather than requiring another
documentation commit after CI.

No new performance measurement is claimed. All 27 local samples and their
recorded costs remain attached to `2495bf8db118de399a11fc11f82ba18bd6a6455d`.
The metering production edit is gated by `multi-instance`, which the measured
default/full and minimum profiles exclude; the other changes are test-only.
No manifest or dependency changes, byte-identical rebuilt binaries, or new size,
RSS or throughput results are inferred.

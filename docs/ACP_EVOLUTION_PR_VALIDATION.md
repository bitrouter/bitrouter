# ACP evolution: main integration validation

Recorded September 12, 2026 (Asia/Shanghai). The feature was integrated with main
`23cc1644` (`bro` invocation and native terminal scrollback).
The final code/fixture source is `85c69d35`. The
[machine-readable receipt](experiments/acp_evolution_pr_validation_20260912.json)
records source hashes, test scopes, prior failures and local log hashes.

## Integration changes

- Keep the `bro` CLI and shipped session reference in sync with main.
- Render multiline rubric applicability and scoring anchors in the native dock,
  retaining filter, selected score and confirmation hints at 40×16, 80×24 and
  120×40. Preserve the nested history-inspector return path in terminal tests.
- Update opt-in worker fixtures for native permission/selector borders and
  explicit concurrent-snapshot retry results.
- Drain an in-flight judge lease renewal when model evaluation completes.
  Dropping that database operation could discard the sole SQLite memory
  connection, causing later checkpoint reads to report a missing table. The
  initial lease is already fresh, so the first heartbeat waits 30 seconds.
  The regression holds the only connection while stopping renewal, then checks
  that the update completes and the checkpoint database survives.

## Validation

| Check | Result |
|---|---|
| All-feature regular suite | 3,395 passed; 22 skipped |
| Judge job regression group | 9 passed |
| All-target, all-feature Clippy with warnings denied | Passed |
| Doc tests | 5 passed; 1 ignored |
| Formatting and diff whitespace | Passed |
| Maintained Codex and Claude terminal checks | 4 passed; no leaked-handle warning |
| Pinned fixture worker cleanup | No worker remained |

The regular suite and doc tests ran at `a22024dc`. The only later source
change is the opt-in worker fixture's handling of an explicitly retryable
concurrent assessment snapshot. The final native checks and Clippy include it;
production code is identical. Loopback tests use
`NO_PROXY=localhost,127.0.0.1,::1` and the lowercase equivalent to bypass the
operator's system proxy.

The native check command is:

```sh
cargo nextest run --all-features --no-fail-fast \
  -E 'test(evolution::native::) & !test(tui_coding_publication_and_rollback)' \
  --run-ignored all --success-output immediate
```

It requires the pinned worker/adapter paths described in the
[experiment report](ACP_EVOLUTION_EXPERIMENTS.md). Both workers wrote code and
recorded executed tests; both feedback flows recorded automatic scoring, a
manual correction, candidate dispatch, operator withdrawal and actual baseline
dispatch while Automatic. These checks use fixed local upstream replies and
labels and do not spend subscription inference credits.

Earlier failures are retained in the receipt: terminal assertions tied to the
old layout, loopback proxy interference, the lease cancellation defect, and a
fixture that failed to retry a concurrent snapshot. An initial Codex trial also
hit a comparable-learning timeout; later complete runs passed. That timeout is
not presented as a proven eliminated timing issue. The regression's initial
timer-boundary assertion was corrected to advance beyond the rounded deadline.

## Experiment scope

The [earlier goal audit](ACP_TS_GOAL_AUDIT.md) and its two full adoption,
monitoring and rollback campaigns remain historical results with their original
source hashes. Those longer campaigns were not rerun on this integrated main
base. The seeded learner, rubric and evidence contracts are unchanged; judge
lease lifecycle and terminal integration changes are validated above.

The actual subscription pilots contain constructed coding tasks and fallible
model references. They are separate from fixed-response native acceptance and
from the seeded controller simulation. None establishes natural-history
calibration, TS superiority or live multi-model savings. The simulation retains
its negative findings, including harmful adoption after prolonged incorrect
provisional labels. Raw local logs and private traces remain outside this repo;
the committed receipt contains summaries and provenance hashes.

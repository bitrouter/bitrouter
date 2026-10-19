# Native queue test migration

The Goal composer accepts current-Goal inputs during execution. It does not
inherit the retired Code interface's busy-Enter FIFO behavior. The native
Thread API retains its own durable FIFO contract. Three old product UI tests
are replaced with actual IPC/native-runtime checks, using an isolated daemon,
real SQLite state, real model HTTP traffic and actual workspace files. Provider
outputs are fixed fixtures, not a real-model-quality evaluation.

| Retired product test | Replacement |
| --- | --- |
| `code_queues_fifo_work_and_drains_after_each_normal_settlement` | `native_api_queue_drains_in_exact_fifo_order_after_normal_completion`: the first Turn waits for real write approval; two exact follow-ups are admitted while it is active. Repeated admission returns the original identity/order. After approval, both complete in FIFO order, with captured HTTP user messages exactly first/first/second/third and no extra generation. The second message preserves leading/trailing spaces, Chinese/emoji and newline. Queue empties only after completion. |
| `code_cancellation_pauses_queue_without_replaying_uncertain_work` | `native_api_cancel_pauses_fifo_until_explicit_resume_without_replay`: cancellation before approval produces no file, preserves both queued IDs/order, and pauses the Thread. Repeating cancellation and reconnecting observers cannot start a model. Only explicit ResumeQueue runs the follow-ups; captured messages prove the cancelled first input is not replayed. |
| `code_abnormal_stop_pauses_queue_without_draining_it` | `native_api_refusal_preserves_paused_fifo_without_replay_or_drain`: after the actual approved write, the provider returns a refused/content-filter finish. The first Turn fails, both queued Turns remain queued and the Thread pauses. Reconnected observers do not call the provider; captured traffic contains exactly the original first input twice, with no replay or queued dispatch. The already completed file effect remains intact. |

A retained raw ACP process test also verifies that `stopReason=refusal`
answers the original prompt ID exactly once without changing/replaying its
text. Its `DelayedRefusal` scenario and control helper stay in use. Other native
session, permission, cancellation and background protocol tests remain.

## Observed evidence

The first queue test compilation failed with E0599 because local `Operation`
does not implement Clone. Tests now construct the same exact RPC values through
closures; the production protocol was not extended merely to satisfy a test.
No tests ran in that failed build.

Before retiring the three old entry tests, all three new native API cases
passed: run `0af60b59-bdf0-408d-b1b5-b81fa90b8504`, exit 0, 13.668 seconds.
The actual runtime mapped the refusal to a Failed Turn and Paused Thread; the
test does not substitute recovery/unknown outcomes for that observed contract.

Final broader regression, 98-file source identity, UI state, Clippy, doctests
and formatting are recorded in the implementation ledger. The three remaining
root legacy UI cases concern detached backlog, native background isolation
and navigation output; opt-in native/evolution old paths still need migration.
These partial gates do not prove full workspace or complete MVP acceptance.

Final app regression: 56 passed, exit 0, run
`8a0f01fc-5e2e-42cb-b2ca-6247730717fa`, 103.348 seconds; 2209 filtered out.
Goal state regression: 22 passed, exit 0, run
`3b95a0fa-9b60-4b05-af67-9bb0e8405b10`, 0.029 seconds; 265 filtered out.
Workspace/all-target/all-feature Clippy with warnings denied, formatting,
diff and 98-file source checks passed. Separate workspace doctests:
6 passed, 1 ignored, exit 0. All checks ended without owned test processes.

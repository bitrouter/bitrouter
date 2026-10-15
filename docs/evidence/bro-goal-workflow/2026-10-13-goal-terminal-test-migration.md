# Goal terminal test migration

The old `code <agent>` product entry is deliberately removed by PRD v0.4 and
spec v0.2. Native harness interaction, headless ACP, transparent ACP stdio,
native Thread APIs and background sessions retain their contracts. This map
replaces five obsolete UI entry tests only after their replacement behavior is
observed. No native queue or protocol test is removed in this slice.

## Replacement evidence

The Goal tests use the actual CLI, daemon, database and PTY. Active workflow
tests use Cargo-built ACP processes and actual files; model judgments remain
fixed fixtures, not real-model-quality evidence.

| Removed old product test | Required behavior and current replacement |
| --- | --- |
| `code_minimal_agent_preserves_multiline_prompt_history_and_terminal` | Current-Goal paste does not submit; explicit Enter persists exact leading/trailing spaces, CJK/emoji and newlines. Up/Enter submits the same original message under a distinct operation identity while both workers run. Covered by `active_goal_enter_permission_readonly_workflow_and_detach_use_actual_execution`. New Goal paste/draft preservation is covered by `bare_and_code_open_goals_without_work_and_preserve_unsent_drafts`. Direct harness prompt bytes remain separately tested by retained `mock_acp_minimal_lifecycle_logs_exact_multiline_prompt_text`; the orchestrated Goal input does not pretend to be a raw worker prompt. |
| `code_uses_normal_buffer_until_an_explicit_inspector` | Bare/code, current Goal and default Workflow use normal scrollback; only explicit fullscreen inspection enters alternate screen. Returning restores the original draft and current Goal scope. Covered by both Goal entry and active Goal PTY tests, including raw alternate-screen sequences and actual mode cleanup. The removed `/open transcript` UI command is replaced by explicit Workflow/detail/fullscreen navigation. |
| `code_agent_deck_expands_inline_and_preserves_multiline_foreground_draft` | Agent deck is replaced by Goals and read-only Workflow. Each Goal keeps its own unsent multiline draft; browsing or changing observation does not submit it or change permissions/control. Covered by the Goal entry test's separate Goal drafts and the active Goal test's live worker/permission checks. The native/background APIs remain separate. |
| `code_sigint_restores_terminal_and_shell` | Actual SIGINT goes to the recorded BRO child PID, not its supervising shell. BRO exits successfully; modes, paste and alternate-screen state restore; the same shell accepts and prints exact leading/trailing spaces and Chinese input. Covered by Goal entry PTY, with both inherited-default settings. Signal exit does not delegate an unsent draft. |
| `code_sigtstp_restores_then_sigcont_reacquires_terminal` | Actual BRO PID stops after restoring terminal custody. PTY is resized while stopped, then SIGCONT reacquires session keys and restores the original draft. Exit subsequently returns to a working shell. Covered by Goal entry PTY for bare/code and both default settings. |

The Goal PTY wrapper starts `/bin/sh` first and passes executable/arguments as
argv without interpolation. It records `$!` in a private test file; signal
tests inspect and signal that actual BRO PID. The shell waits for BRO, records
its exit status, then reads a real canonical-input line and prints it unchanged.
Both BRO and shell must finish, and the output reader must close. Terminal
configuration comparison preserves all raw fields except the separately
documented macOS kernel `PENDIN` state. The existing 10-second cleanup deadline
is retained for the complete exit/shell/reader sequence.

The first attempt tried to start a shell on the original slave after BRO, as
session leader, exited. All three tests failed with EBADF. A focused second
run confirmed failure at shell spawn. The shell-first wrapper passed all three
tests in run `d5c37b85-8cba-4495-ade5-a0e8b7b061ef`, exit 0, 93.268 seconds.
That proof preceded retirement. Final source adds the shared deadline and
removes the five tests plus their now-unused signal helper; its regression and
static checks are recorded in the implementation ledger.

Final terminal-migration v2: app regression 52 passed, exit 0, run
`9a88817b-946d-4112-992f-4505de5dce0f`, 113.437 seconds; Goal UI state 20 passed,
exit 0, run `299cb8b9-bd4f-4c72-9a37-7e074d299a6f`. Workspace/all-target/all-feature
Clippy with warnings denied, formatting, diff and 98-file identity checks
passed. Separate workspace doctests: 6 passed, 1 ignored, exit 0.

App v1 initially failed its retained standalone `agents/attach` check: all
`stty -g` fields matched except `lflag=5cb` versus `lflag=200005cb`. The verifier
now requires exactly one parseable macOS local-flags field and normalizes only
the already documented `PENDIN` bit. Every other field and local flag stays in
the exact comparison; it does not modify terminal settings. V2 runs with
`--no-fail-fast` and includes both retained standalone modes. This is separate
from the five obsolete entry tests retired above.

## Scenarios deliberately still retained

`code_detached_backlog_catches_up_once_after_resizes`,
`permission_arriving_in_inspector_needs_slash_and_fresh_selection`,
`code_minimum_and_below_minimum_permission_paths_are_safe`,
`background_history_is_only_rendered_in_explicit_inspector`,
`code_agents_navigation_keeps_normal_buffer_and_defers_foreground_output`, and
the three FIFO/cancellation/refusal tests still require complete replacement
evidence or migration to their retained API. Their old entry currently prevents
final full-suite success. They are not ignored, weakened or removed here.
Opt-in native/evolution fixtures that still call the retired Code entry also
require migration; the list above refers to the root file's eight cases.

In particular, a single pending permission in the active Goal PTY is not proof
of two simultaneously pending Goal permissions, and existing state/API checks
do not by themselves prove catch-up without duplicate native scrollback output.

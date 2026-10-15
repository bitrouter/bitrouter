# Retired Code entry: maintained-session migration

Six opt-in native evolution scenarios previously invoked the removed
`code <agent> --model ... --no-start --turn-timeout 30` entry and waited for
retired composer/palette states. They now use the existing owner-scoped session
API for actual ACP start, prompt, fresh-session selection, explicit one-time
permission and stop. Evolution mode, candidate registration/restoration,
status and manual rubric preparation/submission use the production CLI.
No hidden old Code entry or writable Goal inspector is restored.

The feedback, coding checkpoint and automatic publication/quality rollback
scenarios each retain their Codex and Claude variant. Actual request identities,
executed Rust test observations, artifact bytes, assessment history, candidate
dispatch, baseline restoration, full cost membership and publication guards
remain assertions. Publication still uses the production default bandit,
16-session batches, at least 20 samples per arm, up to 512 trial arrivals,
12 monitoring arrivals and its original 2,400-second campaign bound.
Automatic feedback/learning runs through the existing scheduler. Tests do not
plant learner/store rows or manually accelerate publication.

Only unused bracketed-paste/old-composer helpers were removed from the old test
fixture. Goal PTY paste/navigation coverage remains in `goal_tui_pty.rs`.

The first migration compile emitted unused-method warnings; the initial
module-path formatting error was corrected during that attempt. Its ordinary
suite ran 30 cases successfully with six opt-in skips, but is not an immutable
source claim. V2 removes the unused helpers and adds a normal regression for
actual CLI receipt, prompt settlement, fresh native identity and explicit stop.
V2 run `5ad3fad6-10fd-42f7-958c-ccfdf2c3f7bf` passed 31 ordinary cases,
six opt-in skips, 86.679 seconds, two slow cases. Its 105-file source manifest
also covers the publication file, previously absent from the Goal manifest.

Maintained adapter identities are checked against the repository's pins:
Codex ACP 1.10.0 and Claude Agent ACP 0.75.1. Local actual workers report
Codex CLI 0.162.1 and Claude Code 2.1.284. All worker homes/configuration and
temporary files are isolated. The pinned Claude package was installed into
`/tmp/bro-evolution-claude-0.75.1`; no global package was replaced. Model traffic
and judge labels use the existing local controlled HTTP responders, not user
provider credentials. This proves harness/runtime mechanics when the cases
pass; it does not prove real-model quality or the Goal input evaluation gate.

V2 maintained run `ef9fa973-5972-4bc3-a322-4a822430793c` ended with one pass
(Claude coding checkpoint) and three failures, exit 100, 121.502 seconds.
Codex coding lacked the expected reply; Claude feedback encountered mixed
gateway resource revisions; Codex feedback was not enrolled into the trial.
The diagnostic-only V3 retained actual events: Codex coding received HTTP
`model_conversion_incompatible`; Codex trial admission was
`session_already_started`, after the worker emitted a missing-model-metadata
warning for the old `gpt-5.2` fixture selector. Claude feedback passed V3.
These runs are not successful migration acceptance.

The local isolated actual Codex `model/list` probe reports `gpt-6.1-sol` as its
known default. V4 uses that known selector and provides native Responses to
Codex strong/cheap routes, keeping judge on Chat and Claude on its prior wire.
The responder retains namespace-qualified function calls, actual
`function_call_output` continuation and standard Responses SSE/usage. It does
not remove opaque tools, disable worker capabilities, downgrade the worker or
change the production conversion/admission guards. The resource-read retry
waits within the existing deadline for scheduler refresh; it does not invoke a
model, synthesize feedback or change observations.

Source review also distinguishes `RoutingOptions.model` (worker configuration)
from `LaunchOptions.required_session_route` (an explicit route lease). This
fixture does not set a required route or send RouteSet/RouteReset to force an
allocation; real applied route leases retain precedence over evolution.

V4 run `9dd59950-7d02-4748-869e-2e9060d63742` ended with three passes and one
failure, 126.318 seconds: both maintained feedback/trial cases and Claude coding
passed, while Codex coding did not settle within the phase deadline. Its actual
initial mode list identifies `agent` as model-powered auto review and
`read-only` as "Ask for approval". The test now selects that offered explicit
approval mode, retains every tool capability, and responds only to actual
one-time permission options for the scoped Rust command.

V5 did not run: the test incorrectly called `as_ref` on SessionModeId and was
repaired with its supported representation. V6 ran 35 cases: 33 passed and two
Codex cases failed because the assertion read `initial_settings` as current
mode. That snapshot intentionally records initial settings; the actual mode
change is retained in ACP Update events. V7 waits for the exact actual
CurrentModeUpdate after the command's prior event cursor. RPC acceptance alone
is not proof of adoption. Fresh sessions repeat the explicit test choice.
Each real permission starts the original bounded next phase, with the original
four-permission limit and 30-second harness turn bound unchanged.

V7 and V8 each failed both Codex cases: the worker acknowledged SetMode but
did not emit the expected mode/config notification. V9 instead uses the
existing SetConfig path. That real RPC returns the complete current option
table, which the supervisor installs before acknowledging the client mutation;
the test checks the returned mode value. It does not infer current state from
the empty SetMode response or the initial modes snapshot.

V9 ended with 34 passes and one Codex coding failure, 142.264 seconds. The
returned mode value was confirmed, but actual events showed HTTP 503 retries.
V10 provider diagnostics identified the current worker's `functions.exec`
custom tool; the traditional-function-only responder rejected it. V11 retains
that actual declaration/namespace, emits a native custom_tool_call with valid
JavaScript that calls the worker's actual `tools.exec_command`, and waits for
the real custom_tool_call_output. Both pass/fail Rust commands and actual
checkpoint observations remain required; no synthetic tool result is inserted.

V11 run `0ce55491-f70d-487b-a184-42b85d0e7f1d` passed all 35 ordinary/short
maintained cases, 122.506 seconds, one slow, two long campaigns filtered,
no LEAK or timeout. Codex coding passed in 4.760 seconds, Claude coding in
2.408 seconds; actual feedback/trial/restoration passed for both workers.
The normal explicit CLI/fresh identity/stop and retained lifecycle tests pass.

Only the nextest name filter changed for V12, preserving the original
60-second by 41 periods campaign deadline and all original evidence thresholds.
The 107-file source manifest includes that test configuration. V12 ran both
campaigns and failed both on a mixed gateway-revision read, 99.573 seconds,
not on a time or quality threshold. V13's existing processed-read wait handles
that known race just as the short trial does, within its original 30-second
window, waiting for the actual scheduler's resource refresh. No model call,
observation mutation or synthetic scheduler tick is added.

Owned combined session 29775 now runs both V13 campaigns serially, followed
only on success by full workspace nextest, strict Clippy, separate doctests,
formatting, diff and final 107-file hash checks. Source is frozen. A failure
stops later checks and is retained; pending checks are not counted as passed.
This slice is local and unpushed; MVP remains incomplete.

V13 subsequently ended with two failures, 144.927 seconds: both clients lost
their control lease while waiting for evaluation. The old terminal client
provided heartbeat; the migrated acceptance client had omitted it. Later
workspace/static/doc commands did not run because the chain stopped on failure.

V14 supplies the original five-second heartbeat against the same owner and
lease generation, without acquiring/recovering control. Errors are retained
and fence new prompts/session changes; explicit stop/drop ends heartbeat.
V14 run `2bfca7a8-6765-4ac1-8d8d-5fe3bd0265f2` passed 35 short/ordinary cases,
127.491 seconds, one slow, two long cases filtered. The V15 actual endpoint
release regression passed (`e878c548-f343-4fa2-9b03-98184716d6d7`, 76.256 seconds
including Cargo fixture build): the guard observed release, refused a prompt,
captured no prompt and left the lease absent. It did not reacquire or change TTL.

Owned combined session 40114 runs V15's two campaigns then, only on success,
the full workspace/static/doc/format/source checks. The 107-file source is
frozen. Actual readonly database inspection observed 53 enrolled sessions,
212 execution records, eight challenger sessions and batch three while the
Claude campaign remained Exploring. These are progress observations, not a
completed publication/rollback result. No real-model accuracy claim is made.

V15's Claude campaign later failed after 1,698.831 seconds: the original
30-second `processed` wait reported that coding evidence/background publication
did not settle. Its last periodic output was trial 192 (176 baseline, 16
challenger), still Exploring. The precise cause is unresolved. Codex is still
running at the PR preparation checkpoint. The combined set-e pipeline cannot
run its later workspace checks after a campaign failure; independent current
source checks were therefore started for the user's renewed PR request.

The user-authorized entry migration reviewer inspected the current diff without
editing or executing it. No confirmed new static blocker was found. The lease,
actual tool-result, publication and rollback assertions remain intact. This
review does not convert a failed or incomplete campaign into acceptance.

For the renewed PR request, V15's independent workspace run
`5356bf5f-8706-4787-874e-1ef497a3ee75` passed 4,600 tests, 22 original opt-in
skips, five slow tests, 792.596 seconds, with no LEAK or timeout. Separate
workspace doctests passed six cases with one original ignored case.

The first strict Clippy run rejected a needless borrow in the new test helper:
`serde_json::to_value(&option.kind)`. V16 only changes this to
`serde_json::to_value(option.kind)` for the Copy enum. No behavior or assertion
is changed. The complete workspace run belongs to V15; V16 repeats the affected
test binary, strict workspace Clippy, format/diff and 107 source hashes.
The first pipeline's source-check mismatch is expected after this recorded
lint cleanup; its failed Clippy result is retained. V15's still-running Codex
campaign uses its original compiled binary and is not V16 evidence.

V16 run `9009986d-fdcf-42f2-bc9c-23464b5f5681` passed 32 affected ordinary
tests, six original opt-in skips, four slow tests, 123.631 seconds. Strict
workspace/all-target/all-feature Clippy passed in 1m45s, as did formatting,
diff checks and all 107 source hashes. V16 did not repeat the complete workspace
run. The precise cleanup and validation scopes above remain explicit.

The PR includes `2026-10-14-pr981-native-campaign-snapshot.log` and its capture
timestamp. This is an immutable, nonterminal observation: Claude failed and
Codex was still running. The original V15 campaign log continues locally and
is not staged while it is being appended. Updated-head hosted CI remains a
separate check; the PR stays Draft and the full MVP remains unfinished.

Separately, published PR head `50a4605f` passed hosted Linux lib job
115798813433: run `30142e38-d788-4082-85ff-374814923d55`, 1,743 passed,
five original skipped, seven slow, 252.426 seconds. The previously failing
parallel Goal handoff/input/verified-delivery case passed in 85.702 seconds.
This is evidence for that published head, not the local test migration, and
does not identify the historical opaque error's exact cause.

The complete published-head CI run is now terminal success. Hosted macOS run
`7d92d2a5-0dd3-405f-a016-dc743521333d`: 4,598 passed, 22 original skipped,
13 slow, 1,359.208 seconds. Hosted Windows run
`284ee346-6fa4-4b7d-a35c-97ba03537ddf`: 4,451 passed, three original skipped,
11 slow, 1,336.853 seconds, no flaky marker. All Linux shards and three-platform
Clippy also passed. These results do not cover the local migration changes.

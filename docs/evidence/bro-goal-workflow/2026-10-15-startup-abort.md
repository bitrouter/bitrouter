# Recovery after a confirmed pre-session startup failure

This change follows the actual OpenCode initialize failure recorded in
`2026-10-15-real-goal-v3-warm-summary.json`. It does not change that historical
Goal or infer a safe retry from its missing native session ID.

The lifecycle owner now distinguishes an error returned before it invokes any
native session open/load/resume request. A supervisor receipt is positive only
when that phase and owned controller teardown are both confirmed. Failure text,
including `database is locked`, does not select the recovery path. An initialize
error with unconfirmed cleanup now propagates that fact instead of only logging
a warning.

The receipt defaults to false for old snapshots and ledger entries. Eligible
failed runs retain their failed state and a hash-bound transcript across restart.
Goal recovery additionally requires the original admitted start/run/workspace
mapping, complete contiguous history without session selection, prompt, update,
permission or omitted material, and a stable capture of the owned workspace.
The resulting artifact states its narrow scope; it does not assert an absence of
ambient initialization effects or unknown fees.

Recovery records stop confirmation and scoped reconciliation before admitting a
new execution identity. It preserves the original execution and cumulative
accounting. The persisted run observation time supplies a short delay between
attempts without blocking independent branches. Existing owner, control, budget,
requirements and dependency checks, three attempts per task version and sixteen
retained attempts per task continue to apply. User pause is not automatically
reversed. Exhaustion is projected as a needs-you state with retained evidence,
instead of an indefinitely pending replacement. Independent branches can continue.

Validation is recorded in the versioned `startup-abort-v*-status.json`, source
manifests and logs. The Cargo ACP peer and fixed model judgments are deterministic
transport/runtime evidence, not credentialed harness or model-quality evidence.
Full current-source workspace checks and the original real Node workflow remain
separate acceptance gates until their results are recorded.

The first two targeted attempts remain failed evidence. Version 1 ran nine
tests, with eight passing and the startup recovery scenario timing out. It
exposed that a typed `anyhow` context must be inspected through the context-aware
downcast API. Version 2 passed that direct type test but still failed actual
startup recovery and exhaustion: four tests passed, two failed and six were not
run after fail-fast. The original deadlines and assertions were retained.
Source inspection then found that the domain's stop-admission rule excluded
mapped failed/unknown executions while Goal intent remained Run. Stopping an
owned mapped execution for reconciliation must be distinct from authorizing
replacement dispatch. A direct initialize-failure process probe was added to
separately verify phase receipt production and preservation.

Version 3's selected app run `1fb58192-3943-4bf2-a1b8-10b1ed29e123`
passed all fourteen tests. The real Cargo ACP transport scenarios establish one
failed initialize followed by recovery and verified fixture delivery, six
initialize-only starts at the two branches' three-attempt bounds, and no new
attempt after explicit user pause. The failed original start receives no native
session open or Goal prompt. Type-context, complete-history, legacy/default-false,
cold hash-bound history, cancellation and needs-you projection checks also pass.
These results retain fixed model judgments; current-source database recovery,
workspace checks and actual OpenCode acceptance are reported separately.

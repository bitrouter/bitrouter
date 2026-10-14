# Bounded current resource revision read

Current local base includes the Goal archive slice validated by all 4,606
workspace tests, strict Clippy, six doctests (one existing ignored), formatting
and 108 source hashes. The new reader is a subsequent change and needs its own
evidence; it cannot inherit that workspace validation.

The maintained V17 campaign finished with two failures. Codex's original
30-second processing deadline expired while reading its learning snapshot,
even though the last background pass recorded publication with no errors.
Source inspection found `effective_assessment` deserializing complete resource
revision history and then taking its last element. Background refresh retains
new resource revisions as gateway coverage advances.

The current reader now queries only the highest resource revision for the
requested checkpoint. Migration 000029 adds the compound checkpoint/revision
index so that selection does not require sorting the entire revision history.
The full resource-history method remains available and no rows are discarded.

Checkpoint manifest/identity/deletion checks run before and after the read.
The selected observation's identity, checkpoint, revision and timestamp must
match its stored row. Existing capture-health, source-content and publication
fences remain unchanged. No cached numeric sample or fabricated observation is
introduced, and no allocation parameter, evidence threshold or campaign deadline
is changed.

The updated late-metering regression checks four retained revisions while the
current view selects the latest charge. A new actual SQLite regression checks
the compound-index query plan, rejects foreign-session and mismatched-payload
reads and rejects a deleted checkpoint. The selected checkpoint and resource
coverage tests are running against a separate 112-file source manifest.

Selected run `a075917e-6d24-4df8-ba3b-743c48ceeae8` (session 25332) is terminal success: 18 checkpoint/resource-coverage
regressions passed, 1,733 filtered, 0.460 seconds. The 112-file source is now
frozen for owned session 52028: full workspace, strict Clippy, separate
doctests, format/diff/hashes, then the two actual maintained campaigns only
after those checks pass. This chain is live; later results remain pending.

This is a structural reduction in current-state read work. It is not a measured
throughput claim, a proven cause attribution for every timeout, successful
publication/rollback acceptance or real-model/provider performance evidence.
The maintained long scenarios will be rerun only after relevant checks pass.

The current 112-source full workspace run is terminal success:
`d0d75553-fc88-4805-9698-7709b966eeb5`, 4,607 passed, 22 existing opt-in
skips, nine slow, 658.671 seconds, no LEAK or timeout. Strict workspace,
all-target, all-feature Clippy with `-D warnings` also passed in 1m09s.
The separate doctest and subsequent format/hash checks are still running.
The user requested an update to the existing Draft PR #981 targeting `main`;
these checks are required before publishing this source update.

Those remaining standard checks have now passed on the same 112-source
manifest: six doctests passed with one existing ignored case; formatting,
diff checks and all 112 hashes passed. The owned pipeline then advanced to
the two maintained publication/rollback scenarios, with original bounds and
thresholds intact. Their outcome is pending and their active log is excluded
from this PR update. The prior failed campaigns remain retained as evidence.

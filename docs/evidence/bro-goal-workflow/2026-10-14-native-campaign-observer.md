# Native campaign observer: V17

Base commit: `067ef9249f06efbdbfb48bdddbbf3e32e6f1b2ea`. This work is local,
not pushed. The previous Goal turn made progress by committing the migration
and evidence to the user-requested Draft PR; it did not complete the MVP.

The original V15 run `e270954d-bdec-41d4-a537-25468a2faa46` remains owned by
session 40114. Claude failed after 1,698.831 seconds because the original
30-second evidence/publication processing wait timed out. Codex was confirmed
live through the same handle and PID 49852. Its owned SQLite file was inspected
read-only: 201, then 204, 206, 222 and 240 enrolled sessions; still Exploring.
At 240 sessions the challenger count was 17 and batch sequence 14. These are
progress observations, not adoption/rollback acceptance.

A one-second sample of that actual Codex test process showed the foreground
`processed` observer reading `learning_status` / `learning_snapshot` /
`effective_assessment`, while the background worker was also in resource
refresh / checkpoint observation. This establishes concurrent work in these
paths, not a performance benchmark or proof of the Claude timeout's cause.
The full local sample is `/tmp/bro-native-codex-campaign-49852-sample.txt`.

V17 retains the original 30-second processing timeout, 2,400-second campaign
bound, default bandit/batch parameters, minimum samples and all actual code,
cost, publication, automatic quality rollback and restored-dispatch assertions.
The read-only observer now inspects the complete learning snapshot at most
once per actual background pass. It neither ticks the scheduler nor writes
learner evidence. Timeout diagnostics retain the current read/wait phase,
last observation flags/cohort status and existing worker status. No result is
accepted from worker status alone.

V17's Cargo-built test binary compiled and all-target/all-feature package
Clippy with `-D warnings` passed. Maintained long-scenario validation is still
required before accepting this change; static checks do not prove the fix.
V15 used its original compiled binary and is not V17 evidence. It is now
terminal: session 40114 exited 100; 0 passed / 2 failed, 4,121.424 seconds.
Codex failed after 2,422.584 seconds at the original total campaign bound:
trial 250 was rejected, with 232 baseline and 17 challenger samples. Its
minimum evidence condition was not bypassed. The combined pipeline's later
workspace/static steps did not run; the PR's independent checks remain
separately recorded.

V17's two maintained campaigns were started only after that terminal result,
with one test thread and the same pins, isolated workers, local HTTP responders,
workload, deadlines and assertions. Current owned session: 11032. The new
source manifest is `2026-10-14-native-optin-migration-v17-source.sha256`.
This is an actual rerun of changed observation code, not a restart triggered
by an observation timeout. Run ID: `a8ccd71e-4169-439c-891a-898dcc466607`.
The owned Claude process (PID 95345) and its SQLite file were read-only checked:
44 enrolled sessions, five challenger sessions, batch two, still Exploring.
The outcome remains pending; no throughput or performance benefit is inferred.

The V17 run is now terminal: session 11032 exited 100, 0 passed / 2 failed,
4,180.874 seconds. Claude failed after 2,411.812 seconds at the original total
bound before trial 220 (199 baseline / 20 challenger), still not adopted.
Codex failed after 1,769.057 seconds, specifically in `reading learning snapshot`.
Its latest completed background report recorded one checkpoint, one judge job
and one publication with no reported errors. The foreground snapshot itself
did not return before its original 30-second processing deadline. This narrows
the next diagnosis; the observer cadence change has not completed acceptance.

Read-only source inspection found that `effective_assessment` retrieves the
complete checkpoint resource history and then selects `.pop()`. Background
refresh creates immutable resource revisions whenever shared gateway coverage
changes. Thus a current-state read deserializes all preceding revisions for
every enrolled session; the explicit history endpoint still needs those rows,
but the current-state reader needs only the latest. This is a concrete growth
path, not yet proof of its exact share of the failed run's elapsed time.
Checkpoint identity/deletion and capture-health fences must remain in any
bounded latest-revision reader. No further identical campaign was restarted.

Independent source audit also found the current task execution list has a
hard 16-entry admission limit, plus a three-attempt limit per task revision.
Its retained payloads can still exhaust the 1 MiB Goal head, and the count
limit prevents indefinite same-Goal requirement cycles. The archive work must
retain original immutable execution/effect/version facts and make cold
history inspectable. Increasing the limits or dropping records is not an
archival implementation. No archive or general recovery gate is marked done.

Read-only local configuration metadata shows no `decision_model` configuration.
The four standard provider/typed credentials are absent from the process
environment; no corresponding names were found in the local BitRouter env file.
Existing OAuth providers may have independent credentials; this check does not
assert that all live generation is unavailable. No secret values were printed,
no configuration was changed, and no paid model call was made.

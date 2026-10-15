# Read-only archived execution inspector

Base: `a316e745845f39982e0151cec94a96028431bf30`, published Draft PR #981.
The new inspector source is local and unpushed. The previous Goal turn made
progress by publishing the archive/resource-reader slice after all 4,607
workspace tests, strict Clippy, doctests, formatting and 112 hashes passed.
This subsequent 114-file slice needs separate validation.

Opening an archive uses the same bounded, hash/path/length-validated private
artifact reader, then queries its stable operation identity through the
actual owner-scoped Goal client. Goal/task identity, complete execution,
retired permission records and the stored record reference must match the
immutable receipt. An uncommitted blob, a different Goal/task, a mismatched
record or changed bytes cannot become authoritative history.

The pinned inspector shows the original execution and permissions, their
source versions and links to original checks, workspace, semantic review,
reconciliation, prepared prompt/context/skills and permission request bytes.
The previous archive link replaces the current historical record rather
than loading the whole chain. The app retains one current archive's links;
the TUI retains one original task and its reading/selection positions.
Escape returns from an evidence preview, then from historical inspection to
that pinned task. Current snapshots do not replace opened historical content.
Late reads are consumed only for the exact artifact still open in the same
Goal. Permission request bytes in this view remain read-only, with no consent
or execution control. Browsing preserves the middle Goal's draft and scope.

The current TUI code passed 25 Goals tests. The new navigation regression
covers two old records, newer current snapshots, ignored right-side control
keys/paste, a late old read, original reading position and draft submission
after return. An actual SQLite/IPC domain-fixture test reads a committed
two-record chain and original material; refuses foreign scope, mismatched
source, uncommitted material and changed bytes; and checks that reads do not
change the Goal or its history. This is a storage/transport fixture, not
real model or branded Goal harness acceptance.

The existing actual daemon resource-edit test also opens the archive through
the UI's production reader after the runtime produces it. It must see the
original execution identity and actual tool/stop reconciliation link. This
test and the client cases are running on the new 114-file manifest. The
earlier 11 client cases passed before the final reader fixture/test helper
was compiled; that earlier run does not prove the final new test.

The final selected application run is now terminal success: all 13 cases
passed, one slow, 115.010 seconds, 1,739 filtered. This includes the actual
SQLite/IPC chain/refusal test and actual daemon-produced archive read. The
114-file manifest still matches. Strict workspace Clippy followed by full
workspace tests, separate doctests, formatting/diff/hashes has started for
this exact source; those later results remain pending.

Strict workspace/all-target/all-feature Clippy is now terminal success,
1m44s. Owned session 81463 has advanced to the full workspace compile/test
stage on the same 114-file source. Its later workspace/doc/format/hash
results remain pending. Session 52028 was separately re-polled and remains
live on the unchanged maintained campaign binary. No new source was pushed.

Owned session 52028 is still running the two maintained native campaigns
against its pre-inspector compiled source/binary, run
`6370ffe1-d3c9-4316-b7e5-bee791c14ba9`. Those results will apply to that
archive/resource-reader version, not these new Goal UI changes. No timeout,
bandit parameter, sample minimum or publication assertion was changed.
The published head's hosted CI is also separate from this local slice.

This closes the missing archive drill-down path, not the entire G13/F09 gate.
General external-effect recovery, long-lived UI event/reference retention,
cross-incarnation fault coverage, real input evaluation/provider/typed
backend evidence and the remaining completion/budget gates are still open.

The 114-file full run is terminal failure: 4,608 passed / one editor PTY failure / 22 original skips, 1,070.370 seconds. Later doctest/format/hash pipeline stages did not run. The editor serial recheck passed in 111.929 seconds. New-frame waits retained the original byte and deadline assertions; the first four-test run had one timeout, then contextual serial validation passed in 84.965 seconds and final four-test parallel validation passed in 150.798 seconds. Config diagnostic redaction and live-input tests subsequently changed local source; a new complete source validation is required. The native campaign is also terminal, Codex pass / Claude fail, on its earlier binary.

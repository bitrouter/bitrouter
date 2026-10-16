# Native evidence local checkpoint

Parent: 867642d95bc5a74e8b777bce2d3bb3f79f11a8a5. No refactor push or hosted CI is claimed.

All native-evidence-* files are immutable trial records or exact source/runner captures. Trials1-3 fail the new serving-report completion requirement, with Goal page limit corrected after source3. Trial4 passes. Trial5 is 18/19 with a historical-attempt selection assertion; trial6 is 19/19. Gates7 pass all 74 Goal tests, strict Clippy/fmt, stress5 and rustdoc. Gates8 are 73/74, with an ACP startup failure whose cause is unconfirmed and original memory DB not retained. Trials9/10 fail copying a memory DB; the independent fixture tars each verify 251 surviving filesystem files, not the memory DB or process state. Trial11 passes explicit file-backed copy/reopen. Gates12 preserve ordinary storage profiles and pass Clippy/fmt,74 Goal tests,two cases x5,rustdoc6+1ignored; one accounting case has a nextest LEAK report. Its five isolated repeats pass without leaks; this does not establish a cause or fix.

Source12 audit: unchanged throughout gates12. Source13 applicability verifies the saved tar and that subsequent differences are documentation only. The initial source1 runner label is stale from a prior campaign; actual command arguments and source hashes are authoritative. Logs are raw, not provider/harness qualification. Actual activity overlap, original-action recovery, live harness/provider and final full-suite gates remain open. Fixture retention is opt-in; memory fixtures record the missing full copy rather than claim it.

See native-evidence-manifest13.json for archive checksums. This archive is local, not yet separately published.

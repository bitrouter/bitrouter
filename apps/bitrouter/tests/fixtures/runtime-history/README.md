# Historical runtime journals

These are exact `StoredExecution` JSON bytes emitted by the production writers
at the revisions in `provenance.json`, compressed with Zstandard. Each capture
uses that revision's real ThreadService and MemoryExecutionStore, two scripted
model replies, and one successful read of a synthetic workspace file. The
historical runtime writes and then shuts down before its stored journal is
serialized. No format number, record, workspace path or nested checkpoint has
been rewritten. These are journal fixtures, not historical SQL databases.

| Format | Writer revision | Records | Distinct retained payload |
| --- | --- | --- | --- |
| 2 | `e2453004734b1a2723b5f7e54087e21f3e64a315` | 34 | Original Thread/Turn model and tool exchange |
| 3 | `321ccaad5fe69f7adb39e0230a11f6e1851ff84d` | 36 | Harness inventory and resources |
| 4 | `aa2ec3476f68b1a5a72faba2e9f97f2be767a120` | 37 | Instruction-context snapshot |
| 6 | `a68c215f0b579aeb34dd15baf1670ef037c310f8` | 106 | 32 native core checkpoint batches; no stored artifacts |

All captures contain a completed read result with `historical writer evidence`
and a completed turn with the answer `done`. The final shutdown checkpoint is
paused; decoding must preserve that status rather than grant execution authority.
Candidate tests reconstruct the journals offline in bounded pages, with and
without core, without opening the recorded temporary workspace paths.

The format-4 and format-6 reconstruction tests run on Unix because their
instruction snapshots retain macOS absolute working-directory paths. Those
paths are not absolute under Windows path rules; the tests do not rewrite them
or relax production validation. Formats 2 and 3 have no instruction snapshot and
their reconstruction tests remain cross-platform. These captures do not prove
Windows-native instruction recovery, nonempty instruction/resource inventories,
or stored core artifact compatibility.

The provenance file records original source/lockfile hashes, the exact added
capture-test patch, retained writer executable identity, raw/compressed payload
hashes and record/fact counts. Capture logs preserve results; only local build
directory prefixes are replaced by placeholders. Model endpoints/credentials in
the historical helper are synthetic; no external provider or user service runs.

To reproduce a capture, check out its exact writer revision in a separate
worktree and apply the corresponding `writer-vN.patch`. The patch adds one
ignored test only. Keep production source, Cargo manifests and the lockfile
unchanged. Set `BITROUTER_HISTORICAL_FIXTURE` to a new absolute JSON output path,
and run with Rust/Cargo 1.99.0:

```sh
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test --locked -p bitrouter-orchestrator --lib -j 1 \
  service::tests::support::write_p0_historical_runtime_fixture -- --ignored --exact
```

Format 2 uses the test name
`service::recovery_tests::write_p0_historical_runtime_fixture`. The fixture test
requires exactly two model responses and one tool result before writing a new
file. Fresh captures have new random identities and timestamps; they are not
expected to hash identically to the retained original bytes.

Format 7 has a separate actual baseline database and journal in
[`../baseline-runtime-v7`](../baseline-runtime-v7/README.md). The inspected
format-6 introduction changes the writer directly from 4 to 6 and adds reader
acceptance of 5 in that same commit. No format-5 writer has been established;
this collection makes no historical format-5 payload claim and does not fabricate
one by editing a version tag. The existing reader's version acceptance remains
unchanged.

# Pre-refactor runtime format 7 fixture

This fixture was written by the unchanged `bro` executable built from
`9956a427bd5d4316b3bf2359f88e293b158781d5`, before the service refactor. Its
SHA-256, capture source/lock hashes and output identities are in
`provenance.json`. The separate Cargo-built Rust generator runs a private
foreground service and a read-only native task against a synthetic loopback
provider. Two model calls surround one actual read of `evidence.txt`; the final
answer is `The evidence says baseline persisted payload.`

- `database.sqlite.zst` contains a standalone SQLite snapshot with 284 execution
  records, format/version 7/284, and all 26 baseline migration ledger entries.
- `journal.json.zst` contains the exact decoded JSON of those ordered records,
  including 142 nested turn facts and 92 native checkpoint batches. Checkpoint
  strings, digests, identities and recorded workspace paths are unchanged.
- The task output, provider requests and configuration retain the synthetic
  capture inputs and externally observed completion.
- `provenance/generator-original.*` retains the exact capture source, manifest
  and lockfile. `generator/` is the runnable capture tool with a subsequent
  readiness fix: it checks the private control socket's PID against its owned
  foreground child before invoking the task CLI. Its own workspace is test
  support, not another product package; it imports no application library.

The captured main database depended on committed WAL pages. The retained
snapshot was produced with SQLite's read-only `.backup`, then independently
opened with `immutable=1` and checked. Its ordered raw payload extraction exactly
matches the original database plus WAL. The main-file and WAL hashes are retained
separately; the original main file alone is not a usable fixture. Compression
only packages the files; it does not change runtime format fields or record data.

The database regression reopens a private copy twice, reapplies migrations and
checks ledger timestamps, raw record bytes, decoded records, bounded record and
public-history pages, acceptance keys and stopped-owner evidence. Offline native
replay and complete Thread reconstruction consume the original journal with and
without core; no test opens its recorded workspace or executes its tools.
Corrupt checkpoint digests remain rejected. These checks do not establish live
scheduler restoration, full/minimum/full artifact transitions or formats 2–6.
The current-writer fixture in `../native-replay` remains separate.

To capture deliberately on macOS, build the pinned historical source, verify its
identity, and supply an absolute executable path and a nonexistent output path:

```sh
cargo run --locked --manifest-path \
  apps/bitrouter/tests/fixtures/baseline-runtime-v7/generator/Cargo.toml -- \
  /absolute/path/to/baseline/bro /tmp/new-bitrouter-baseline-capture
node apps/bitrouter/tests/fixtures/baseline-runtime-v7/export.mjs \
  /tmp/new-bitrouter-baseline-capture /tmp/new-bitrouter-baseline-export
```

The optional offline exporter requires Node with Zstandard support and the
SQLite CLI; normal Rust tests require neither. It verifies every compressed row's
original size/digest, restores any packed Base64 field and concatenates original
JSON without reserializing it. A fresh capture has new IDs, paths and timestamps;
review its provenance rather than expecting identical whole-file bytes. Never
regenerate with the candidate writer merely to accept a compatibility regression.

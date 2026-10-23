# Native journal replay fixture

`journal.json.zst` is a compressed JSON array of exact `ExecutionRecord` values
written by the local native driver. Its scripted model reads `evidence.txt`,
receives a real workspace-tool result and returns a final answer. Checkpoint
payload strings, hashes, identities, event order and display projections are
retained as written. Replay does not open the original temporary workspace.

The fixture was generated during P0 scheduler isolation, after `ab89178`, by
`harness::agent::tests::native::write_native_replay_fixture` with all application
features enabled. It represents that writer's current format; it is not evidence
for historical release-format compatibility. The test source records the model
script and workspace input used to produce it.

The same replay tests run with core enabled and disabled. They validate the
original journal and reject digest corruption, an earlier immutable fact changed
under a newly valid digest, and display data inconsistent with its checkpoint.
They do not claim to exercise scheduler restoration or a cross-version database
upgrade.

To regenerate deliberately from the repository root:

```sh
BITROUTER_NATIVE_REPLAY_FIXTURE="$PWD/apps/bitrouter/tests/fixtures/native-replay/journal.json.zst" \
  cargo test -p bitrouter --all-features --lib \
  harness::agent::tests::native::write_native_replay_fixture -- --ignored --exact
```

Review changed bytes and rerun both feature profiles after regeneration; do not
rewrite the fixture merely to accept a replay regression.

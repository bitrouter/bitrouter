# M0 gateway settlement capture

These two SQLite snapshots come from the same local run of BitRouter at `b0d7e2fbeabe39b96692c48787a066e89c04aed0`. The capture driver calls the assembled gateway pipeline, the ACP trajectory recorder, and the production metering store. Only the HTTP providers are fixtures.

The first provider returns HTTP 503. The final provider reports ten prompt tokens and two completion tokens. Its configured rates yield a 30 micro-USD observation. The direct requests later receive a 47 micro-USD bill and an explicit Unknown receipt. The fallback request receives a 61 micro-USD bill. Its dispatch journal retains the final hop observation and an unknown route total.

`dispatch.sqlite.zst` contains the stopped recording and the pending metering rows. `reconciled.sqlite.zst` includes the later receipts. M0 wrote the capture connection, lifecycle events, coverage revisions, ordered hops, and settlements. The migration tests compare the two original journals and check the current gateway’s cost reader after restart.

The uncompressed SHA-256 digests identify the committed capture.

| Snapshot | SHA-256 |
| --- | --- |
| Dispatch | `70348ba0b6b056eba6b079824d4377913c3a6d1868a5cd2ca402981ba3c91e3b` |
| Reconciled | `3db57577c52c157d5daa930464556cc10bcd3200e187b3127cc318cb02db5f26` |

To reproduce, copy `capture.rs` to `apps/bitrouter/examples/m4_gateway_capture.rs` in a clean checkout of the pinned revision. Run the following from that checkout. Each capture has fresh timestamps, connection IDs, and attempt IDs.

```sh
out=$(mktemp -d)
BITROUTER_HOME="$out" cargo run --locked -p bitrouter --no-default-features \
  --features harness --example m4_gateway_capture -- "$out"
zstd -19 "$out/dispatch.sqlite" "$out/reconciled.sqlite"
```

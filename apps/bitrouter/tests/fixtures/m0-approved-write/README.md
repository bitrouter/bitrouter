# M0 approved write capture

`database.sqlite.zst` is a SQLite backup of the stopped, unmodified M0 owner
built from `b0d7e2fbeabe39b96692c48787a066e89c04aed0`. The M0 task client
accepted the real permission request for `write-evidence`. The native tool
changed `evidence.txt` from `baseline persisted payload\n` to
`approved M0 write\n`. The fixture retains the recorded approval, dispatch,
typed JSON result, completed effect, and both computed charges of 57 micro-USD.
The loopback provider supplied only model responses and usage. The task ended
as completed with `unknown_effect: false`, and the foreground owner stopped.

The snapshot has one format-7 Thread with 307 records and a stopped generation
1 owner. The expected result is `{"type":"json","value":{"path":"evidence.txt","bytes":18}}`.
The compressed snapshot and the workspace file respectively have these SHA-256
digests:

- `4eb1c6f5fc48724044bc66bb98967f432a1eddf635ed30f4761b35f12cf9f321`
- `846d78fec1dc12bd6b71ce48d0307c1621e82bc07a21c2f96a361730fd79f2de`

The uncompressed SQLite SHA-256 is
`f67e19c570602ced5b277e0b125523377c1d08ff2f10bb2f3e885d06aba1795c`.

To reproduce, build `bro` in a clean checkout of the pinned revision with
`cargo build --locked -p bitrouter --bin bro --no-default-features --features core,harness`.
From this checkout, run the generator with that executable and a fresh short
absolute directory (for the M0 Unix socket):

```sh
cargo run --locked --manifest-path \
  apps/bitrouter/tests/fixtures/baseline-runtime-v7/generator/Cargo.toml -- \
  /absolute/path/to/m0/target/debug/bro /tmp/m0-approved-write approved-write
sqlite3 -readonly /tmp/m0-approved-write/bitrouter.db \
  '.backup /tmp/m0-approved-write/snapshot.sqlite'
sqlite3 'file:/tmp/m0-approved-write/snapshot.sqlite?immutable=1' 'PRAGMA integrity_check;'
zstd -19 /tmp/m0-approved-write/snapshot.sqlite
```

Each capture generates fresh identities and timestamps. No test needs to
rebuild M0 or open the workspace at its recorded path.

# Baseline plain transcript captures

These fixtures were generated using the unchanged `bitrouter-tui` at commit
`9956a427bd5d4316b3bf2359f88e293b158781d5`. The generator depends directly on a
sibling checkout named `bitrouter-baseline`, not the implementation under test.
It constructs synthetic SDK updates, serializes and deserializes the inputs,
then calls the baseline public helpers. Expected output is never handwritten,
trimmed, normalized, or produced by the replacement formatter.

The files are authoring evidence and replay data only. Test integration belongs
to the consuming test harness.

## Files and replay contract

`cases/captures.json` preserves the original capture format: `captures[]` entries
name `id`, `driver`, `input`, and `expected`. All paths are relative to `cases/`.
Byte ranges are zero-based, start-inclusive and end-exclusive. Compare the whole
expected byte file and every recorded range, including empty ones.

| Case ID | Input / expected files | Driver and coverage |
| --- | --- | --- |
| `markdown-unicode` | `markdown-unicode.input.json` / `markdown-unicode.expected.txt` | Three settled turns; literal user text, Markdown, tables and oversized-table fallback, code fences, tabs, CJK, accents, ZWJ emoji, long lines, split emphasis, repeated reads |
| `tools-diffs-reasoning` | `tools-diffs-reasoning.input.json` / `tools-diffs-reasoning.expected.txt` | Two turns; plans, thoughts, completed and failed tool patches, command frames, truncated output, separated diff hunks, new-file and unchanged diffs, attachment descriptions |
| `cost-omission` | `cost-omission.input.json` / `cost-omission.expected.txt` | Two turns; absent, harness, router, unknown, malformed, missing-figure, and explicit-zero cost updates; six empty usage/repeat checkpoints |
| `failure-tails` | `failure-tails.input.json` / `failure-tails.expected.txt` | Long CRLF log, short log without final LF, and empty log, through the actual baseline failure-tail helper composition |
| `line-constructors` | `line-constructors.input.json` / `line-constructors.expected.txt` | Fifteen direct constructor cases: line raw / from owned string / from borrowed string, span raw / styled; mixed separators, empty strings, and newline-only strings |

For the three transcript cases, input is
`{format_version, id, coverage, steps}`. Each step is either
`{"op":"update","update":<SDK SessionUpdate>}` or
`{"op":"checkpoint","id":...,"reason":...}`. Start a fresh transcript per
case. Apply updates in order. Only at a checkpoint call `unwritten()`, then
serialize those rows. Checkpoint metadata has `id`, `reason`, `step_index`,
`byte_start`, `byte_end`, `rows`, and `sha256`. Do not call `finish_stream`, flush
per chunk, add an implicit final checkpoint, add a footer, or wrap output again.
The baseline extent is 80 columns by 24 rows.

Failure inputs are an array of `{id, path, log, max_lines}`. For each input call
`log_tail::render(path, log, max_lines)`, then `plain::write`. Its `captures[]`
metadata contains `id`, `byte_start`, `byte_end`, `rows`, and `sha256`.
`max_lines` is the baseline's `TAIL_LINES`, 20.

Constructor inputs are an array of `{id, constructor, text}`. Constructor values
are `line_raw`, `line_from_string`, `line_from_str`, `span_raw`, and `span_styled`.
The first three construct a `Line` directly; the last two construct one `Span`
and convert it to a `Line`. Styled spans are yellow and bold. Serialize each
single row through baseline `plain::write`. This case uses analogous `captures`
metadata with an additional `lf_bytes` count. `rows` is always one, while an
unmodified span can contain several LF bytes. The one-shot generator retains
the fifteen small input strings for its lifetime to exercise `Line::from(&str)`
with the baseline writer's required `'static` lifetime.

The captured distinction is intentional: line constructors split with Rust
`str::lines()` and concatenate their spans when written, removing LF and CRLF
separators. Span constructors preserve the supplied content. Lone CR, U+2028,
U+2029, tabs and Unicode remain content. `plain::write` adds one final LF per
logical row and drops styles. Do not strip CR or normalize whitespace in these
fixtures. The two pinned constructor source files also match the checked crate
archive; their hashes are in `provenance/constructor-sources.json`.

## Baseline call sites and limits

The baseline `crates/bitrouter-tui/src/plain.rs` owns `Transcript::apply`,
`Transcript::unwritten`, `text`, and `write`. Both
`apps/bitrouter/src/acp_cli.rs` (`Presenter::turn_settled`) and
`apps/bitrouter/src/chat/session.rs` (`chat_plain`) serialize settled transcripts
through those helpers. Usage updates update journal metadata without adding
transcript entries; these captures contain no cost footer.

The failure composition is not hypothetical: baseline
`apps/bitrouter/src/chat/session.rs:37` (`write_session_log_tail`) calls
`log_tail::render` followed by `plain::write`. `chat_plain` invokes it on the
failed-turn stdout path at line 219; `report_failed_launch` invokes it for stderr
at line 63. The fixture exercises those public helpers with supplied log text,
not the private file-reading wrapper, an actual failing agent, or stream routing.
The log path is retained even when the log is empty.

These are not complete CLI subprocess transcripts. They exclude chat's separate
stop-reason line, `turn failed: ...` prefix, permission notices, ACP result
envelopes, unreadable-file OS errors, and launch/teardown behavior. There is no
invented completion or cost footer. In-place patches are settled before emission;
this does not test rewriting rows already emitted to a pipe.

Capture host: macOS, aarch64. `tools-diffs-reasoning` uses fixed POSIX absolute
paths rendered by the diff helper; keep its byte golden Unix-only unless a
Windows baseline is captured. The other cases may be replayed across platforms,
but no Windows run was performed here. The synthetic paths are display inputs,
not files the generator opens. Shell commands and URLs are inert input strings.
The isolated generator's features are recorded; this is not a capture of the
entire application's unified dependency feature graph.

## Reproduce

Run from the consuming repository root. A sibling `../bitrouter-baseline` must
already be checked out at the recorded revision. Never point the dependency at
the rewritten formatter. The standalone `[workspace]` keeps the generator out
of the application's workspace. The only package path dependency is the
baseline `bitrouter-tui` crate.

```sh
set -eu
fixture="$PWD/apps/bitrouter/tests/fixtures/transcript"
baseline="$(cd ../bitrouter-baseline && pwd)"
test "$(git -C "$baseline" rev-parse HEAD)" = 9956a427bd5d4316b3bf2359f88e293b158781d5
(cd "$fixture" && shasum -a 256 -c SHA256SUMS)
(cd "$baseline" && shasum -a 256 -c "$fixture/provenance/baseline-source-sha256.txt")

export CARGO_TARGET_DIR=/tmp/bitrouter-p0-transcript-baseline-target
export CARGO_BUILD_JOBS=2
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
export CARGO_INCREMENTAL=0
export CARGO_NET_OFFLINE=true
capture_root="$(mktemp -d "${TMPDIR:-/tmp}/bitrouter-transcript.XXXXXX")"
cargo run --manifest-path "$fixture/generator/Cargo.toml" --offline --locked --jobs 2 -- "$capture_root/a"
cargo run --manifest-path "$fixture/generator/Cargo.toml" --offline --locked --jobs 2 -- "$capture_root/b"
diff -qr "$capture_root/a" "$capture_root/b"
diff -qr "$fixture/cases" "$capture_root/a"
cargo clippy --manifest-path "$fixture/generator/Cargo.toml" --offline --locked --jobs 2 --all-features -- -D warnings
cargo test --manifest-path "$fixture/generator/Cargo.toml" --offline --locked --jobs 2 --all-features
cargo fmt --manifest-path "$fixture/generator/Cargo.toml" -- --check
```

The output directories must not already exist; the generator refuses to
overwrite files. Offline reproduction requires the recorded dependencies in
the Cargo cache. Match the Rust/Cargo versions and build overrides recorded in
`provenance/environment.json` when investigating differences. The generator
binary has no unit tests; its substantive verification is repeated execution,
complete-file equality, checkpoint hashes, and the input/coverage audit.

## Provenance and verification

- `generator/src/main.rs`, `generator/Cargo.toml`, `generator/Cargo.lock`: complete
  authoring source, exact direct pins, and the entire resolved dependency lock.
- `provenance/baseline-sources.json` and `baseline-source-sha256.txt`: byte sizes,
  SHA-256 and Git blob identities for all baseline TUI source files, manifests,
  baseline lock and the two application call-site files (29 files total).
- `provenance/baseline.Cargo.lock`: unchanged original workspace lock.
- `provenance/resolved-dependencies.json`: 154 resolved packages and features;
  all 152 registry package versions/checksums match the baseline lock, and all
  152 cached crate archives match those checksums. Adding the direct ratatui pin
  for constructor capture adds no new resolved package or feature.
- `provenance/constructor-sources.json`: hashes of the two ratatui constructor
  implementations, independently compared to their verified crate archive.
- `provenance/capture-history.json`: digests of the pre-existing early/final
  captures, the resumed original replay, and two runs of the retained generator.
  The original final capture was reproduced before payload cleanup. Earlier
  source was unavailable; its captures are historical evidence, not the final
  expected bytes. The retained source uses neutral sample text and adds the
  constructor case; it does not alter rendering logic.
- `provenance/environment.json` and `verification.json`: source/generator/lock
  hashes, toolchain, baseline status, build constraints, checks and limitations.
- `SHA256SUMS`: every retained file except itself. Byte preservation attributes
  prevent checkout newline conversion, including for the CRLF edge fixture.

The retained generator produced two identical captures. All 36 byte ranges
(18 transcript checkpoints, three tails, fifteen constructor cases), including
11 empty ranges, were checked against hashes, lengths and input identities.
Clippy with warnings denied and formatting checks pass. The baseline's five
pre-existing deleted generated assets are listed in the environment record;
none of the 29 relevant source/lock files differ from the baseline commit.

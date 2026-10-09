# BRO six base tools acceptance

Updated: 2026-10-04. Contract: [six base tools](BRO_BASE_TOOLS_SPEC.md).
This record separates original tool experiments, Thread/Turn integration and
later Conversation/main acceptance. It condenses existing records; no new provider
run, Rust suite or hosted CI was executed for the documentation cleanup.

## Thread Turn integration

Integration starts from #945 `e2453004`; its source hashes are in
[the integration manifest](evidence/bro-base-tools/integration.json).
The earlier dirty snapshot `55dad4a7` is not its implementation baseline.
The integration preserves effect classification, bounded read workers, ordered
exclusive effects, approvals/steering and durable ACKs. Independent verification
uses the Turn's selected interpreter and `EffectStatus`; unavailable execution
cannot be misclassified as an unknown launched effect.

| Recorded gate, macOS ARM64 / Rust 1.97.0 | Result |
| --- | --- |
| Final workspace all-feature nextest | 3,698 passed, 22 skipped, zero failures/leaks |
| Strict Clippy, fmt, doc tests, strict rustdoc, dist check | Passed; doc tests 5 passed/1 ignored |
| Native/local/HTTP cross-process fixtures | Five passed, including selected-interpreter loss with NotExecuted and no effect |
| Real `openai/gpt-5.4-mini` coding | Eight calls covering all six tools, no tool errors; independent verification passed |
| Real read-only inspection | Six calls using read/glob/grep only; all fixture hashes unchanged |

[Coding summary](evidence/bro-base-tools/integrated-coding/summary.json) and
[read-only summary](evidence/bro-base-tools/integrated-readonly/summary.json)
retain binary identity, calls, usage, interpreter/verification and terminal state.
The original manifest tracks hosted checks on PR #951; it is a historical record,
not proof of fresh CI at later UI/main source. An early inspection-tools leak
warning and stale TUI "Approve bash" assertion preceded the successful full rerun.
Complete original details remain in [the previous acceptance record](https://github.com/bitrouter/bitrouter/blob/65555b126e8d14982b2a7c977b618d545cd8f5b7/docs/BRO_BASE_TOOLS_ACCEPTANCE.md).

Later source `485728f9` includes Conversation UI and main integration. Its final local
fixture results and limitations are in [UI acceptance](BRO_CONVERSATION_UI_IMPLEMENTATION.md)
and indexed in [runtime acceptance](BRO_AGENT_RUNTIME_IMPLEMENTATION.md).
Earlier tool provider/platform results are not promoted to that revision.

## Original experiments and platform evidence

[The original manifest](evidence/bro-base-tools/manifest.json) identifies the
2026-10-01 macOS experiments and source/binary/prompt hashes. Four controlled
`bitrouter:openai/gpt-5.4-mini` runs succeeded:

| Scenario | Result |
| --- | --- |
| Directed six-tool coding | Eight calls, paginated directory reads, repaired code, NOTES.md and independent tests |
| Read-only | Six read/glob/grep calls, correct diagnosis and unchanged fixture hashes |
| Natural six-tool coding | Eight calls, repaired code and independent tests |
| Natural seven-tool baseline | Seven calls, repaired code and independent tests |

Shell and verification declared `/bin/bash` with dialect `bash`; every recorded
request had the expected tool profile, and every model step reported usage.
These are controlled functional results, not broad coding quality or Cloud charge
settlement. The initially misbuilt baseline and pre-tool authentication failure
remain unsuccessful attempts in the historical record.

[CI summary](evidence/bro-base-tools/ci.json) preserves the historical final run
[`36822925832`](https://github.com/bitrouter/bitrouter/actions/runs/36822925832)
at `fa18a82c`: all 22 jobs passed; Windows 3,487 passed, 3 skipped, zero flaky tests.
The preceding run passed with one flaky verification fixture. Its Windows
observation bound was raised to ten seconds; Unix stayed at three seconds, with
production timeouts and outcome assertions unchanged. Full job identities and
Windows lines, including earlier retries, are in the original raw archive.

This CI covers its named six-tool source. Windows uses controlled model fixtures
and actual PowerShell processes, not real-provider E2E. The failed macOS Windows
cross-check lacked SDK headers and is not successful platform evidence. Merge,
release and fresh integration CI are separate outcomes.

## Controlled comparison

Natural before/after runs share a prompt, initial fixture and upstream model;
the before binary is tied to snapshot `55dad4a7`. Both passed two independent tests.

| Metric | Seven tools | Six tools |
| --- | ---: | ---: |
| Model steps / calls | 6 / 7 | 6 / 8 |
| Elapsed seconds, including startup/check | 11.62 | 8.98 |
| First-step input tokens | 652 | 690 |
| Total input / output tokens | 5,920 / 312 | 6,710 / 329 |
| Cache-read tokens | 1,152 | 2,304 |

This single pair supports functional equivalence on the fixture. Cache/provider
scheduling and model choices prevent a causal efficiency claim. Fewer tool names
did not reduce the measured first-step prompt size.

## Reproduce

Use a configured inference gateway, a fresh short output directory and a rebuilt
binary. Provider calls consume real usage; offline archive verification does not.

```sh
cargo build -p bitrouter --bin bro --all-features
python3 scripts/probe-bro-base-tools.py --binary target/debug/bro \
  --output-dir /tmp/bro6-review-coding \
  --upstream-model openai/gpt-5.4-mini --scenario coding
```

The default gateway is `127.0.0.1:4356`; set `--api-base` for another unprotected
gateway and choose an upstream model selector that gateway accepts. Repeat with
`--scenario readonly` or `natural`. For historical before/after comparison, build
in separate target directories and verify declarations rather than trusting cache
reuse. The probe creates the tiny Python fixture; archived final files are outputs,
not additional source fixtures that must be maintained.

## Raw exports and remaining gates

[Evidence storage and verification](evidence/bro-base-tools/README.md) describe
the lossless archives, per-member SHA-256 index and original source snapshot.
Summaries/prompts stay readable; original model/execution records, event streams,
cleanup, final fixture files and CI detail are compressed. Uncommitted temporary
DB/full-stream paths are historical provenance, not downloadable evidence.

The tool contract owns argument, profile, path and interpreter rules. Runtime
acceptance owns restart/ownership guarantees. This record does not establish
operator recovery, native core integration, broad task performance, production
settlement or new UI/platform/provider acceptance beyond the named runs.

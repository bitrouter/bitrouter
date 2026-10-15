# Actual harness Responses boundary, 2026-10-14

This is a prerequisite for the real Goal/harness/provider acceptance, not the complete Goal workflow. No model or harness is mocked. The user-authorized OpenRouter key remains outside the repository; configuration references its environment name only.

## Actual configuration and failed preflight

- A copy-on-write snapshot of the current BRO binary starts a private daemon at loopback port 54311, with independent home, SQLite database, Codex home and project under `/tmp/bro-goal-live-eval-20261014/harness-v1`.
- The maintained ACP adapter is `@agentclientprotocol/codex-acp@1.10.0`, already cached; actual worker is `codex-cli 0.162.1`. BRO routes model selector `gpt-6.1-sol` to OpenRouter `openai/gpt-6.1-sol`, using Responses. Jev typed configuration uses the same authorized credential.
- Ordinary user configuration is not edited. Optional remote telemetry stays disabled. No subscription authentication is imported.
- [First actual readonly run](2026-10-14-harness-preflight-v1-failed.jsonl) reports a stream error instead of reading the marker from info.md. ACP adapter returns end_turn and CLI exit 0 despite that error; neither establishes task success. A second run with inherited desktop CODEX_* and OPENAI_BASE_URL removed [fails identically](2026-10-14-harness-preflight-v2-clean-failed.jsonl), so environmental inheritance is not established as the cause.
- The original [request report](2026-10-14-harness-preflight-v1-request-report.json) shows unavailable metering and no settled rows. It is not proof that no provider call happened or no charge accrued.

## Direct versus BRO actual HTTP evidence

[OpenRouter's official Responses guide](https://openrouter.ai/docs/api_reference/responses/overview) documents the stateless endpoint and rejection of stored/previous-response continuation. The probes explicitly set store=false, omit previous_response_id, use streaming and a 64-token output cap.

The same minimal request asking for exactly OK returns HTTP 200 in both paths. [Direct response](2026-10-14-responses-direct-before.sse) has a valid response.completed, actual usage and one trailing untyped `[DONE]`. [BRO response](2026-10-14-responses-bro-before.sse) delivers OK then generates response.failed with upstream_invalid_response. No tool definition or Codex client is needed to reproduce this boundary.

The captured upstream stream is also committed as a Rust protocol regression fixture. [RED](2026-10-14-responses-terminator-red.log) fails with “Responses stream emitted non-empty data after its terminal”. This identifies the decoder's treatment of the optional transport marker, rather than a failed or missing provider terminal.

## Correction and current scope

The decoder accepts one untyped/default-message `[DONE]` only after a validated successful terminal. It does not create success from the marker, release the terminal before EOF, accept duplicate markers or ignore later JSON/malformed data. Existing ID/status/header consistency and duplicate lifecycle checks remain. The encoder continues to emit the standard Responses lifecycle without `[DONE]`.

The former sentinel rejection case is moved to a captured positive case; duplicate terminals, unknown trailing events and garbage remain negative cases. New negatives cover early markers, repeats, named failure markers and new content after the marker. [63 Responses tests pass](2026-10-14-responses-terminator-green.log). New BRO build and actual HTTP/harness rechecks are required before claiming the live path works.

This does not yet prove two actual Goal branches, overlapping execution, current-Goal modification, dependency handoff, versioned verification, detach/reconnect or autonomous delivery. Those original acceptance gates remain open.

## Actual fixed network and harness recheck

The rebuilt BRO [returns a normal completed response](2026-10-14-responses-bro-after.sse) with OK and actual usage, without adding an outbound `[DONE]`. The [real maintained Codex ACP recheck](2026-10-14-harness-preflight-v3-fixed.jsonl) contains 50 events, including an actual file-read tool call/update, usage, the exact BRO_REAL_HARNESS_20261014 file marker and end_turn. This establishes the previously broken Responses transport and an actual harness/provider read, rather than just a green captured fixture.

## First actual Goal attempt: unfinished

The production TUI creates Goal `8f974923-6c6f-417a-9892-ddd2d1c4626c` in an isolated JavaScript project, with a 1,200-second elapsed limit and 20-USD managed model-token estimate limit. This is not an all-cost external-harness cap. Actual planning produces independent numbers/text module tasks and a dependent integration/documentation task; no plan or worker result is seeded.

Original contracts remain unchanged. Before either implementation exists, the [baseline](2026-10-14-real-goal-v1-baseline-contracts.log) intentionally has two TODO contracts; that exit 0 is not final verification. The Goal explicitly requires both modules, all original contracts executing with zero TODO/skipped tests, dependent artifact handoff and actual combined npm test evidence.

The attempt reveals two concrete failures:

- A numbers session actually exists under the original execution workspace, confirmed route and start time, while the Goal's mapping is Unknown/null after a SQLite database-locked commit failure. This demonstrates a missing reconciliation path after accepted dispatch, not proof the harness was never started.
- The text session returns a model_conversion_incompatible error as agent text. Its end_turn and process stop cannot count as successful implementation or checked handoff.

[Actual inventory](2026-10-14-real-goal-v1-session-inventory.json) and [paused Goal snapshot](2026-10-14-real-goal-v1-paused-snapshot.json) retain both facts. The original session start times overlap in admission, but successful parallel work and delivery are unproven. The test Goal is explicitly paused through the real TUI and detached; the discovered numbers session is [stopped using its existing run ID](2026-10-14-real-goal-v1-owned-stop.json). No unknown dispatch is replayed and no fabricated mapping/result is committed. The host Codex development Goal remains active.

[313 original Goal operations](2026-10-14-real-goal-v1-operations.json) remain available. Current work must recover the original dispatch identity, address the SQLite write race and inspect the real conversion report before progressing the same Goal. Full source127 checks are running independently with bounded test concurrency; their eventual status does not replace these actual workflow gates.

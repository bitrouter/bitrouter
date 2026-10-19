# Current-Goal resource edits and execution propagation

Reasoning task updates now accept an optional complete resource selection.
Omitted/null resources preserve the existing selection; an object replaces the
selection, including an empty object to remove it. Intrinsic selection bounds,
task scope and requirement versions are validated in the durable input
transition. The host also revalidates configured MCP IDs, advertised skill
IDs/versions and model route/control wiring before committing a proposal.
Invalid proposals produce a retained clarification without saving requirements
or controls. Runtime validation does not establish classification accuracy.

Resource edits use the existing dependency invalidation, stopping and local
effect reconciliation path. They immediately change the durable version and
limit old dispatch/handoff; replacement starts only at the next safe boundary.
The old execution and its supplied bytes remain historical facts. Independent
branches retain their task revision and run. Paused work does not resume.
Private in-flight context, private skill activation and actual MCP use are not
inferred from supplying new material or configuring a route.

The read-only task detail shows requested resources at the current task
revision alongside each execution's frozen supplied material and acknowledged
BRO route. All source/harness/provider identities remain evidence-bounded.

v1 run `35566123-cb22-49e4-8ed5-661bd005c093` passed 79 selected tests, exit 0,
115.087 seconds, two slow cases and no LEAK or timeout. The actual coordinator
case rejected an unavailable server while two branches existed, then saved a
LEFT task resource change. Its replacement carried revision 2, acknowledged
goal-model, the selected MCP descriptor, exact skill/context bytes and the
persisted prompt over ACP. The previous execution had confirmed stop; RIGHT
kept a single execution at revision 1. Actual files passed handoff/integration
verification. The fixture executable implements ACP and provider setup; model
and typed judgments remain fixed test judgments, not real-model quality.

v2 adds durable omit-versus-empty selection checks and shipped CLI/skill/plugin
documentation. It also includes the existing actual Goal PTY regressions.
Results are recorded against its independent source manifest. The remaining
input-quality, real typed backend/harness/provider and complete acceptance gates
remain open. No private harness hot-mutation capability is fabricated.

v2 run `4294d98e-a49d-4362-a92d-da051f5f8626` passed all 83 selected cases,
exit 0, 135.463 seconds, two slow cases and no LEAK/timeout. The durable case
verified invalid-path rollback, pause preservation, independent task scope,
omitted selection preservation and explicit empty selection removal across
successive versions. All four original actual Goal PTY cases and all original
coordinator scenarios passed. This remains a selected check (4,521 filtered),
not a full workspace or real-backend evaluation result.

The frozen v2 source subsequently passed the full workspace: run
`89a17a03-df57-4f8f-90d0-351c518516c4`, 4,582 passed, 22 existing opt-in skips,
71 binaries, 704.084 seconds, nine slow cases and no LEAK or timeout. The
compound validation session 8824 exited 0 after strict workspace/all-target/
all-feature Clippy, separate doctests (six passed, one existing ignored),
formatting, diff checks and all 102 source hashes. These results precede the
Windows patch-input repair; its source and follow-up checks are recorded
separately. This does not close the real-backend or complete acceptance gates.

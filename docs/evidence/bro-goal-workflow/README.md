# BRO Goal workflow final acceptance evidence

This directory keeps final acceptance summaries, source bindings, the two
unchanged input-evaluation fixtures and the material needed to reproduce the
controlled local workflow. Raw logs and intermediate evaluation snapshots are
held in an independent Git archive, not in the functional PR diff.

## Final evidence

- [Acceptance matrix: all 62 G/A/F IDs](2026-10-15-final-acceptance.md).
- [Full local check status](2026-10-15-main-goal-final-v1-status.json) and
  [original 1,023-file source binding](2026-10-15-main-goal-final-v1-source.sha256).
- [R16 actual ACP workflow](2026-10-15-real-goal-v16-summary.json),
  [delivery patch, checks and assessment](2026-10-15-real-goal-v16-delivery-proof.json),
  and [actual terminal reconnection](2026-10-15-real-goal-v16-ui-reconnect.json).
- [Final 54 corpus + 18 sequence records](2026-10-15-input-dependency-proof-v1-summary.json),
  [separate semantic audit](2026-10-15-input-semantic-audit.json), and
  [source applicability](2026-10-15-input-final-source-applicability.json).
- [Windows helper applicability](2026-10-15-windows-test-cfg-applicability.json)
  and [native path fixture applicability](2026-10-15-windows-fixture-path-applicability.json).
- [Reproduction instructions](REPRODUCE.md), [original project inputs](reproduce-project.json),
  [relocatable connection configuration](reproduce-config.yaml),
  [corpus fixture](input-eval-cases.jsonl), and [sequence fixture](input-eval-sequences.jsonl).

## Independent archive

- Immutable commit: [`382c2b58c5c25b02805175a33d60707dc4227b46`](https://github.com/bitrouter/bitrouter/tree/382c2b58c5c25b02805175a33d60707dc4227b46).
- Retention branch: `codex/pr981-evidence-archive-20261015`.
- [Provenance](https://github.com/bitrouter/bitrouter/blob/382c2b58c5c25b02805175a33d60707dc4227b46/PROVENANCE.json) and [per-file SHA-256 list](https://github.com/bitrouter/bitrouter/blob/382c2b58c5c25b02805175a33d60707dc4227b46/SHA256SUMS).
- Checksum-list SHA-256: `b9ee7aee0a4b375f0239f8f692f6c932b9e7f23bf31fb2a0d6c20b993db95c36`.
- Original payload: 890 evidence files plus the original implementation ledger,
  64181265 bytes, preserving exact committed bytes and modes.
- [Complete historical implementation ledger, including D01-D262](https://github.com/bitrouter/bitrouter/blob/382c2b58c5c25b02805175a33d60707dc4227b46/docs/BRO_GOAL_ORIENTED_IMPLEMENTATION.md).

[archive.json](archive.json) pins the archive commit, tree, source commit, count,
byte size and checksum-list digest. The [archive README](https://github.com/bitrouter/bitrouter/blob/382c2b58c5c25b02805175a33d60707dc4227b46/README.md) gives
extraction and verification commands. Historical failed runs, response-format
errors and superseded results remain available there; they are not relabeled
as successful final acceptance.

A bare historical filename in a retained JSON summary resolves under
`docs/evidence/bro-goal-workflow/` at the archive commit when it is absent here.
Original manifests bind their original evaluated source, not this later
documentation cleanup. No credentialed campaign or full local suite was rerun
merely to move evidence. Source/test code and the two compiled-in JSONL
fixtures remain unchanged.

Hosted CI for the pre-cleanup functional head `4772d9e09deb9c69f539e97d2b24dda9ca770e0c`
[completed successfully](https://github.com/bitrouter/bitrouter/actions/runs/38737081691).
Checks for later PR heads are separate and must be read at that head.

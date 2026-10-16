# ACP never-started metadata checkpoint

Local only; PR #981 is not pushed and the refactor Goal is incomplete. This is not private ACP session restoration.

- red1: real SQLite stopped-owner metadata reopen refused as private session continuation; six-file failure image retained and independently verified.
- green2: real DB/Rust peer case, 32 shared recovery/worker cases, strict Clippy and fmt passed.
- green3: live owner refusal, 3 real-DB repeats, strict Clippy/fmt passed.
- follow4: 11 Goal Thread/admission cases passed on identical green3 source.
- green5: shared setup plus submitted/unknown prompt refusal; 3 ACP guard cases, strict Clippy/fmt passed. Production unchanged from green2.

All raw logs, exact source maps/tars, configurations, failure image audit, and runners remain separate from the feature PR. No LEAK or source drift in these runs. Final post-test edits are docs/skills/plugin descriptions only. ACP sessions with an accepted Turn remain unconfirmed, never replayed. Final full-source suite, remaining ACP same-session qualification, remote evidence publication and hosted CI remain pending.

# Reproduce the controlled local Goal MVP

Run from the BitRouter repository root. Rust/Git are required for repository
checks; the real harness workflow also requires Node/npm, network access and
working OpenRouter credentials for the historically configured model IDs.
Provider availability and a new model result are external prerequisites, not
guaranteed by the recorded R16 success. Paid commands below are opt-in.

## Repository checks without provider credentials

```sh
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=1
export CARGO_PROFILE_TEST_DEBUG=1
export CARGO_BUILD_JOBS=2
cargo build -p bitrouter --bin bro
cargo nextest run --workspace --all-features
cargo test --workspace --all-features --doc
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo fmt -- --check
git diff --check
```

Use `cargo test --workspace --all-features` when nextest is unavailable.
Explicitly ignored credentialed/long vendor tests remain opt-in; a skipped
test is not proof that its external campaign passed. The original final run
recorded 4,675 passes and 24 existing opt-in skips, plus 6 passed doctests and
one existing ignored doctest. Later test-only Windows corrections have their
own applicability summaries and hosted CI.

## Materialize the original R16 project

The data bundle contains the exact five original project inputs, checked
against `original_input_hashes` in the retained R16 summary. It contains no
delivered implementation, private profile or credential.

```sh
export BRO_GOAL_REPRO_HOME="$(mktemp -d /tmp/bro-goal-repro.XXXXXX)"
node --input-type=module <<'NODE'
import { readFileSync, mkdirSync, writeFileSync, copyFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { createHash } from 'node:crypto';
const root = 'docs/evidence/bro-goal-workflow';
const home = process.env.BRO_GOAL_REPRO_HOME;
const project = join(home, 'project');
const bundle = JSON.parse(readFileSync(join(root, 'reproduce-project.json'), 'utf8'));
for (const file of bundle.files) {
  if (!/^(AGENTS\.md|info\.md|package\.json|tests\/(numbers|text)\.test\.mjs)$/.test(file.path)) throw Error('Unexpected input path');
  if (createHash('sha256').update(file.utf8).digest('hex') !== file.sha256) throw Error('Input checksum mismatch');
  const target = join(project, file.path);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, file.utf8, { flag: 'wx' });
}
mkdirSync(join(home, 'claude-profile'), { recursive: true });
copyFileSync(join(root, 'reproduce-config.yaml'), join(home, 'bitrouter.yaml'));
NODE
git -C "$BRO_GOAL_REPRO_HOME/project" init
git -C "$BRO_GOAL_REPRO_HOME/project" -c user.name='BRO acceptance' -c user.email='bro-acceptance@example.invalid' add .
git -C "$BRO_GOAL_REPRO_HOME/project" -c user.name='BRO acceptance' -c user.email='bro-acceptance@example.invalid' commit -m 'test: seed goal reproduction contracts'
```

The provided configuration relocates the original connection shape to a new
SQLite database and a fresh Claude profile. It uses loopback port 54322,
the pinned Claude ACP adapter 0.75.1 and the historical BRO routes. Ensure
that port is available. No old database, sessions or private profile is copied.

## Real workflow (model calls incur charges)

Set `OPENROUTER_API_KEY` privately in the Terminal environment; never commit
its value. Then launch from an interactive terminal:

```sh
BITROUTER_HOME="$BRO_GOAL_REPRO_HOME" ./target/debug/bro code \
  --config "$BRO_GOAL_REPRO_HOME/bitrouter.yaml" \
  --workspace "$BRO_GOAL_REPRO_HOME/project"
```

Directory reads do not create work. Press `n`, then paste the complete
`original_goal` from `2026-10-15-real-goal-v16-summary.json`. Retain the
original tests and explicit no-merge/no-deploy constraints. Set a deliberate
test budget before submitting. When two actual branches overlap, submit:

```text
Keep the numbers branch unchanged. Update only the text branch and its dependent
integration: add // BRO_TEXT_CHANGE_ADOPTED_R16 to text.mjs without changing its
public behavior or the original tests. Add an actual BRO check for this marker
and retain all original npm checks. Do not pause the Goal.
```

A fresh goal and execution identities are expected. Check actual requirements
version2, unchanged independent numbers execution, stopped/reconciled old text
execution, replacement text prompt/file adoption, and the persisted additive
marker check. Dependent integration must receive both checked handoffs.
Completion requires an actual combined `npm test` report with 2 passed,
0 failed, 0 skipped and 0 TODO, all current reviews, and the complete Goal
assessment. A saved edit or route acknowledgment alone is insufficient.
Inspect the delivery workspace from Ctrl-I; the original project stays clean.

Ctrl-Q detaches without pausing. Reopen with the same home/config and confirm
the same Goal, input, execution and requirements identities without duplicate
work. Ctrl-O reads persisted history; the right-side Workflow stays read-only.
Unknown effects or missing evidence must remain unfinished, not be forced
complete. The retained summary/proof/reconnect files show the original run;
following these steps is a new campaign, not replay of its private sessions.

## Real input evaluation (separate from harness dispatch)

The following existing ignored tests call real typed/reasoning backends over
durable fixture Goal state; they do not dispatch a live ACP workflow. Run
sequentially, with credentials in the environment and a fresh private output
directory per campaign:

```sh
BRO_GOAL_LIVE_EVAL_OUTPUT="$(mktemp -d /tmp/bro-input-corpus.XXXXXX)" \
  cargo test -p bitrouter --lib --all-features \
  live_input_corpus_preserves_scope_permissions_and_durable_feedback -- --ignored
BRO_GOAL_LIVE_EVAL_OUTPUT="$(mktemp -d /tmp/bro-input-sequences.XXXXXX)" \
  cargo test -p bitrouter --lib --all-features \
  live_continuous_goal_inputs_use_persisted_state_and_durable_effects -- --ignored
```

Keep outputs private and retain failures. q03/q07 are human-reviewed;
remaining labels are drafts. Persisted forbidden effects, native predictions,
reasoning proposals and semantic quality are different measurements. All
current typed inputs upgrade; no calibrated typed fast-path benefit is claimed.

## BRO execution refactor

Use [current refactor acceptance](2026-10-16-refactor-acceptance.md) for the complete workspace command, focused original-session load case and current limits. [Final summary](2026-10-16-refactor-summary.json) pins the evaluated source inputs and independent raw archive. Current fixtures are Cargo-built Rust; BRO_GOAL_FIXTURE_KEEP_FAILURES=1 preserves subsequent failures when supported. Historical database/filesystem copies are forensic evidence and do not imply arbitrary process/state takeover.

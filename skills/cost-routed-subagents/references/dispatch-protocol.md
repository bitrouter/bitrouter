# Dispatch protocol: controller loop, status, two-stage review

The controller (your flagship session) never lets a worker wander. It curates the
worker's entire context, dispatches a focused task, then reviews. Workers are
**one-shot** `claude -p` processes: they cannot ask mid-task questions, so context
must be complete up front, and "I need more" comes back as a status, not a prompt.

> Methodology adapted from obra/superpowers (MIT) — `subagent-driven-development`
> and `dispatching-parallel-agents`. See [attribution.md](attribution.md).

## The controller loop

For each independent sub-task:

1. **Curate context.** Paste the full task text and the scene-setting the worker
   needs (where it fits, constraints, the interface it must honor). Do **not** tell
   the worker to "read the plan" — it has no access to this conversation.
2. **Pick a tier.** See [model-tiers.md](model-tiers.md). Implementer →
   `cheap`/`standard`. Reviewers → strong tier.
3. **Dispatch the implementer** into an isolated git worktree:
   `./dispatch.sh --tier cheap --role implementer --task task.md --dir "$WORKTREE"`.
4. **Handle the status** (below).
5. **Two-stage review** once the implementer reports `DONE`: spec compliance first,
   then code quality. Re-dispatch fixes until both pass.
6. **Integrate.** The controller reads the diff and merges. The controller is the
   final reviewer — never skip this.

Parallelism: independent sub-tasks can run as concurrent `dispatch.sh` background
jobs (different worktrees). Do **not** run parallel writers in the *same* directory.

## Worker status protocol

Every worker (implementer or reviewer) ends with one of:

| Status | Meaning | Controller action |
|---|---|---|
| `DONE` | Completed and self-reviewed. | Proceed to review (implementer) / accept verdict (reviewer). |
| `DONE_WITH_CONCERNS` | Completed but flagged doubts. | Read the concerns; address correctness/scope before review; note observations. |
| `NEEDS_CONTEXT` | Missing information it couldn't obtain. | Provide the missing context; **re-dispatch** (this replaces a mid-task question). |
| `BLOCKED` | Cannot complete. | Re-dispatch one tier up, split the task smaller, or escalate to the human. Never silently retry the same model unchanged. |

## Two-stage review (after implementer `DONE`)

1. **Spec compliance** — `./dispatch.sh --tier standard --role spec-reviewer …`.
   Does the change implement exactly the spec — nothing missing, nothing extra?
   Issues → implementer fixes → re-review. Do **not** start stage 2 until stage 1
   is clean.
2. **Code quality** — `./dispatch.sh --tier standard --role quality-reviewer …`.
   Naming, structure, tests-verify-behavior, YAGNI, follows existing patterns.
   Issues → implementer fixes → re-review.

Then the controller does the final integration review on the diff.

## Why two stages and not one

Spec compliance and code quality fail in different ways and a single pass tends to
privilege one. Separating them keeps "did we build the right thing?" distinct from
"did we build the thing right?", and keeps each reviewer's prompt small.

## Hard rules

- Never start implementation on `main`/`master` without explicit human consent.
- Provide full task text — never make a worker read the plan file.
- Don't skip either review, and don't move on while a review has open issues.
- A reviewer finding issues means **not done** — the implementer fixes, then the
  reviewer re-reviews.
- Don't dispatch parallel implementers into the same directory.
- The controller's final diff review is mandatory, especially for `cheap`-tier work.

A Codex loop spends most of its calls on steps that do not need a frontier
model: reading a file back, checking whether a command succeeded, restating what
just happened. This recipe keeps GPT-5.5 on the turn that plans, and lets
BitRouter learn which of the routine follow-up steps can run on Kimi K2.7 Code
instead.

## How it routes

The routing key is the **step in the loop**, not the model name. `opening` — the
first turn, before any tool has run — stays on the flagship, because that is
where the plan is formed. `after_bash`, `after_exec_command`, and `midstream`
drop to the cheap tier. Anything the table does not name falls back to the
flagship, so an unfamiliar step never silently downgrades.

Two guardrails keep the downgrade from costing more than it saves:

- **Tool calls stay flagship.** A request carrying tools is clamped up to a
  tier known to handle tool calls reliably, so a downgrade never strands a tool
  call on a model that fumbles it.
- **A failing downgrade escalates itself.** With `adequacy.enabled`, a
  fingerprint whose downgraded route hard-fails is pinned back to the flagship
  for 30 minutes, then re-tried. The table is a starting point that the daemon
  corrects while it runs.

## Where the numbers come from

The measured run is [experiment 001](https://github.com/bitrouter/bitrouter/blob/main/benchmarks/001-2026-07-10-tbench-v2.1-codex-gpt55-kimi-k27.md):
Terminal-Bench 2.1, an 88-task comparable set, **one attempt per task**. Against
a GPT-5.5-only control, the learned policy round replaced 373 strong calls with
219 Kimi calls — a replacement, not exploration piled on top — cutting total
tokens 21.2% and zero-cache imputed cost 32.8%, while scoring one task lower.

Read the report's limitations before citing it. In short: this is a mechanism
study under a modified protocol, not a Terminal-Bench leaderboard submission;
the cost figures are reproducible zero-cache upper bounds (a post-run audit
places the reduction at 28.6–32.8% under equal cache-read shares); and at one
attempt per task a one-task score difference is within noise.

## Tuning it

The fingerprints are the part worth editing. Start by moving one step at a time
into `cheap` and watching `bitrouter route` and your own eval — a fingerprint
that is broad enough to cover two different kinds of step will average out to a
worse decision on both.

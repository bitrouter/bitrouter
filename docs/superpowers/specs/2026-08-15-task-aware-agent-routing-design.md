# Task-Aware Agent Routing Design

**Date:** 2026-08-15

**Status:** Approved for implementation

## Context

BitRouter's auto-router currently predicts the next workflow role and risk, then
resolves a three-tier model policy through keys shaped as
`agent_route/v1|<role>|<risk>`. This captures where an agent is in its workflow
and how cautiously the next request should be handled, but it deliberately does
not describe the semantic kind of work being performed.

OpenRouter's current Auto Router classifies prompts into roughly thirty task
types before applying a cost band and market ranking. Its published Code and
Agents rows demonstrate useful semantic granularity, while also mixing semantic
domains such as debugging with workflow actions such as shell execution and
tool dispatch.

The next BitRouter iteration adds a semantic task-family axis without replacing
the existing role, action, risk, or three-tier model policy. It does not add a
five-band cost control or a learned classifier.

## Goals

- Distinguish common Code and Agents workloads at a granularity comparable to
  the useful semantic rows in OpenRouter's matrix.
- Keep classification deterministic, local, bounded, explainable, and free of
  an additional model request.
- Preserve the current workflow role and risk semantics.
- Let task-specific evidence override the role baseline sparsely rather than
  requiring a Cartesian product of task, role, and risk routes.
- Expose the predicted task family and evidence in routing decisions and eval
  settlement data.
- Preserve old locks and old observed-trace fallbacks.

## Non-goals

- Copy OpenRouter's model matrix or five cost tiers.
- Treat shell execution, file I/O, or tool dispatch as semantic task families;
  the existing action and role axes already represent those operations.
- Introduce a neural classifier, remote classifier, prompt retention, or mutable
  per-session classifier state.
- Pre-populate every possible task × role × risk route.
- Claim model/task fitness without benchmark evidence.

## Canonical Task Families

The first version has twelve routable families plus `unknown`.

| Domain | Canonical key | Meaning |
| --- | --- | --- |
| Code | `code:generation` | Implementing, extending, or refactoring code |
| Code | `code:debugging` | Diagnosing and fixing failures or regressions |
| Code | `code:review` | Reviewing code, diffs, pull requests, or security properties |
| Code | `code:sql_database` | SQL, schema, migration, query, and database work |
| Code | `code:frontend_ui` | Frontend, UI, CSS, DOM, and visual component work |
| Code | `code:devops_config` | Deployment, CI, infrastructure, and service configuration |
| Code | `code:repository_analysis` | Repository discovery, codebase analysis, and dependency tracing |
| Agents | `agent:multi_step_planning` | Planning, decomposition, and architecture of multi-step work |
| Agents | `agent:workflow_execution` | Agent orchestration, pipelines, handoffs, and workflow control |
| Agents | `agent:web_research` | Web research, source gathering, and current-information lookup |
| Agents | `agent:memory_operations` | Memory extraction, context synthesis, and durable fact handling |
| Agents | `agent:general` | Agent work with clear agent semantics but no narrower family |
| Fallback | `unknown` | Insufficient or conflicting causal evidence |

The classifier maps OpenRouter's Shell execution, File I/O, and Tool dispatch
rows to BitRouter's existing `NextActionClass` and `NextStepRole` axes. This
avoids classifying the same request twice under two competing meanings.

## Routing Key and Fallback

A confident task prediction produces this primary key:

```text
agent_route/v2|<task-family>|<role>|<risk>
```

The resolver checks candidates in this order:

1. exact task-aware v2 key;
2. the existing predictive v1 `agent_route/v1|<role>|<risk>` key;
3. observed `agent_trace/v2` key;
4. observed `agent_trace/v1` compatibility key;
5. policy default.

An `unknown` task family uses the v1 key as its primary key. A policy therefore
opts into task-aware behavior by adding only the v2 cells supported by its eval
evidence; every other request retains the proven role baseline. Existing v1
locks continue to work without migration.

Both predictive namespaces are bound by the same signed predictor contract.
The contract digest changes when task-family behavior is introduced, so a lock
cannot silently pair a new binary with old classifier semantics.

## Deterministic Classifier

Task-family inference is part of the compiled predictive scorecard. It uses only
the bounded causal prompt/history already admitted by the workflow predictor.
It does not read private routing headers, harness names, generated task labels,
or future tool results.

The classifier uses literal feature groups with explicit weights and a stable
tie order. Examples include:

- failure, exception, regression, and fix signals for debugging;
- review, audit, diff, pull request, and vulnerability signals for review;
- SQL, migration, schema, and database-engine signals for SQL/database;
- frontend frameworks, CSS, DOM, screenshot, and component signals for UI;
- deployment, container, CI, infrastructure-as-code, and service configuration
  signals for DevOps/config;
- repository, codebase, dependency, locate, and scan signals for repository
  analysis;
- plan, decompose, architecture, workflow, handoff, browse, source, memory, and
  context signals for the agent families.

Specific families outrank generic generation or general-agent signals. A family
is emitted only when its score, evidence coverage, and margin clear compiled
thresholds. Otherwise the classifier returns `unknown` and routing remains v1.
The output includes bounded reason codes, a heuristic confidence value, and the
compiled predictor digest. Raw prompt text is never written to decision or eval
records.

## Data Model and Observability

`PredictiveRouteIR` gains:

- `task_family`;
- `task_family_confidence`;
- bounded `task_family_evidence`.

The fields have backward-compatible serde defaults. `PolicyDecision`, durable
decision records, and eval settlement attributes expose the categorical family
and confidence. The primary `route_projection` retains the v2 key even when the
matched `request_key` falls back to v1, making classification and policy match
separately observable.

Decision summaries aggregate by predicted task family in the same way they
already aggregate by predicted role. Replay recomputes the task family from the
same causal fixture and rejects divergent projections.

## Policy and Optimization

Task-aware routes remain ordinary exact policy-lock routes. Consequently the
existing compiler, route certificates, shadow evaluation, reward feedback, and
optimizer can collect evidence independently for each v2 cell.

The official auto-router template remains a three-tier policy. Its v1 matrix is
the complete baseline; a small set of v2 entries demonstrates sparse overrides
where the selected tier is intentionally different. Adding or changing an
override requires the same route evidence and predictor-contract checks as any
other predictive route.

This design also gives future optimizers a safe cold-start rule:

- use the task-specific cell when it has admissible evidence;
- otherwise inherit the role/risk baseline;
- promote a new task-specific cell only through the existing eval and lock
  publication workflow.

## Compatibility and Failure Behavior

- Old `agent_route/v1` policies are accepted and behave exactly as before.
- New policies may mix v1 baseline cells and sparse v2 override cells.
- Unknown, low-margin, incomplete-history, or contradictory classifications
  fall back to v1.
- Malformed v2 keys are rejected rather than coerced.
- Predictor contract admission applies whenever either v1 or v2 predictive keys
  are present.
- Explicitly routed model requests and server-tool guardrails keep their current
  precedence.

## Verification Strategy

Implementation follows test-driven development and covers:

1. exact v2 key parse/round-trip and malformed-key rejection;
2. literal prompts for every task family plus ambiguous/contradictory fallback;
3. invariance across Codex, Claude, Terminus, private headers, and task labels;
4. exact v2 → predictive v1 → observed v2 → observed v1 → default fallback;
5. decision, summary, replay, and eval-settlement propagation;
6. predictor-contract admission for mixed v1/v2 locks;
7. official template compilation and sparse override behavior;
8. repository-wide format, clippy, and all-feature test gates.

## Source Note

The comparison is based on OpenRouter's official Auto Router documentation as
observed on 2026-08-15. The documentation describes a lightweight classifier,
roughly thirty task types, five cost bands, trailing seven-day spend-share
rankings, and ranked fallbacks. BitRouter adopts the useful semantic granularity
while retaining its deterministic workflow/evidence architecture.

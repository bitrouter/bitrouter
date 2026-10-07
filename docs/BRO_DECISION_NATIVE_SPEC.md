# Unified decision-native routing

PR #961 on `codex/decision-native` extends #956 and #945. Request and Core
session are input adapters to one routing pipeline. Their available signals,
context authority and constraints determine what that pipeline can do. No
HTTP/Core mode chooses a different classifier or routing algorithm.

## Pipeline and ownership

1. SDK resolves the original named router, defaults, authentication and request
   checks and freezes the named-policy snapshot before semantic inference. Router
   identity survives model selection.
2. The input owner projects bounded public instructions, conversation and
   ordinary tool exchanges into `routing::input::Input`. Recent history has
   priority within the bound. Private reasoning, provider metadata and opaque
   continuation state do not enter the semantic backend. Core also supplies
   explicit task facts and immutable evidence references. Missing history is
   not declared complete.
3. One System One batch classifies task family, next execution role and progress.
   Core can append independent evidence-representation questions to that batch.
   The decoder validates the complete response and abstains below the frozen
   threshold. It retains raw distributions separately from accepted labels.
4. App's named-policy selector maps the shared assessment to a canonical
   `semantic_route/v1|<task-family>|<role>|<risk>` key. Exact task cells precede
   the unknown-family baseline and then the configured default. Tool, progress
   and continuation constraints operate on this frozen policy snapshot.
5. A policy action owns `{model, effort?, context}`. The SDK's shared planner
   chooses among the context owner's admitted views for that action. Context
   strategy is independent of authority: `preserve` keeps the source view;
   `evidence` admits the owner's offered representations within granted rights.
   Candidate envelopes cannot change instructions, tools or request controls.
   Context checks run again if the selected message history changes.
6. Core acknowledges the selected plan before execution. The existing SDK
   provider router then resolves the effective model to physical endpoints,
   applies capacity/admission checks, and owns execution, fallback and settlement.
   A route hook cannot change the frozen model, context or request parameters.
7. Request and trajectory settlement both export shared semantic and exact-plan
   receipts to the existing Eval Exchange. External evaluators supply quality;
   observation alone never grants a pass verdict or edits the active lock.

The SDK owns source-independent projection, classification, capability and view
planning. App owns policy locks, experiments, guards, Eval and publication. Core
owns tasks, evidence provenance, source revisions, materialization and durable
ACKs. Neither Core nor a decision provider maintains a second generation-model
catalog. The production planner consumes the model action admitted by the named
policy; the provider catalog remains authoritative for physical execution.

A rich API request can infer workflow state. A Core task can still lack enough
information to classify it. Neither fact grants context mutation permission.
An HTTP host may offer the same representations and capabilities as Core through
trusted SDK preparation; request text and headers cannot grant those rights.

## Decision backend

`DecisionExecutor` is the backend-independent typed boundary. Choice, Score and
Noul responses have strict rubric and usage validation. Shared routing currently
uses Choice heads; their reported probabilities are semantic evidence, not
policy action probabilities or calibrated quality scores.

The shipped HTTP adapter speaks TypeSafe's protocol. Jev and other backends can
implement `DecisionExecutor` without changing the router. A custom URL works
without an adapter only if the remote service implements the same wire protocol.
The built-in executor bounds time and response size and never retries an
uncertain paid attempt. Backend configuration is fixed at assembly and changing
`decision_model` requires a daemon restart.

An assessment commits to public input, rubric, requested and actual backend model,
and the confidence threshold. Policy configuration and model capacity remain
independent constraints. No valid assessment means explicit unknown labels and
conservative context, subject to ordinary capacity failure.

## Durable context

Core stores immutable tool-exchange groups, exact UTF-8 extracts, source-bound
historical summaries and task-owned evidence inventories. Protected instructions
and tool protocol integrity cannot be removed by a classification result.
Workers receive admitted evidence references, build independent views and return
conclusion references. Foreign tool calls render as historical data rather than
another worker's live protocol. Recall reads the original admitted evidence.

Core commits decision intent before dispatch, then outcome and usage before
applying a view. Source revision changes invalidate pending work. Recovery reuses
acknowledged outcomes; an interrupted attempt remains uncertain and is not
silently retried. Applied views and shared plan receipts commit to the exact
canonical prompt, and replay checks that commitment against the stored view.

The native Thread/Turn/Item service uses the same Core adapter and retains local
workspace tools, approval, cancellation, artifacts and user-facing events. Large
tool bodies are persisted before references are exposed. Capacity limits remain
explicit; this implementation does not promise unlimited history.

## Learn and publication

The existing request/episode/task subject and evaluator result protocol is shared.
Semantic receipt IDs deduplicate reused decisions, while each executed plan has
its own request identity and prompt commitment. Trajectory outbox publication
retains those same receipts and rejects conflicting reuse of one receipt ID.

Route measurement schema 2 includes context strategy in action and candidate-set
identity. Logging propensities describe policy assignment before deterministic
guards. Semantic distributions, action propensities and evaluator quality are
stored separately. Request completion is diagnostic, not independent task
quality. Overlapping task/episode attribution and repeated assignment units must
not create additional independent samples. Mixed classifier cohorts cannot
justify one promotion; backend revision, rubric and threshold define the cohort. Compiled route
certificates bind that cohort, and a mismatch falls through to the policy default.
Semantic evidence must also include its measured model/effort/context catalog.

Quality evaluation and policy publication remain separate. The lock alone
controls serving; no legacy adequacy rows mutate live routes or migrate into
new semantic evidence.

## Breaking contracts

Only policy lock version 4 is accepted. Tier targets are structured with required
`model` and `context`, and optional `effort`. Version 1–3 locks, scalar targets,
the global `policy_table` entry, the `fingerprints` lock alias, legacy migration
certificates and `decision_model.policy.generation_models` are rejected.

The starter template contains a conservative strong default and no relabeled
scorecard experiments. Offline scorecard analysis remains research tooling;
serving uses the shared decision assessment. Validation for this revision is
tracked in the PR; benchmark experiments are outside its scope.

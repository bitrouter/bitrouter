# Generalized Agent Trace Router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the workflow- and harness-specific policy key path with a native-request-to-agent-trace adapter boundary and a source-independent routing projection, while preserving every currently supported agent adapter and the existing adequacy learning loop.

**Architecture:** Protocol ingress supplies a canonical `Prompt`, native headers, and protocol metadata. A registry of trace adapters may recognize runtime-specific wire evidence and enrich a common `WorkflowStateIR`, but the router, adequacy ledger, reward import, and policy lock consume only a versioned `RouteProjection`. Runtime source, protocol, workflow names, roles, raw tool names, and evaluator identity remain diagnostic evidence and never participate in the policy key. Existing `workflow_state` lock files remain readable as a legacy spelling, but all newly generated locks use `agent_trace`.

**Tech Stack:** Rust, Serde, the existing BitRouter SDK canonical prompt pipeline, existing policy lock and adequacy ledger, Cargo nextest/test, Clippy, rustfmt.

## Global Constraints

- Product code and comments MUST be English and MUST NOT reference private workspace documents or closed-source services.
- No route decision may depend on `x-bitrouter-harness`, `x-superpowers-*`, a workflow name, a subagent role, a task ID, a review kind, or a caller-supplied complexity label.
- Runtime-specific parsing MUST live behind the trace-adapter boundary; policy, adequacy, reward, and evaluation code MUST consume only generic IR/projection fields and stable request/episode identities.
- Native evidence MUST take precedence over legacy compatibility hints. Missing or conflicting evidence MUST fall back to generic/unknown behavior rather than a confident runtime guess.
- Codex, Claude Code, Hermes, OpenClaw, Terminus 2, Smithers-originated traffic, and generic OpenAI-compatible traffic MUST remain parseable. No private BitRouter header may be required for routing.
- The active policy remains deterministic and immutable during a request. Observe/reward paths may update the existing adequacy ledger but MUST NOT rewrite a policy lock.
- `@auto` and `@auto:cost` use the existing `@preset[:variant]` grammar. Physical model names remain passthrough.
- The core runtime MUST add no Node, Bun, Python, Smithers, Redis, Postgres, Docker, remote evaluator, or remote control-plane dependency.
- Follow `AGENTS.md`: no `#[allow]`, no public-module re-exports, no `.unwrap`/`.expect`/`panic!` in production code, no dead code, conventional commits under 60 characters.
- Every behavior change follows RED → GREEN → REFACTOR. Tests assert observable behavior and hand-derived values, not source text or mocks.

---

## Task 1: Define the source-independent route projection

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/ir.rs`
- Modify: `crates/bitrouter-sdk/src/config/mod.rs`
- Modify: `crates/bitrouter-sdk/src/config/tests.rs`
- Modify: `apps/bitrouter/src/policy_lock.rs`
- Test: unit tests in the files above

**Interfaces:**
- Consumes: existing `WorkflowStateIR`, `WorkflowStateKind`, `RecoverySignal`, `CapabilityConstraints`, and `PolicyKeyStrategy`.
- Produces: `RouteRisk`, `RouteProjection { schema_version, state_kind, risk }`, `WorkflowStateIR::route_projection()`, `RouteProjection::key()`, and `PolicyKeyStrategy::AgentTrace` with `workflow_state` as a deserialize-only compatibility alias.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteRisk { Normal, Guarded }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteProjection {
    pub schema_version: u8,
    pub state_kind: WorkflowStateKind,
    pub risk: RouteRisk,
}

impl RouteProjection {
    pub fn key(&self) -> String {
        format!("agent_trace/v{}|{}|{}", self.schema_version, self.state_kind, self.risk)
    }
}
```

- [ ] **Step 1: Write failing projection tests.** Add literal assertions proving semantically equivalent Codex, Claude Code, Hermes, Terminus 2, OpenClaw, Smithers, and generic IR values produce the same key; changing `harness_id`, `protocol`, `active_workflow`, `subagent_role`, or `last_tool_name` must not change it. Assert normal `edit`, `test`, and `tool_followup` keys are exactly `agent_trace/v1|<state>|normal`; recovery, high context pressure, or high redo penalty produces `agent_trace/v1|<state>|guarded`.
- [ ] **Step 2: Run `cargo test -p bitrouter --all-features workflow_state::ir::tests` and record the expected RED caused by missing projection types/methods.**
- [ ] **Step 3: Implement the minimal projection.** `RouteRisk::Guarded` applies when recovery is likely, state is `Unknown`, `Debug`, `Review`, `Recovery`, or `Finalization`, context pressure is high, expected redo penalty is high, or output precision is high. All remaining states are normal. Keep the existing detailed `routing_key()` only as an explicitly named legacy method for reading old evidence; make the active path call `route_projection().key()`.
- [ ] **Step 4: Add config compatibility tests.** `key_strategy: agent_trace` serializes/deserializes as `agent_trace`; `key_strategy: workflow_state` deserializes to `AgentTrace`; generated schema advertises only the new canonical spelling.
- [ ] **Step 5: Update lock parsing/validation so old locks remain readable and new frozen locks emit `agent_trace`. Run the focused SDK, IR, and policy-lock tests until GREEN.**
- [ ] **Step 6: Commit `refactor(policy): add agent trace projection`.**

## Task 2: Make native trace adapters the only runtime-specific parser

**Files:**
- Modify: `apps/bitrouter/src/workflow_state/extractors.rs`
- Modify: `apps/bitrouter/src/workflow_state/online.rs`
- Modify: `apps/bitrouter/src/workflow_state/extractors/codex.rs`
- Modify: `apps/bitrouter/src/workflow_state/extractors/claude_code.rs`
- Modify: `apps/bitrouter/src/workflow_state/extractors/hermes.rs`
- Modify: `apps/bitrouter/src/workflow_state/extractors/openclaw.rs`
- Modify: `apps/bitrouter/src/workflow_state/extractors/smithers.rs`
- Modify: `apps/bitrouter/src/workflow_state/extractors/terminus_2.rs`
- Modify: `apps/bitrouter/src/workflow_state/session.rs`
- Modify: `apps/bitrouter/src/workflow_state/real_trace.rs`
- Test: unit tests in `extractors.rs`, `online.rs`, and each adapter

**Interfaces:**
- Consumes: `ExtractorInput { protocol_hint, headers, raw_body, prompt, harness_hint }`, where `harness_hint` is compatibility evidence only.
- Produces: `TraceAdapterMatch { source: HarnessId, confidence: f32, evidence_kind: &'static str }`, `detect_trace_adapter(&ExtractorInput)`, and `OnlineWorkflowState` whose `routing_key()` is always the Task 1 projection key.

```rust
pub struct TraceAdapterMatch {
    pub source: HarnessId,
    pub confidence: f32,
    pub evidence_kind: &'static str,
}

pub trait WorkflowStateExtractor {
    fn detect(&self, input: &ExtractorInput<'_>) -> Option<TraceAdapterMatch>;
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR;
}
```

- [ ] **Step 1: Write failing table tests for adapter detection without private headers.** Cover Claude Code via `anthropic-beta`, Codex via Responses plus Codex user-agent or `previous_response_id`, Hermes via native metadata/user-agent, OpenClaw via `agentRuntime`, Terminus 2 via its official prompt contract, Smithers-originated traffic via native Smithers metadata, and generic fallback. Include conflicting evidence and prove native evidence wins over `x-bitrouter-harness`.
- [ ] **Step 2: Run `cargo test -p bitrouter --all-features workflow_state::extractors` and capture the expected RED.**
- [ ] **Step 3: Implement deterministic adapter detection in `extractors.rs`.** Adapter selection is the only `match` over `HarnessId`. Add observed evidence for the selected adapter. Treat `x-bitrouter-harness` as a low-confidence compatibility fallback only when no native adapter matches.
- [ ] **Step 4: Delete the Codex `x-superpowers-phase` / `x-superpowers-skill` override and the `superpowers_agent_context_key` fast path. Add regression tests proving those headers cannot change state or routing key.**
- [ ] **Step 5: Move source-specific session extraction and Terminus compaction parsing behind adapter helpers. `session.rs` may combine adapter-produced identity hints but must not match on `HarnessId`.**
- [ ] **Step 6: Make `infer_online_context` infer protocol from the internal inbound-protocol hint first and delegate runtime selection to the adapter registry. Keep direct test construction through `harness_hint` working only as compatibility input.**
- [ ] **Step 7: Make real-trace capture call the same detector after parsing the body. Stop injecting `x-bitrouter-harness`, `x-bitrouter-protocol`, or derived workflow-identity headers into live requests; capture legacy headers only as optional input evidence.**
- [ ] **Step 8: Run all extractor, online, session, real-trace, replay, and shadow-policy tests until GREEN.**
- [ ] **Step 9: Commit `refactor(trace): detect agents from native traffic`.**

## Task 3: Remove runtime/workflow branches from routing and learning

**Files:**
- Modify: `apps/bitrouter/src/policy_table_router.rs`
- Modify: `apps/bitrouter/src/adequacy/observer.rs`
- Modify: `apps/bitrouter/src/adequacy/settlement.rs`
- Modify: `apps/bitrouter/src/workflow_state/decision.rs`
- Modify: `apps/bitrouter/src/workflow_state/reward_feedback.rs`
- Test: unit tests in the files above

**Interfaces:**
- Consumes: `OnlineWorkflowState::routing_key()`, generic `WorkflowStateKind`, generic session identity, existing `AdequacyLedger`, and request-scoped outcome candidates.
- Produces: `PolicyDecision` and `PolicyDecisionRecord` where the policy/ledger key is an `agent_trace/v1` projection; diagnostic source and identity never influence tier selection, qualification, pins, semantic-success counts, or reward application.

```rust
pub struct PolicyDecisionRecord {
    pub request_key: String,
    pub trace_state: String,
    pub trace_identity: WorkflowIdentity,
    pub selected_tier: Option<String>,
    pub selected_model: Option<String>,
    // existing generic decision and adequacy fields remain
}
```

- [ ] **Step 1: Write failing cross-source routing tests.** Feed equivalent native requests from every supported adapter with no private BitRouter headers and assert identical `request_key`, selected tier/model, exploration eligibility, and adequacy ledger key. Mutate workflow/role/complexity headers and assert no result changes.
- [ ] **Step 2: Run focused router and observer tests and record RED against the existing harness/workflow key behavior.**
- [ ] **Step 3: Switch every `PolicyKeyStrategy` branch to `AgentTrace`; remove the Terminus-specific `HarnessId`/`AgentRole` exploration branch. Use generic missing-context evidence, state, recovery, and capability risk only.**
- [ ] **Step 4: Keep source, protocol, state, and session identity in decision records solely for diagnostics. Rename new JSON fields to `trace_state` and `trace_identity`, while accepting `workflow_state` and `workflow_identity` as deserialize aliases for old artifacts. Update summaries to `by_trace_state`; preserve old input compatibility.**
- [ ] **Step 5: Update adequacy/reward tests to use projection keys. Add one test applying outcomes from two different adapters to the same projection and prove they update one generic ledger entry without reading harness/workflow fields.**
- [ ] **Step 6: Run router, observer, settlement, decision, reward-feedback, replay, archive, and Smithers reward-loop tests until GREEN.**
- [ ] **Step 7: Commit `refactor(policy): decouple trace source from learning`.**

## Task 4: Ship the generalized `@auto` template and migration contract

**Files:**
- Delete: `templates/superpowers-policy/README.md`
- Delete: `templates/superpowers-policy/bitrouter.yaml`
- Delete: `templates/superpowers-policy/policy-lock.yaml`
- Delete: `templates/superpowers-policy/policy-metadata.json`
- Create: `templates/auto-router/README.md`
- Create: `templates/auto-router/bitrouter.yaml`
- Create: `templates/auto-router/policy-lock.yaml`
- Create: `templates/auto-router/policy-metadata.json`
- Modify: `templates/README.md`
- Modify: `skills/bitrouter/SKILL.md`
- Modify: `skills/bitrouter/references/cli.md`
- Delete or replace: `skills/bitrouter/references/harness-smithers.md`
- Test: `crates/bitrouter-sdk/src/config/tests.rs`, `apps/bitrouter/src/policy_lock.rs`, and template validation commands

**Interfaces:**
- Consumes: `@preset[:variant]`, `PolicyKeyStrategy::AgentTrace`, and the projection keys from Task 1.
- Produces: `@auto`, `@auto:cost`, and an agent/workflow-independent pretrained starter policy. Normal `edit`, `test`, and `tool_followup` projections route to economy; guarded or unmatched projections route strong.

```yaml
presets:
  auto:
    model: "openai-codex:gpt-5.6-sol"
    policy: auto
variants:
  cost:
    routing: { sort: cost }
```

- [ ] **Step 1: Write failing config/lock tests loading `templates/auto-router/bitrouter.yaml`, resolving `@auto` and `@auto:cost`, and validating the bound lock. Assert a physical model remains passthrough.**
- [ ] **Step 2: Run the focused config/template tests and capture RED because the generalized template does not exist.**
- [ ] **Step 3: Replace the Superpowers template with the auto-router template. Use `presets.auto.policy: auto`, a top-level `variants.cost`, `key_strategy: agent_trace`, three normal mechanical projection routes, strong default, and the existing strong/economy capability guardrails. Metadata must say the policy is migrated from a same-scenario evaluation and that cross-agent quality remains unvalidated; do not claim new benchmark results.**
- [ ] **Step 4: Replace workflow-specific template and Smithers instructions with generic adaptive-routing documentation. Document that runtime adapters enrich diagnostics, not policy keys, and private headers are unnecessary.**
- [ ] **Step 5: Run `cargo run -p bitrouter -- config validate --config templates/auto-router/bitrouter.yaml`, `cargo run -p dist-helper -- check`, focused config/lock tests, and skill/plugin checks until GREEN.**
- [ ] **Step 6: Commit `feat(templates): add generalized auto router`.**

## Task 5: Prove compatibility and generalization end to end

**Files:**
- Modify: `apps/bitrouter/tests/workflow_state_real_agent_e2e.rs`
- Modify: `apps/bitrouter/tests/workflow_state_replay.rs`
- Create: `apps/bitrouter/tests/agent_trace_generalization.rs`
- Modify: any fixture only when required to represent native traffic accurately

**Interfaces:**
- Consumes: real HTTP protocol adapters, native agent traces, `@auto`, `RouteProjection`, policy decision recording, replay, and adequacy outcome application.
- Produces: executable proof that supported agents work without BitRouter workflow headers and share policy evidence by generic projection.

```rust
const SUPPORTED_SOURCES: &[HarnessId] = &[
    HarnessId::Codex,
    HarnessId::ClaudeCode,
    HarnessId::Hermes,
    HarnessId::OpenClaw,
    HarnessId::Terminus2,
    HarnessId::Smithers,
    HarnessId::Generic,
];
```

- [ ] **Step 1: Write a failing HTTP-level integration matrix.** Send representative Chat Completions, Responses, and Messages requests for Codex, Claude Code, Hermes, OpenClaw, Terminus 2, Smithers-originated, and generic clients without `x-bitrouter-harness`, `x-bitrouter-workflow-*`, `x-bitrouter-agent-*`, or `x-superpowers-*`. Assert every request routes and emits a decision.
- [ ] **Step 2: Assert equivalent edit/test/tool-followup traces share literal `agent_trace/v1` keys and the same tier, while recovery/high-risk variants select the strong default. Assert source remains available only in diagnostic evidence.**
- [ ] **Step 3: Add replay compatibility for old `workflow_state` lock/artifact inputs and new `agent_trace` outputs. Prove no migration silently reinterprets an old source-specific key as a new projection.**
- [ ] **Step 4: Run the new integration test and relevant real-agent/replay suites until GREEN.**
- [ ] **Step 5: Run `cargo fmt -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo nextest run --all-features` (or `cargo test --all-features`), and `cargo run -p dist-helper -- check`.**
- [ ] **Step 6: Commit `test(policy): prove agent trace generalization`.**

## Requirement Coverage

| Requirement | Primary task | Evidence |
|---|---|---|
| No harness-specific policy key | Task 1 | projection equality tests and literal keys |
| Only runtime-specific parser is trace adapter boundary | Task 2 | adapter registry and native detection tests |
| No private workflow header requirement | Tasks 2 and 5 | negative header tests and HTTP matrix |
| No workflow/evaluator-specific learning identity | Task 3 | shared ledger/outcome tests |
| Preserve all supported agents | Tasks 2 and 5 | adapter and HTTP matrices |
| One policy works across agents/workflows | Tasks 1, 4, and 5 | source-independent projection plus `@auto` |
| Preserve current self-correction mechanics | Task 3 | adequacy/reward compatibility tests |
| Existing artifacts remain readable | Tasks 1, 3, and 5 | Serde aliases and replay compatibility |
| Zero required external runtime dependencies | All tasks | Cargo-only focused and full test gates |

## Review and Delivery Protocol

1. Each task is implemented and committed independently after RED/GREEN evidence.
2. Each task receives an independent spec-compliance and code-quality review before the next task starts.
3. Critical and Important findings are fixed and re-reviewed; Minor findings are recorded for the final review.
4. A final reviewer audits the complete diff against this plan and the generalization objective.
5. The controller reruns the full verification commands on the exact final tree, updates the existing PR description, pushes the current PR branch, and confirms GitHub checks and mergeability.

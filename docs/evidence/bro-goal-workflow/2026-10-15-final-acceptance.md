# Goal MVP acceptance audit

All13 final check stages passed on the integrated source:4,675 workspace tests passed,24 pre-existing opt-in tests skipped; doctests6 passed/1 existing ignored; strict Clippy, formatting, diff/source hashes and registry validate/build/check passed. This audit supersedes the historical midpoint table in the implementation ledger. Hosted CI is separate from these local results.

The acceptance scope is a controlled local MVP, as defined by spec 9.3. A deterministic fixture can prove a guard or injected failure; it cannot prove model quality or an actual vendor campaign. Unknown external effects remain blocked. General external rollback, every harness/provider/platform, a global OS sandbox, production availability and complete external invoices are outside this evidence.

Evidence keys:

- **P**: [final workspace run](2026-10-15-main-goal-final-v1-workspace-nextest.log), including actual process/IPC/database/HTTP/PTY tests; [source identity](2026-10-15-main-goal-final-v1-source.sha256) binds 1,023 source/config/catalog inputs.
- **S**: [final check status](2026-10-15-main-goal-final-v1-status.json), strict all-feature/all-target Clippy, formatting, diff checks, doctests and registry checks. An unfinished step is not a pass.
- **R**: [actual R16 workflow](2026-10-15-real-goal-v16-summary.json), [original delivery patch/checks/assessment](2026-10-15-real-goal-v16-delivery-proof.json), [actual UI reconnection](2026-10-15-real-goal-v16-ui-reconnect.json). Claude ACP adapter 0.75.1, real configured OpenRouter native Messages route; default Tokio stack. Four original runs, no manual permission choice. Actual worker provider/model identity remains unreported.
- **I**: [54 corpus and 18 sequence records](2026-10-15-input-dependency-proof-v1-summary.json), [separate semantic audit](2026-10-15-input-semantic-audit.json). Real typed/reasoning calls, durable fixture Goal state, no harness dispatch. q03/q07 human-reviewed; other labels are drafts. Input code/labels are unchanged by the later permission-only fix and upstream catalog/test merge; the evaluated binary is still identified by its original manifest.
- **B**: [actual R10 bounded repair exhaustion](2026-10-15-real-goal-v10-summary.json), [R14 failed upstream call and independent continuation](2026-10-15-real-goal-v14-summary.json), [R15 pre-permission streaming block](2026-10-15-real-goal-v15-summary.json). These remain failures, not successful deliveries.

## Product gates

| ID | Implementation and evidence | Scope |
| --- | --- | --- |
| G01 | main dispatch/goal_code; P bare/code and inherited-default PTYs | Directory browsing starts no work; actual R16 uses `bro code`. |
| G02 | CodeArgs/parser and P retired-entry tests | Rejects agent argument/retired flags without silent retargeting or local side effects. |
| G03 | native shortcuts/acp_cli; P native/session/stdio regressions | Original direct harness and external ACP contracts retained; no all-brand production claim. |
| G04 | TUI Goal state/projection and P focus/draft/navigation tests | Other Goal updates do not steal scope or unsent drafts; bounded local UI evidence. |
| G05 | read-only Workflow/inspector; P active Goal PTY and R reconnect | No editable underlying session terminal or implicit control authority. |
| G06 | input job/Goal input service; I and R actual middle Enter | Query, modification and control are separate; R saves requirements2 with control=null. |
| G07 | typed executor/native validation/model_control; I | Real typed calls, finite candidate answers and reasoning escalation; all current typed inputs upgrade. No calibrated direct-control threshold or speed benefit claimed. |
| G08 | versioned inputs/resources/permissions/checks and projection; P/R | Saved, supplied, dispatched, adopted and checked facts stay distinct. R verifies marker in actual prompt, delivered file and persisted check. |
| G09 | completion service; P explicit acceptance tests and R eight-condition assessment | Default autonomous verified delivery; human acceptance only when explicitly required. |
| G10 | check/artifact identity/source/CAS fences; P/R | Old, changed, incomplete or relevant in-flight evidence cannot complete the current Goal. |
| G11 | shared budget/accounting admission; P budget/reservation tests, R UI | Managed configured token estimates and elapsed limits, shared reservations and unknown fees shown separately; no complete external bill hard-cap claim. |
| G12 | generic ACP negotiation/resources/controller; P/R | One real maintained harness completes the full protocol path; unsupported optional controls are refused. |
| G13 | outbox/lease/cold history/archive/evidence reader; P/R | Large cold history and version retirement remain bounded and receipt-verified; actual detach/reconnect preserves identities. No unmeasured multi-day SLO claim. |

## Failure gates

| ID | Implementation and evidence | Scope |
| --- | --- | --- |
| F01 | SQLite rollback and model_control durable-before-dispatch fault tests, P | Failed authority commits cannot authorize the corresponding external work; injected boundaries, not exhaustive machine crash proof. |
| F02 | actual outbox/database receipt and lost-ack tests, P | Original operation identity returns its original result; no blind resubmission. |
| F03 | startup/unknown dispatch mapping and reconciliation tests, P; B R14 | Unknown outcomes retain original identity and block replay. Local recovery requires actual stop/history/scope/stable workspace proof. |
| F04 | input/completion source fences and independent-review race, P; R | Old model/check responses cannot overwrite newer requirements; unchanged independent execution is retained. |
| F05 | paused Goal/branch edits and active two-branch fixtures, P | Pause seals admission; edits preserve Pause and dependency scope, never implicitly resume. |
| F06 | competing reservations/unknown usage/restart/elapsed tests, P | One managed budget authority; unknown amounts keep reservations, not fabricated zero cost. |
| F07 | changed artifact/nonzero check/completion capture races, P; R hash audit | Changed or late artifacts fail current handoff/completion identity checks. |
| F08 | stale permission/acceptance and exact offered-option tests, P | Rejects foreign, obsolete, persistent or unavailable responses without widening consent. |
| F09 | P outbox/history/PTY detach/cold archive tests; R actual reconnect | Bounded reconstruction keeps input/execution identity and independent drafts; no repeated execution. |
| F10 | P unsupported resource/model/controller/startup tests; R route confirmation | No fabricated adoption or native fallback when ACP lacks a capability. |
| F11 | I real native validation plus three controlled fault rows | Timeout/error/contradictory typed responses upgrade or stay Unknown; invalid scores never authorize control. Historical native conformance failure remains retained. |
| F12 | P obsolete-plan/known-failure/unknown-review/exhaustion tests; B | Known repairs bounded to three per task version; no-new-fact planning does not rearm itself, independent valid work continues, failure/impossible does not become success. |

## PRD acceptance criteria

| ID | Implementation and evidence | Scope |
| --- | --- | --- |
| A01 | planning/model selection, P/R | Natural-language delegation creates independent tasks without selecting a worker in the UI; isolated R harness capability restriction is explicit. |
| A02 | dynamic task graph/Workflow, P/R | Independent branches and dependent integration; no new foreground agent tab. |
| A03 | checked handoff/dependencies, P/R | Exact actual upstream check bytes frozen before dependent prompt; both artifacts consumed. |
| A04 | Goal scope/focus reducer, P | Other Goal matters preserve current focus and draft; deterministic UI state boundaries. |
| A05 | separate drafts/navigation/cache, P | Browsing, resizing and returning preserve scope/position without submitting work. |
| A06 | active input/resource replacement, P/R | Explicit mid-run change commits without another plan approval and is actually adopted by text2. |
| A07 | stop-boundary deadline/reconciliation/feedback, P/B | Unsupported steering waits at a real bounded boundary; unfinished effects remain unknown. |
| A08 | dependency version propagation, P/R | Updated text and dependent integration use version2; original numbers version1/execution continues. |
| A09 | actual permission material/cards/input, P/R | Exact one-time offered local choices; path/kind conflicts and late changes refuse authorization. |
| A10 | outbox/start recovery/source identity, P/B | Lost replies query original facts; unknown effects are retained rather than replayed. |
| A11 | satisfaction/completion projection, P/B/R | Worker exit, failed check, partial delivery and satisfied Goal are separate states. |
| A12 | Workflow/inspector authorization, P/R | Right side and underlying session observation remain read-only. |
| A13 | active Goal PTY detach and R reconnect | Daemon continues accepted work after UI exit; opening an existing Goal starts no new work. |
| A14 | actual PTY paste/Unicode/editor/resize/suspend/signal tests, P | Terminal modes restored; external editor drafts require actual Enter to submit. |
| A15 | I corpus/sequences and P domain guards | Hypothetical/negated/mixed messages preserve allowed effects; native semantic mistakes reported separately. |
| A16 | typed multi-question candidates and reasoning upgrade, I/P | Conservative escalation; no uncalibrated fast-path permissions or direct typed Pause. |
| A17 | durable model admission/input transactions, P | Runtime validates source/scope/capability/budget before persistent commit and dispatch. |
| A18 | frozen inputs/controller observations/check reports, P/R | Each UI effect claim has the matching source/version and actual execution evidence. |
| A19 | continuous inputs/paused changes/direct withdrawal, I/P | Later inputs do not erase unresolved requests or resume work; exact withdrawal works without a model or budget. |
| A20 | explicit scope/new requirements/old source guards, P/R | No retargeting a missing task or promoting an old artifact to current authority. |
| A21 | actual bare/code default-config PTYs, P | Both default entries open Goals; directory observation starts no worker. |
| A22 | retired parser and migrated consumers, P | Removed agent-argument surface rejects rather than silently invoking another mode. |
| A23 | native shortcut/session/provider regressions, P and retained opt-in evidence | Direct native harness interaction retained. Historical opt-in vendor timeout is not erased or called production certification. |
| A24 | transparent external stdio ACP/session authority, P/R | Harness remains authority for its own session; observation does not acquire write/control permission. |
| A25 | generic negotiation/resources, P/R | Capabilities are negotiated and reported; one real harness suffices under spec9.3. |
| A26 | completion autonomous service, P/R | Real current integrated delivery finishes without human acceptance by default. |
| A27 | exact explicit acceptance/source-version tests, P | Human acceptance required only by contract and bound to that exact delivery; no assertion of real human acceptance in R. |
| A28 | hash-bound check/spec/environment/source reports, P/R | Actual output, versions and local environment retained; outside-scope facts stay unverified. |
| A29 | immutable verifier baseline inputs/changed-test and integration tests, P/R | Original assertions independently hash-checked in R source and delivery; branch approval alone cannot prove final behavior. |
| A30 | relevant in-flight/late input/artifact/CAS fences, P | Unknown writes and stale assessment cannot commit completion. R final in-flight count is zero. |
| A31 | shared managed admission/reservations/check work, P | Planning through final assessment uses the same managed estimate authority; unknown external fees remain outside a claimed precise cap. |
| A32 | verified completion at budget boundary/withdrawal/projection, P | Existing verified evidence may complete without a new paid call; otherwise partial result and unmet conditions remain. |
| A33 | cost provenance/stop states, P/R/B | Configured estimates, unpriced work, requested stop and confirmed stop remain separate facts. |
| A34 | independent blocked branches/planning clarification, P/B | R14 text succeeds despite numbers failure; Goal and concrete blocker remain, no false overall delivery. |
| A35 | bounded repair/obsolete plan/unknown review, P/B | Known failures may repair within limits; unknown effects/review outcomes do not grant retry, impossible/stagnation retains Goal. |
| A36 | cold receipt/history/archive/outbox/source tests, P/R | Reconnect preserves original records and control source; archived facts never become current execution authority. |
| A37 | required check/query-only clarification/completion verdict tests, P/I/R | No invented failure cause or extra completion condition from q03; required missing evidence blocks, unrelated unknowns are explicitly scoped. |

## Model and evidence limits

The latest corpus has no prohibited persisted effects, but native intent selection is imperfect: defined control actions match16/23; native modification precision is0.50 and recall0.80 against draft labels. Both reasoning lanes' concrete control proposals match23/23 in this small corpus. Reasoning has no explicit query-intent output, so its query precision is unavailable. Every current typed input upgrades; no speed, cost saving or direct-control threshold is claimed. Paired semantic input state matches27/27 after excluding only updated timestamp/coordinator lease metadata. Fault injections are not vendor error-rate measurements.

R16 proves one full local credentialed workflow. Prior unsuccessful runs, the earlier native response-conformance failure, opt-in vendor campaign timeouts, scope-external unknown effects, unspecified worker provider identity and unpriced external fees remain disclosed. No production deployment, merge or all-vendor certification is implied.

//! Expanding advisory representations must not veto a feasible full prompt.
use super::*;
use crate::core::context_router::owned::CompileOptions;
use crate::gateway::model::decision::types::Answer;
use crate::storage::checkpoint::snapshot::context::{
    action::ContextAction, evidence::SourceSpan, view::ContextRepresentation,
};
use bitrouter_ai::core::protocol::{ContextMode, ModelMode};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const ARCHIVE: &str = "exact archive sentinel";

fn answer(request: &Request, representation: &str) -> ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    let Some(questions) = body["questions"].as_array() else {
        return ResponseTemplate::new(400);
    };
    let mut answers = Vec::new();
    for question in questions {
        let Some(choices) = question["choices"].as_array() else {
            return ResponseTemplate::new(400);
        };
        let selected = if choices.iter().any(|value| value["value"] == representation) {
            representation
        } else if choices.iter().any(|value| value["value"] == "unknown") {
            "unknown"
        } else {
            "full"
        };
        answers.push(
            json!({"type":"choice","name":question["name"],"choice":selected,
            "confidence":1.0,"probabilities":choices.iter().map(|value| json!({
                "value":value["value"],"probability":if value["value"] == selected {1.0} else {0.0}
            })).collect::<Vec<_>>() }),
        );
    }
    ResponseTemplate::new(200).set_body_json(json!({"model":"judge-reported-r1","answers":answers,
        "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10,
            "input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":0}}}))
}

#[tokio::test]
async fn owned_choice_expanding_summary_falls_back_to_full_in_auto_and_fixed() -> Result<()> {
    for context in [ContextMode::Auto, ContextMode::Fixed] {
        expanding(context, "summary", false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn owned_choice_expanding_extract_falls_back_to_full_in_auto_and_fixed() -> Result<()> {
    for context in [ContextMode::Auto, ContextMode::Fixed] {
        expanding(context, "extract", false).await?;
    }
    Ok(())
}

#[tokio::test]
async fn owned_choice_rejects_when_expanding_and_full_candidates_both_exceed_limit() -> Result<()> {
    for representation in ["summary", "extract"] {
        expanding(ContextMode::Auto, representation, true).await?;
    }
    Ok(())
}

async fn expanding(
    context: ContextMode,
    representation: &'static str,
    too_small: bool,
) -> Result<()> {
    let mut generation = Provider::start(false).await?;
    let mut choice = wire::Endpoint::start(false, false).await?;
    let decisions = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decisions"))
        .respond_with(move |request: &Request| answer(request, representation))
        .mount(&decisions)
        .await;
    // Reuse the sealed resource fixture with a real socket that chooses the
    // admitted derivation instead of the default endpoint's conservative answer.
    choice.url = format!("{}/v1", decisions.uri());
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let bound = fixture::sqlite(&generation, &choice, directory.path(), 2).await?;
    let checked = tokio::time::timeout(Duration::from_secs(60), async {
        let spies = Arc::new(HostSpies::default());
        let mut session = fixture::open(&bound, &generation, &workspace, spies.clone()).await?;
        let core = session.core().clone();
        let checked = async {
            // Pressure exceeds the frozen 96 KiB soft target without growing
            // the task/Choice request. This is protected original user evidence.
            let padding = "p".repeat(100 * 1024);
            core.seed_native_context(vec![Message::text(Role::User, &padding),
                Message::text(Role::Assistant, ARCHIVE)], Vec::new(), 1).await?;
            let mut initial = fixture::task();
            initial.routing.model = ModelMode::Fixed;
            initial.routing.context = ContextMode::Fixed;
            initial.limits = Some(crate::harness::resources::managed::NativeResources::limits());
            let baseline = session.run(initial.clone(), Arc::new(Approval), CancellationToken::new()).await?;
            ensure!(baseline.run.as_ref().context("baseline run")?.status == RunStatus::Completed);
            ensure!(baseline.context_store.decisions.is_empty());
            generation.receive().await?;
            let block = baseline.context_store.evidence.values()
                .find(|block| block.messages == vec![Message::text(Role::Assistant, ARCHIVE)])
                .context("catalogued optional archive")?;
            ensure!(!block.protected);
            let action = if representation == "summary" {
                ContextAction::Publish { block_ids: vec![block.block_id.clone()],
                    summary: "Expanding historical summary. ".repeat(64) }
            } else {
                ContextAction::Extract { block_id: block.block_id.clone(), spans: vec![SourceSpan {
                    message_index: 0, content_index: 0, start: 0, end: ARCHIVE.len(),
                }] }
            };
            let expected = core.derive_context_fallback_for_test(action).await?;
            // Repeating this task keeps the derivation's task/instruction digest
            // stable. The full catalog retains each identical source group once.
            // Leave less headroom than either representation's rendering wrapper.
            let full_bytes = serde_json::to_vec(&expected)?.len();
            let limit = if too_small { full_bytes - 1 } else { full_bytes + 32 };
            let mut task = initial.clone();
            task.routing.model = ModelMode::Policy;
            task.routing.context = context;
            task.context_limit_bytes = Some(u64::try_from(limit)?);
            let state = session.run(task, Arc::new(Approval), CancellationToken::new()).await?;
            let run = state.run.as_ref().context("bounded run")?;
            let decision = state.context_store.decisions.values()
                .find(|decision| decision.run_id == run.run_id).context("acknowledged Choice")?;
            ensure!(!decision.stale && state.context_store.decisions.len() == 1);
            ensure!(state.context_store.evidence.get(&block.block_id) == Some(block),
                "fallback rewrote immutable source history");
            let mut original = expected.clone();
            original.messages.push(Message::text(Role::User, &initial.text));
            ensure!(crate::storage::checkpoint::snapshot::context::digest(&original)? == decision.source.prompt_sha256,
                "fixture differs from the actual preparation input");
            let response = decision.outcome.as_ref().context("Choice outcome ACK")?.as_ref()
                .map_err(|error| anyhow::anyhow!("Choice failed: {error}"))?;
            let (question, candidate) = decision.candidates.candidates.iter()
                .find(|(_, candidate)| candidate.block_id == block.block_id).context("admitted archive candidate")?;
            ensure!(if representation == "summary" { candidate.summary_id.is_some() } else { candidate.extract_id.is_some() });
            ensure!(matches!(response.answers.get(question), Some(Answer::Choice { choice, .. }) if choice == representation));

            // Establish the actual boundary with the acknowledged candidate set,
            // independently of the successful/failed preparation assertions.
            let options = |response| CompileOptions { source: decision.source.clone(), policy: &decision.policy,
                response, decision_id: Some(decision.decision_id.clone()), reason: "boundary_check", hard_limit_bytes: limit };
            let routed = decision.candidates.compile(&state.context_store, &original, options(Some(response)));
            ensure!(routed.err().context("routed representation unexpectedly fits")?.code
                == bitrouter_ai::core::protocol::ErrorCode::NoFeasibleRoute);
            let full = decision.candidates.compile(&state.context_store, &original, options(None));
            let turn = state.agents.get(&state.agent_id).and_then(|agent| agent.turn.as_ref()).context("bounded turn")?;
            ensure!(turn.steps.len() == 1, "fallback started another model step");
            if too_small {
                ensure!(full.err().context("full unexpectedly fits")?.code == bitrouter_ai::core::protocol::ErrorCode::NoFeasibleRoute);
                ensure!(run.status == RunStatus::Failed && generation.calls() == 1);
                ensure!(decision.view_ids.is_empty() && turn.steps[0].plan.is_none());
                ensure!(turn.terminal_reason.as_deref().is_some_and(|reason| reason.contains("no context candidate fits the prompt byte limit")));
            } else {
                let (_, full) = full?;
                ensure!(full == expected && run.status == RunStatus::Completed);
                let execution = state.context_store.executions.get(&turn.steps[0].step_id).context("committed full execution")?;
                let view = state.context_store.views.get(&execution.view_id).context("committed full view")?;
                ensure!(view.reason == "full_context_candidate" && view.omitted.is_empty());
                ensure!(view.selected.iter().all(|selected| matches!(selected, ContextRepresentation::Full { .. })));
                ensure!(view.decision_id.as_deref() == Some(decision.decision_id.as_str())
                    && execution.decision_id == view.decision_id && decision.view_ids == vec![view.view_id.clone()]);
                let routing = execution.routing.as_ref().context("actual preparation selection")?;
                ensure!(routing.selected_context == "full" && routing.candidates.iter().all(|candidate| candidate.context == "full"));
                let plan = turn.steps[0].plan.as_ref().context("generation plan")?;
                ensure!(plan.prompt.messages == expected.messages && plan.prompt.system == expected.system
                    && plan.prompt.tools == expected.tools && serde_json::to_value(&plan.prompt.params)? == serde_json::to_value(&expected.params)?);
                ensure!(plan.prompt.messages.iter().any(|message| message == &Message::text(Role::Assistant, ARCHIVE)));
                let sent = generation.receive().await?;
                let mut messages = vec![json!({"role":"system","content":expected.system})];
                for message in &expected.messages {
                    let [Content::Text { text, .. }] = message.content.as_slice() else { anyhow::bail!("fixture requires plain exact source history"); };
                    messages.push(json!({"role":message.role,"content":text}));
                }
                ensure!(sent.body["messages"] == json!(messages), "generation did not receive exact full history");
                ensure!(generation.calls() == 2);
            }
            let saved = fixture::journal(&bound).await?;
            let links = saved["owned_choices"].as_object().context("durable Choice custody")?;
            ensure!(links.len() == 1);
            let link = links.get(&decision.decision_id).context("retained Choice link")?;
            ensure!(link["outcome_digest"].as_str().is_some_and(|digest| digest.len() == 64));
            let paid = receipt(&bound, link["child_execution"].as_str().context("Choice execution")?).await?;
            ensure!(paid.attempts.len() == 1 && paid.attempts[0].outcome == Outcome::Completed
                && paid.attempts[0].usage.input_tokens == Some(7) && paid.attempts[0].usage.output_tokens == Some(3));
            let requests = decisions.received_requests().await.context("Choice socket requests")?;
            ensure!(requests.len() == 1, "fallback sent a second paid Choice request");
            ensure!(serde_json::from_slice::<Value>(&requests[0].body)?["model"] == "judge-r1");
            ensure!(spies.decisions.load(Ordering::SeqCst) == 0, "owned fallback invoked a host decision runtime");
            Ok(())
        }.await;
        let closed = session.close().await;
        checked.and(closed.map_err(Into::into))
    }).await.context("owned fallback deadline").and_then(|result| result);
    let closed = choice.finish().await;
    finish(&bound.fixture, generation, checked).await?;
    closed
}

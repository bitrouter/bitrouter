//! Actual subject HTTP and distinct paid execution; the effective head is never relabelled.
mod integrity;
use super::*;
use crate::core::evolution::evidence::EvidencePacket;
use fixture::Options;
use tokio::sync::mpsc;

#[derive(Clone)]
struct State {
    release: CancellationToken,
    observed: mpsc::Sender<Value>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
struct Judge {
    url: String,
    state: State,
    received: mpsc::Receiver<Value>,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}
impl Judge {
    async fn new(held: bool) -> Result<Self> {
        let release = CancellationToken::new();
        if !held {
            release.cancel();
        }
        let (observed, received) = mpsc::channel(4);
        let state = State {
            release,
            observed,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let router = axum::Router::new()
            .route("/v1/chat/completions", axum::routing::post(judge))
            .with_state(state.clone());
        let task =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            url,
            state,
            received,
            task: Some(task),
        })
    }
    async fn receive(&mut self) -> Result<Value> {
        tokio::time::timeout(TIMEOUT, self.received.recv())
            .await?
            .context("actual paid request absent")
    }
    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }
    async fn stop(mut self) -> Result<()> {
        self.state.release.cancel();
        let task = self.task.take().context("judge task absent")?;
        task.abort();
        match task.await {
            Err(error) if error.is_cancelled() => Ok(()),
            value => {
                value??;
                Ok(())
            }
        }
    }
}
fn answer(body: &Value) -> Result<RubricEvaluation> {
    let messages = body["messages"]
        .as_array()
        .context("actual judge messages")?;
    let content = messages
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .context("actual judge evidence text")?;
    let projected: Value = serde_json::from_str(content)?;
    let packet: EvidencePacket = serde_json::from_value(projected["evidence"].clone())?;
    ensure!(!packet.items.is_empty() && packet.gaps.is_empty());
    let citations: Vec<_> = packet
        .items
        .iter()
        .map(|item| item.citation.clone())
        .collect();
    Ok(RubricEvaluation {
        rubric_version: rubric::RUBRIC_VERSION.into(),
        items: rubric::library()
            .into_iter()
            .map(|criterion| RubricItem {
                criterion_id: criterion.id.into(),
                applicability: if criterion.mandatory {
                    Applicability::Applicable
                } else {
                    Applicability::NotApplicable
                },
                selection_reason: "The real fixture requested only one recorded text reply".into(),
                score: if criterion.mandatory {
                    CriterionScore::Scored {
                        value_ppm: rubric::PPM,
                    }
                } else {
                    CriterionScore::NotApplicable
                },
                evidence: citations.clone(),
                explanation: "Compare the actual captured prompt and returned reply".into(),
            })
            .collect(),
        diagnostics: Vec::new(),
        severe_violation: false,
        violation_evidence: Vec::new(),
        summary: "Finite actual reply fixture judgement".into(),
    })
}
async fn judge(
    axum::extract::State(state): axum::extract::State<State>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.calls.fetch_add(1, Ordering::SeqCst);
    if headers
        .get("authorization")
        .is_none_or(|h| h != "Bearer explicit-judge-secret")
        || state.observed.try_send(body.clone()).is_err()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let response = answer(&body).and_then(|value| Ok(serde_json::to_string(&value)?));
    state.release.cancelled().await;
    let Ok(response) = response else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    axum::Json(json!({"id":"block-paid-reply","object":"chat.completion","created":1,"model":"judge-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":response},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
            "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}})).into_response()
}
async fn paid_rows(host: &Host, kind: &str) -> Result<Vec<Value>> {
    let rows = host.fixture.db.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT body FROM checkpoint_evolution WHERE installation_id=? AND tenant_id=? AND creator_principal_id=? AND kind=? ORDER BY record_id LIMIT 5",
        [host.fixture.installation.as_str().into(), host.fixture.tenant.as_str().into(), host.fixture.principal.as_str().into(), kind.into()])).await?;
    ensure!(rows.len() <= 4);
    rows.into_iter()
        .map(|row| {
            let body: String = row.try_get("", "body")?;
            ensure!(body.len() <= 1024 * 1024);
            Ok(serde_json::from_str(&body)?)
        })
        .collect()
}
async fn receipt(host: &Host, id: &str) -> Result<Value> {
    let result = crate::tenancy::receipts::store::lookup(
        &host.fixture.db,
        &host.fixture.installation,
        &host.fixture.tenant,
        &id.try_into()?,
    )
    .await?
    .context("actual035 receipt absent")?;
    Ok(serde_json::to_value(result)?)
}
fn usage(value: &Value) -> Result<()> {
    ensure!(
        value["state"] == "committed"
            && value["outcome"] == "completed"
            && value["attempts"].as_array().context("attempts")?.len() == 1
            && value["usage"]["complete"] == true
            && value["usage"]["input_tokens"] == 4
            && value["usage"]["output_tokens"] == 2
    );
    Ok(())
}
async fn control(host: &Host) -> Result<Value> {
    let mut rows = host.records("owned_block_control_v1").await?;
    ensure!(rows.len() == 1);
    rows.pop().context("actual control")
}
fn observation(control: &Value) -> Result<&Value> {
    let rows = control["report"]["observations"]["sessions"]
        .as_object()
        .context("actual cohort")?;
    ensure!(rows.len() == 1);
    rows.values().next().context("actual observation")
}
async fn start(held: bool, source: AssessmentSource) -> Result<(Host, Judge, String)> {
    let judge = Judge::new(held).await?;
    let opened = async {
        let mut host = Host::configured(Options {
            assessment: Some((&judge.url, source)),
            ..Default::default()
        })
        .await?;
        let result = async {
            let native = host.open().await?;
            host.current()?
                .prompt(&native, "Reply with exactly native reply; no tools")
                .await?;
            tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
            Ok::<_, anyhow::Error>(native)
        }
        .await;
        match result {
            Ok(native) => Ok((host, native)),
            Err(error) => {
                host.finish(Err(error)).await?;
                anyhow::bail!("unexpected successful failed-init cleanup")
            }
        }
    }
    .await;
    match opened {
        Ok((host, native)) => Ok((host, judge, native)),
        Err(error) => {
            judge.stop().await?;
            Err(error)
        }
    }
}
async fn finish(host: Host, judge: Judge, checked: Result<()>) -> Result<()> {
    judge.state.release.cancel();
    let closed = host.finish(checked).await;
    let stopped = judge.stop().await;
    closed.and(stopped)
}
async fn original_facts(host: &Host) -> Result<(Value, Value)> {
    let jobs = paid_rows(host, "owned_judge_job").await?;
    ensure!(jobs.len() == 1);
    let intents = host.records("owned_block_intent_v1").await?;
    ensure!(intents.len() == 1);
    let subject = receipt(
        host,
        intents[0]["intent"]["request_id"]
            .as_str()
            .context("subject execution")?,
    )
    .await?;
    let paid = receipt(
        host,
        jobs[0]["execution"].as_str().context("paid execution")?,
    )
    .await?;
    usage(&subject)?;
    usage(&paid)?;
    ensure!(subject["execution_id"] != paid["execution_id"]);
    ensure!(
        jobs[0]["output"]["request_id"] == paid["execution_id"]
            && jobs[0]["output"]["usage"]["prompt_tokens"] == 4
            && jobs[0]["output"]["usage"]["completion_tokens"] == 2
    );
    Ok((subject, paid))
}
async fn one_budget(host: &Host, judge: &Judge) -> Result<()> {
    ensure!(judge.calls() == 1);
    let counters = paid_rows(host, "owned_judge_budget").await?;
    ensure!(counters.len() == 1 && counters[0]["accepted"] == 1);
    ensure!(paid_rows(host, "owned_judge_pending").await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_paid_block_head_validates_original_receipt_and_never_relabels_human_or_refunds()
-> Result<()> {
    let (host, mut judge, native) = start(false, AssessmentSource::Agentic).await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let current = host.current()?;
        current
            .checkpoint(&native)
            .await?
            .context("real frozen checkpoint")?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        ensure!(judge.receive().await?["model"] == "judge-model");
        let facts = original_facts(&host).await?;
        let view = current.review_checkpoint(&native, None).await?;
        ensure!(
            view.history.len() == 1
                && view.history[0].input.source == AssessmentSource::Agentic
                && view.history[0].selected_on_submission
        );
        current.checkpoint(&native).await?; // Same logical input; no new paid Accept.
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let selected = control(&host).await?;
        let value = observation(&selected)?;
        ensure!(value["quality"] == 1.0 && value["total_cost_micro_usd"].is_null());
        ensure!(
            value["measurement_contract"]
                == selected["state"]["blocks"]["same-block"]["definition"]["measurement_contract"]
        );
        ensure!(selected["state"]["blocks"]["same-block"]["status"] != "adopted");
        let human = current
            .submit_assessment(&native, label(&view, "replace-paid-with-human")?)
            .await?;
        current.checkpoint(&native).await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let changed = control(&host).await?;
        let actual = observation(&changed)?;
        ensure!(
            actual["revision"] != value["revision"]
                && actual["measurement_contract"] != value["measurement_contract"]
        );
        ensure!(actual["quality"] == 1.0 && actual["total_cost_micro_usd"].is_null());
        ensure!(
            changed["report"]["unavailable"]
                .as_object()
                .context("unavailable")?
                .values()
                .any(|r| r
                    .as_array()
                    .is_some_and(|r| r.iter().any(|v| v == "measurement_contract_changed")))
        );
        ensure!(
            current
                .review_checkpoint(&native, None)
                .await?
                .scoring
                .expected_revision
                .as_deref()
                == Some(human.revision.revision_id.as_str())
        );
        one_budget(&host, &judge).await?;
        ensure!(
            original_facts(&host).await? == facts,
            "cohort rewrote original035 usage"
        );
        // An actually changed source reaches the normal acceptance ceiling.
        current
            .prompt(
                &native,
                "another actual source under the SAME retained revision",
            )
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        ensure!(
            current.checkpoint(&native).await.is_err(),
            "Block supplied an extra paid allowance"
        );
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        one_budget(&host, &judge).await?;
        ensure!(host.fixture.service.current().is_ok());
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("subject calls")?
                .len()
                == 2
        );
        ensure!(
            receipt(
                &host,
                facts.0["execution_id"]
                    .as_str()
                    .context("original subject")?
            )
            .await?
                == facts.0
        );
        ensure!(
            receipt(
                &host,
                facts.1["execution_id"].as_str().context("original judge")?
            )
            .await?
                == facts.1
        );
        Ok(())
    })
    .catch_unwind()
    .await;
    finish(
        host,
        judge,
        match checked {
            Ok(value) => value,
            Err(_) => Err(anyhow::anyhow!("paid Block test unwound")),
        },
    )
    .await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_block_human_wins_held_judge_and_retraction_preserves_both_paid_facts() -> Result<()>
{
    let (host, mut judge, native) = start(true, AssessmentSource::Human).await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let current = host.current()?;
        let work = current.checkpoint(&native);
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => anyhow::bail!("judge was not held: {result:?}"),
            result = judge.receive() => { ensure!(result?["model"] == "judge-model"); }
        }
        let view = current.review_checkpoint(&native, None).await?;
        let human = current
            .submit_assessment(&native, label(&view, "human-during-paid")?)
            .await?;
        ensure!(paid_rows(&host, "owned_judge_active_v3").await?.len() == 1 && judge.calls() == 1);
        judge.state.release.cancel();
        tokio::time::timeout(TIMEOUT, &mut work).await??;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let facts = original_facts(&host).await?;
        let after = current.review_checkpoint(&native, None).await?;
        ensure!(
            after.scoring.expected_revision.as_deref() == Some(human.revision.revision_id.as_str())
        );
        let automatic = after
            .history
            .iter()
            .find(|r| r.input.source == AssessmentSource::Agentic)
            .context("paid terminal revision")?;
        ensure!(!automatic.selected_on_submission && automatic.selection_reason == "head_changed");
        ensure!(paid_rows(&host, "owned_judge_job").await?[0]["status"] == "unselected");
        current.checkpoint(&native).await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let selected = control(&host).await?;
        ensure!(
            observation(&selected)?["quality"] == 1.0
                && observation(&selected)?["total_cost_micro_usd"].is_null()
        );
        let retracted = current
            .retract_assessment(
                &native,
                RevisionInput {
                    submission_id: "block-human-retraction".into(),
                    checkpoint_id: after.checkpoint.checkpoint_id,
                    expected_revision: after.scoring.expected_revision,
                    source: AssessmentSource::Human,
                    evaluator_id: String::new(),
                    evaluator_version: String::new(),
                    assessment: None,
                    reason: "Withdraw original human judgement".into(),
                },
            )
            .await?;
        current.checkpoint(&native).await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let withdrawn = control(&host).await?;
        ensure!(
            observation(&withdrawn)?["quality"].is_null()
                && observation(&withdrawn)?["revision"] != observation(&selected)?["revision"]
        );
        ensure!(
            current
                .review_checkpoint(&native, None)
                .await?
                .scoring
                .expected_revision
                .as_deref()
                == Some(retracted.revision_id.as_str())
        );
        one_budget(&host, &judge).await?;
        ensure!(
            original_facts(&host).await? == facts
                && host
                    .upstream
                    .received_requests()
                    .await
                    .context("subject requests")?
                    .len()
                    == 1
        );
        Ok(())
    })
    .catch_unwind()
    .await;
    finish(
        host,
        judge,
        match checked {
            Ok(value) => value,
            Err(_) => Err(anyhow::anyhow!("held paid Block test unwound")),
        },
    )
    .await
}

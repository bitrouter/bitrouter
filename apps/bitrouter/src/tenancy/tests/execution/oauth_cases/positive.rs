use super::*;
use bitrouter_ai::tenancy::execution::Outcome;

fn check_system(posted: &Value, oauth: bool) -> Result<()> {
    let system = &posted["system"];
    ensure!(system.to_string().contains("retain my instruction"));
    let identity = crate::gateway::providers::anthropic::headers::CLAUDE_AGENT_SYSTEM_PROMPT;
    if oauth {
        let blocks = system.as_array().context("owned OAuth system blocks")?;
        ensure!(blocks.len() >= 2);
        ensure!(blocks[0]["type"] == "text" && blocks[0]["text"] == identity);
    } else {
        ensure!(
            !system.to_string().contains(identity),
            "static auth must not inject OAuth identity"
        );
    }
    Ok(())
}

#[tokio::test]
async fn expired_selected_seed_refreshes_once_then_messages_json_sse_and_reopen_use_generation_one()
-> Result<()> {
    for &h2 in modes() {
        for stream in [false, true] {
            let mut harness = Harness::new(true).await?;
            let client = Harness::client(h2)?;
            harness.provider.refresh_gate.add_permits(1);
            harness.provider.model_gate.add_permits(2);
            let response = harness.send(&client, stream).send().await?;
            ensure!(
                response.status() == StatusCode::OK,
                "initial OAuth response {}",
                response.status()
            );
            ensure!(
                response.version()
                    == if h2 {
                        ::http::Version::HTTP_2
                    } else {
                        ::http::Version::HTTP_11
                    }
            );
            let id = execution_id(&response)?;
            let body = response.text().await?;
            if stream {
                ensure!(body.contains("event: message_stop"));
            } else {
                ensure!(
                    serde_json::from_str::<Value>(&body)?["content"][0]["text"] == "owned reply"
                );
            }
            let posted = tokio::time::timeout(TIMEOUT, harness.model_seen.recv())
                .await?
                .context("actual model")?;
            check_system(&posted, true)?;
            let receipt = harness.receipt(&id).await?;
            ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
            ensure!(
                receipt.usage.input_tokens == Some(16) && receipt.usage.output_tokens == Some(7)
            );
            ensure!(
                receipt.attempts[0]
                    .target
                    .as_ref()
                    .context("actual target")?
                    .credential_version
                    == harness.slot.revision_id
            );
            ensure!(
                receipt.attempts[0]
                    .native_usage
                    .iter()
                    .any(|counter| counter.name
                        == bitrouter_ai::tenancy::execution::UsageDimension::InputTokens
                        && counter.value == 11)
            );
            ensure!(harness.row().await?.metadata.generation == 1);
            // Same immutable seed may be reimported but never resets the live slot.
            harness
                .resources
                .provision_bytes(&harness.fixture.db, &harness.manifest)
                .await?;
            ensure!(harness.row().await?.metadata.generation == 1);
            harness.reopen().await?;
            let response = harness.send(&client, stream).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let second = execution_id(&response)?;
            response.bytes().await?;
            ensure!(second != id && harness.receipt(&second).await?.attempts.len() == 1);
            let reposted = tokio::time::timeout(TIMEOUT, harness.model_seen.recv())
                .await?
                .context("actual model after reopen")?;
            check_system(&reposted, true)?;
            ensure!(
                harness.provider.refreshes.load(Ordering::SeqCst) == 1
                    && harness.provider.models.load(Ordering::SeqCst) == 2
            );
            harness.fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn fresh_import_uses_the_bound_store_without_a_refresh_or_auxiliary_row() -> Result<()> {
    let harness = Harness::new(false).await?;
    harness.provider.model_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.row().await?.metadata.generation == 0
    );
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.attempts.len() == 1 && receipt.usage.input_tokens == Some(16));
    use sea_orm::{ConnectionTrait, Statement};
    let row = harness
        .fixture
        .db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM tenancy_receipt_auxiliary".to_owned(),
        ))
        .await?
        .context("aux count")?;
    ensure!(row.try_get::<i64>("", "n")? == 0);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn static_selected_messages_keeps_existing_auth_without_oauth_lookup() -> Result<()> {
    let mut harness = Harness::new_mode(false, true).await?;
    harness.provider.model_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.attempts.len() == 1 && receipt.usage.input_tokens == Some(16));
    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 0);
    let posted = tokio::time::timeout(TIMEOUT, harness.model_seen.recv())
        .await?
        .context("actual static model")?;
    check_system(&posted, false)?;
    ensure!(
        storage::load(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.slot
        )
        .await?
        .is_none()
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

/// Observe the real native static consumer without inheriting a host applier.
/// Owned OAuth remains limited to the retained HTTP Messages profile.
#[derive(Default)]
struct NativeControl {
    plans: AtomicUsize,
    attempts: AtomicUsize,
    text_parts: AtomicUsize,
    stream: bool,
}
#[async_trait::async_trait]
impl crate::gateway::pipeline::native::NativeExecutionControl for NativeControl {
    fn observe_stream(&self) -> bool {
        self.stream
    }
    async fn on_stream_part(&self, _: &str, part: &bitrouter_ai::types::StreamPart) {
        if matches!(part, bitrouter_ai::types::StreamPart::TextDelta { text } if text == "owned reply")
        {
            self.text_parts.fetch_add(1, Ordering::SeqCst);
        }
    }
    async fn plan(
        &self,
        plan: crate::gateway::pipeline::native::NativePlan,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::native::NativePlanAdmission> {
        if plan.routes.len() != 1
            || plan.routes[0].protocol != bitrouter_ai::types::ApiProtocol::Messages
        {
            return Err(crate::gateway::error::BitrouterError::Forbidden(
                "unexpected native fixture route".into(),
            ));
        }
        self.plans.fetch_add(1, Ordering::SeqCst);
        Ok(crate::gateway::pipeline::native::NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, _: &str, _: u32) -> crate::gateway::error::Result<()> {
        Ok(())
    }
    async fn after_attempt(&self, _: bitrouter_ai::core::evidence::NativeAttemptReport) {
        self.attempts.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn native_static_messages_succeed_but_owned_oauth_is_fenced_before_provider_io() -> Result<()>
{
    for static_auth in [true, false] {
        for stream in [false, true] {
            let mut harness = Harness::new_mode(!static_auth, static_auth).await?;
            let checked: Result<()> = async {
                let input = crate::gateway::http::parse_input(
                    &bitrouter_ai::types::ApiProtocol::Messages,
                    json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"system":"retain my instruction","max_tokens":25}),
                )?;
                let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) = input else {
                    anyhow::bail!("native generation input");
                };
                let control = Arc::new(NativeControl { stream, ..Default::default() });
                if static_auth {
                    harness.provider.model_gate.add_permits(1);
                }
                let result = tokio::time::timeout(
                    TIMEOUT,
                    harness.fixture.service.execute_native_controlled(*prompt, harness.headers.clone(), control.clone()),
                ).await?;
                if static_auth {
                    let response = result?;
                    let id: ExecutionId = response.request_id.try_into()?;
                    let receipt = harness.receipt(&id).await?;
                    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
                    ensure!(receipt.attempts[0].outcome == Outcome::Completed);
                    ensure!(receipt.usage.input_tokens == Some(16) && receipt.usage.output_tokens == Some(7));
                    ensure!(control.plans.load(Ordering::SeqCst) == 1 && control.attempts.load(Ordering::SeqCst) == 1);
                    ensure!(control.text_parts.load(Ordering::SeqCst) == usize::from(stream));
                    let posted = tokio::time::timeout(TIMEOUT, harness.model_seen.recv()).await?
                        .context("actual controlled static Messages send")?;
                    check_system(&posted, false)?;
                    ensure!(posted["stream"].as_bool() == Some(stream));
                    ensure!(harness.provider.requests.load(Ordering::SeqCst) == 1);
                    ensure!(harness.provider.models.load(Ordering::SeqCst) == 1);
                    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 0);
                    ensure!(storage::load(&harness.fixture.db, &harness.fixture.installation, &harness.slot).await?.is_none());
                } else {
                    // Native Generation is deliberately outside Wire(Messages).
                    // Neither an expired seed nor stream observation widens it.
                    ensure!(matches!(result, Err(crate::gateway::error::BitrouterError::Forbidden(ref message))
                        if message == "installation or dispatch binding does not match"));
                    ensure!(harness.provider.requests.load(Ordering::SeqCst) == 0);
                    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 0);
                    ensure!(harness.provider.models.load(Ordering::SeqCst) == 0);
                    ensure!(control.text_parts.load(Ordering::SeqCst) == 0);
                    ensure!(harness.row().await?.metadata.generation == 0);

                    // The same live headers and retained credential do work on
                    // the supported HTTP profile: rejection was not stale auth.
                    harness.provider.refresh_gate.add_permits(1);
                    harness.provider.model_gate.add_permits(1);
                    let response = harness.send(&Harness::client(false)?, stream).send().await?;
                    ensure!(response.status() == StatusCode::OK);
                    let id = execution_id(&response)?;
                    let body = response.text().await?;
                    if stream {
                        ensure!(body.contains("event: message_stop"));
                    } else {
                        ensure!(serde_json::from_str::<Value>(&body)?["content"][0]["text"] == "owned reply");
                    }
                    let receipt = harness.receipt(&id).await?;
                    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
                    ensure!(receipt.usage.input_tokens == Some(16) && receipt.usage.output_tokens == Some(7));
                    let posted = tokio::time::timeout(TIMEOUT, harness.model_seen.recv()).await?
                        .context("supported HTTP OAuth control")?;
                    check_system(&posted, true)?;
                    ensure!(posted["stream"].as_bool() == Some(stream));
                    ensure!(harness.provider.requests.load(Ordering::SeqCst) == 2);
                    ensure!(harness.provider.refreshes.load(Ordering::SeqCst) == 1);
                    ensure!(harness.provider.models.load(Ordering::SeqCst) == 1);
                    ensure!(harness.row().await?.metadata.generation == 1);
                }
                Ok(())
            }.await;
            let shutdown = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
            let closed = harness.close().await;
            checked?;
            shutdown??;
            closed?;
        }
    }
    Ok(())
}

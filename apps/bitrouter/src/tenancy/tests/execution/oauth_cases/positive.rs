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
            let technical =
                tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.observed(&id))
                    .await??;
            technical.validate_against(&receipt)?;
            ensure!(
                technical.operations.len() == 1 && technical.operations[0].attempt_ids.len() == 1
            );
            ensure!(!technical.operations[0].partial && technical.operations[0].streamed == stream);
            ensure!(technical.operations[0].first_semantic.is_some() == stream);

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

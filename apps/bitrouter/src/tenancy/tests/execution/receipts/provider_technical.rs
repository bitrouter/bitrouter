//! Actual owning Core sender + protected getter; no financial qualification.
use super::*;
use bitrouter_ai::tenancy::provider_technical_facts::{
    FirstSemanticKind, ProviderTechnicalFactsV1,
};
use sea_orm::{ConnectionTrait, TransactionTrait};

async fn owned(
    protocol: &str,
    status: StatusCode,
    body: String,
    stream: bool,
) -> Result<(tempfile::TempDir, Harness)> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("technical.sqlite");
    let harness = Harness::with_observed_storage(
        protocol,
        status,
        body,
        stream,
        None,
        false,
        (
            crate::telemetry::receipt_latency::Sink::default(),
            Some(&path),
        ),
    )
    .await?;
    Ok((home, harness))
}

async fn measured(
    harness: &Harness,
    client: &reqwest::Client,
    id: &ExecutionId,
) -> Result<Option<ProviderTechnicalFactsV1>> {
    tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(id)).await??;
    let response = client
        .get(format!(
            "{}/control/v1/tenants/{}/executions/{id}/provider-technical-facts/v1",
            harness.base, harness.fixture.tenant
        ))
        .bearer_auth(
            harness
                .credential
                .secret
                .as_deref()
                .context("read credential")?,
        )
        .header(
            "Bitrouter-Installation-Id",
            harness.fixture.installation.as_str(),
        )
        .send()
        .await?;
    ensure!(
        response.status() == StatusCode::OK,
        "technical read status {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get("cache-control")
            .is_some_and(|value| value == "no-store")
    );
    Ok(response.json().await?)
}

async fn measured_v2(
    harness: &Harness,
    client: &reqwest::Client,
    id: &ExecutionId,
) -> Result<Option<bitrouter_ai::tenancy::provider_technical_facts_v2::ProviderTechnicalFactsV2>> {
    tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(id)).await??;
    let response = client
        .get(format!(
            "{}/control/v1/tenants/{}/executions/{id}/provider-technical-facts/v2",
            harness.base, harness.fixture.tenant
        ))
        .bearer_auth(
            harness
                .credential
                .secret
                .as_deref()
                .context("read credential")?,
        )
        .header(
            "Bitrouter-Installation-Id",
            harness.fixture.installation.as_str(),
        )
        .send()
        .await?;
    ensure!(
        response.status() == StatusCode::OK,
        "scope getter status {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get("cache-control")
            .is_some_and(|v| v == "no-store")
    );
    Ok(response.json().await?)
}

#[tokio::test]
async fn actual_json_sender_getter_binds_original_attempts_and_read_never_sends() -> Result<()> {
    for protocol in ["chat_completions", "messages", "responses", "decisions"] {
        let (_home, harness) = owned(
            protocol,
            StatusCode::OK,
            serde_json::to_string(&json_response(protocol, full_usage(protocol))?)?,
            false,
        )
        .await?;
        let checked: Result<()> = async {
            let client = Harness::client(false)?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let id = execution_id(&response)?;
            ensure!(!response.bytes().await?.is_empty());
            tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&id)).await??;
            let original = harness.receipt(&client, &id).await?;
            let data = measured(&harness, &client, &id)
                .await?
                .context("normal JSON observations")?;
            data.validate_against(&original)?;
            let scope_facts=measured_v2(&harness,&client,&id).await?.context("actual JSON v2 producer/getter")?;
            scope_facts.validate_against(&original)?;
            ensure!(scope_facts.operation_view()==Some(data.clone()));
            let scope=scope_facts.pipeline_scope.as_ref().context("actual finalized owning context")?;
            ensure!(!scope.streamed && scope.stream.is_none() && !scope.context_request_id.is_empty());
            ensure!(scope.request_duration_ms>=data.operations[0].upstream_duration_ms.context("operation elapsed")?);
            ensure!(scope_facts.hop_observations.len()==1);
            let hop=&scope_facts.hop_observations[0];
            ensure!(hop.profile==bitrouter_ai::tenancy::provider_technical_facts_v2::HopProfile::CoreJsonHopCallbacksV1);
            ensure!(hop.attempt_ids==data.operations[0].attempt_ids && hop.duration_ms.is_some() && !hop.partial);
            ensure!(hop.end==bitrouter_ai::tenancy::provider_technical_facts_v2::HopEnd::Completed);
            ensure!(data.operations.len() == 1 && !data.operations[0].streamed);
            ensure!(
                !data.operations[0].partial && data.operations[0].upstream_duration_ms.is_some()
            );
            ensure!(
                data.operations[0].first_semantic.is_none()
                    && data.operations[0].generation_duration_ms.is_none()
            );
            ensure!(
                data.operations[0].selected_attempt_id.as_ref()
                    == original.attempts.last().map(|attempt| &attempt.attempt_id)
            );
            for _ in 0..2 {
                ensure!(measured(&harness, &client, &id).await? == Some(data.clone()));
            }
            ensure!(
                harness.calls.load(Ordering::SeqCst) == 1,
                "reads started another inference"
            );
            Ok(())
        }
        .await;
        let shutdown = harness.fixture.service.shutdown().await;
        checked?;
        shutdown?;
    }
    Ok(())
}

#[tokio::test]
async fn actual_sse_semantic_before_hooks_is_one_delta_with_zero_generation() -> Result<()> {
    let (_home, harness) = owned(
        "chat_completions",
        StatusCode::OK,
        super::wire::chat_stream(full_usage("chat_completions"), true)?,
        true,
    )
    .await?;
    let checked: Result<()> = async {
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        ensure!(!response.bytes().await?.is_empty());
        tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&id)).await??;
        let original = harness.receipt(&client, &id).await?;
        let data = measured(&harness, &client, &id)
            .await?
            .context("normal SSE observations")?;
        data.validate_against(&original)?;
        let scope_facts=measured_v2(&harness,&client,&id).await?.context("actual SSE v2 producer/getter")?;
        scope_facts.validate_against(&original)?;
        ensure!(scope_facts.operation_view()==Some(data.clone()));
        let scope=scope_facts.pipeline_scope.as_ref().context("owning stream context")?;
        ensure!(scope.streamed);
        let stream=scope.stream.as_ref().context("singleton merged processor clock")?;
        ensure!(!stream.partial && stream.first_semantic==data.operations[0].first_semantic && stream.generation_duration_ms==Some(0));
        ensure!(stream.source_attempt_id==data.operations[0].selected_attempt_id && stream.terminal_attempt_id==data.operations[0].selected_attempt_id);
        ensure!(scope_facts.hop_observations.len()==1);
        let hop=&scope_facts.hop_observations[0];
        ensure!(hop.profile==bitrouter_ai::tenancy::provider_technical_facts_v2::HopProfile::CoreStreamHopNotifyV1);
        ensure!(hop.end==bitrouter_ai::tenancy::provider_technical_facts_v2::HopEnd::Completed && !hop.partial && hop.duration_ms.is_some());
        let operation = &data.operations[0];
        ensure!(
            operation.streamed && !operation.partial && operation.upstream_duration_ms.is_some()
        );
        ensure!(
            operation.first_semantic.as_ref().context("semantic")?.kind == FirstSemanticKind::Text
        );
        ensure!(operation.generation_duration_ms == Some(0));
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    let shutdown = harness.fixture.service.shutdown().await;
    checked?;
    shutdown?;
    Ok(())
}

#[tokio::test]
async fn actual_optional_sql_refusal_never_rolls_back_or_relabels_original_receipt() -> Result<()> {
    let (_home, harness) = owned(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
    )
    .await?;
    let checked:Result<()>=async {
        crate::db::migration::provider_technical::install(&harness.fixture.db).await?;
        harness.fixture.db.execute_unprepared("CREATE TRIGGER refuse_optional_provider BEFORE INSERT ON tenancy_provider_technical_facts BEGIN SELECT RAISE(ABORT,'diagnostic only'); END").await?;
        let client=Harness::client(false)?;let response=harness.send(&client).send().await?;
        ensure!(response.status()==StatusCode::OK);let id=execution_id(&response)?;let _body=response.bytes().await?;
        let original=harness.receipt(&client,&id).await?;
        ensure!(original.outcome==bitrouter_ai::tenancy::execution::Outcome::Completed);
        ensure!(measured(&harness,&client,&id).await?.is_none());
        let terminal=store::lookup_terminal(&harness.fixture.db,&harness.fixture.installation,&harness.fixture.tenant,&id).await?.context("original ACK retained")?;
        let before=(terminal.json().to_owned(),terminal.private_json().to_owned(),terminal.digest().to_owned());
        harness.fixture.db.execute_unprepared("DROP TRIGGER refuse_optional_provider").await?;
        ensure!(measured(&harness,&client,&id).await?.is_none(),"read repaired missing metrics");
        let after=store::lookup_terminal(&harness.fixture.db,&harness.fixture.installation,&harness.fixture.tenant,&id).await?.context("original immutable")?;
        ensure!(after.json()==before.0 && after.private_json()==before.1 && after.digest()==before.2);
        ensure!(harness.calls.load(Ordering::SeqCst)==1);
        let tx=harness.fixture.db.begin().await?;
        // Ordinary protected read remains authorized; no second send/financial
        // callback or caller-supplied technical value is used to repair absence.
        tx.rollback().await?;Ok(())
    }.await;
    let shutdown = harness.fixture.service.shutdown().await;
    checked?;
    shutdown?;
    Ok(())
}

#[tokio::test]
async fn corrupt_technical_getter_does_not_fence_receipts_or_bypass_tenant_authority() -> Result<()>
{
    let (_home, harness) = owned(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
    )
    .await?;
    let checked: Result<()> = async {
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        let _body = response.bytes().await?;
        tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&id)).await??;
        let original = harness.receipt(&client, &id).await?;
        ensure!(measured(&harness, &client, &id).await?.is_some());
        for version in [1, 2] {
            let wrong = client
            .get(format!(
                "{}/control/v1/tenants/foreign/executions/{id}/provider-technical-facts/v{version}",
                harness.base
            ))
            .bearer_auth(harness.credential.secret.as_deref().context("credential")?)
            .header(
                "Bitrouter-Installation-Id",
                harness.fixture.installation.as_str(),
            )
            .send()
            .await?;
            ensure!(wrong.status() == StatusCode::FORBIDDEN);
        }
        harness
            .fixture
            .db
            .execute_unprepared(
                "UPDATE tenancy_provider_technical_facts SET technical_digest=printf('%064d',0)",
            )
            .await?;
        for version in [1, 2] {
            let invalid = client
                .get(format!(
                    "{}/control/v1/tenants/{}/executions/{id}/provider-technical-facts/v{version}",
                    harness.base, harness.fixture.tenant
                ))
                .bearer_auth(harness.credential.secret.as_deref().context("credential")?)
                .header(
                    "Bitrouter-Installation-Id",
                    harness.fixture.installation.as_str(),
                )
                .send()
                .await?;
            ensure!(invalid.status() == StatusCode::SERVICE_UNAVAILABLE);
        }
        let after = harness.receipt(&client, &id).await?;
        ensure!(serde_json::to_vec(&after)? == serde_json::to_vec(&original)?);
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    let shutdown = harness.fixture.service.shutdown().await;
    checked?;
    shutdown?;
    Ok(())
}

mod lifecycle;

mod semantic;

#[cfg(unix)]
mod reopen;

#[cfg(feature = "server-tools")]
mod owned_loop;

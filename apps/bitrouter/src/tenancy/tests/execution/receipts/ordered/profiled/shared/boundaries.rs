use super::*;

#[tokio::test]
async fn shared_probe_stream_drop_releases_group_without_certifying_late_terminal() -> Result<()> {
    let mut profile = grouped(1000)?;
    let health = profile.health.as_mut().context("health")?;
    health.transient_cooldown_seconds = 0;
    health.persistent_cooldown_seconds = 0;
    health.probe_lease_seconds = 3600;
    let harness = harness(&[profile.clone(), profile.clone(), profile], false, false).await?;
    let protocol = GenerationProtocol::ChatCompletions;
    for index in [1, 2, 1] {
        let selected = binding(
            &harness,
            &harness.fixture,
            &harness.revision,
            &harness.route,
            index,
            protocol,
        )
        .await?;
        refused(
            &harness,
            &harness.fixture,
            invoke(
                &harness,
                harness.token()?,
                &selected,
                protocol,
                false,
                HttpVersion::Http1Only,
            )
            .await,
            503,
            1,
        )
        .await?;
    }
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates
            .iter()
            .all(|c| c.health == HealthObservation::ProbeReady)
    );
    let gate = Arc::new(barrier::Gate {
        entered: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
    });
    *harness
        .controls
        .held_stream
        .lock()
        .map_err(|_| anyhow::anyhow!("stream gate"))? = Some(gate.clone());
    let selected = binding(
        &harness,
        &harness.fixture,
        &harness.revision,
        &harness.route,
        0,
        protocol,
    )
    .await?;
    let client = GatewayClient::new(&harness.endpoint, None)?;
    let request: ChatRequest = serde_json::from_value(
        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":true}),
    )?;
    let reply = client
        .ordered_tenant(harness.token()?, &selected)?
        .chat_reply(&request)
        .await?;
    let id = reply.metadata.execution_id.context("execution")?;
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    let sibling = binding(
        &harness,
        &harness.fixture,
        &harness.revision,
        &harness.route,
        2,
        protocol,
    )
    .await?;
    refused(
        &harness,
        &harness.fixture,
        invoke(
            &harness,
            harness.token()?,
            &sibling,
            protocol,
            false,
            HttpVersion::Http1Only,
        )
        .await,
        503,
        0,
    )
    .await?;
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates
            .iter()
            .all(|c| c.health == HealthObservation::Blocked)
    );
    drop(reply.body);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(
        receipt.state == ReceiptState::Unknown
            && receipt.outcome == Outcome::Unknown
            && receipt.attempts.len() == 1
    );
    ensure!(
        receipt.attempts[0].candidate_index == 0
            && receipt.attempts[0].observed_http_status == Some(200)
    );
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates
            .iter()
            .all(|c| c.health == HealthObservation::ProbeReady)
    );
    gate.release.add_permits(1);
    // A sibling can claim the released group; its actual provider failure is
    // retained, and a later successful probe is the only recovery witness.
    refused(
        &harness,
        &harness.fixture,
        invoke(
            &harness,
            harness.token()?,
            &sibling,
            protocol,
            false,
            HttpVersion::Http1Only,
        )
        .await,
        503,
        1,
    )
    .await?;
    let completed = invoke(
        &harness,
        harness.token()?,
        &selected,
        protocol,
        true,
        HttpVersion::Http1Only,
    )
    .await?;
    ensure!(harness.receipt(&completed).await?.outcome == Outcome::Completed);
    ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == serde_json::to_vec(&receipt)?);
    let observation = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?;
    ensure!(
        observation
            .candidates
            .iter()
            .all(|c| c.health == HealthObservation::Healthy)
    );
    for candidate in observation.candidates {
        ensure!(candidate.rate.context("rate")?.shared_count == 6);
    }
    ensure!(
        harness.observed()?
            == [
                "model-1", "model-2", "model-1", "model-0", "model-2", "model-0"
            ]
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn actual_resource_put_version_dispatch_refuses_raw_ambiguity_and_normalizes_exact_numbers()
-> Result<()> {
    let harness = harness(&[grouped(100)?], false, false).await?;
    let command = ResourcePutV2 {
        format: 2,
        operation_id: "raw-shared-member".try_into()?,
        installation_id: harness.fixture.installation.clone(),
        key: ResourceRef {
            resource_id: "new-member".try_into()?,
            ..reference(0)?
        }
        .key(),
        expected_revision: None,
        definition: DefinitionV2::AdmissionProfileV2 {
            profile: grouped(7)?,
        },
    };
    let raw = serde_json::to_string(&command)?;
    let client = reqwest::Client::new();
    let send = |body: String| {
        client
            .post(format!(
                "{}/control/v1/tenants/{}/resources",
                harness.endpoint, harness.fixture.tenant
            ))
            .bearer_auth(OPERATOR)
            .header(
                "Bitrouter-Installation-Id",
                harness.fixture.installation.as_str(),
            )
            .header("content-type", "application/json")
            .body(body)
            .send()
    };
    let before = harness.fixture.service.current().map_err(failure)?;
    let revision = before.revision;
    let count = before.resource_lifecycle.len();
    drop(before);
    for invalid in [
        raw.replace("\"format\":2,", ""),
        raw.replace("\"format\":2", "\"format\":2,\"format\":2"),
        raw.replace("\"format\":2", "\"format\":3"),
        raw.replace("\"format\":2", "\"format\":1"),
        raw.replace("\"format\":2", "\"format\":2.00000000000000001"),
        raw.replace("\"health_group_id\":\"shared-circuit\",", ""),
        raw.replace(
            "\"health_group_id\":\"shared-circuit\"",
            "\"health_group_id\":null",
        ),
        raw.replace(
            "\"health_group_id\":\"shared-circuit\"",
            "\"health_group_id\":\"shared-circuit\",\"health_group_id\":\"shared-circuit\"",
        ),
        raw.replace("\"member_rpm\":7", "\"member_rpm\":7.1"),
        raw.replace(
            "\"owner_kind\":\"operator_pool\"",
            "\"owner_kind\":\"tenant\"",
        ),
    ] {
        ensure!(
            invalid != raw,
            "negative fixture mutation did not change bytes"
        );
        let response = send(invalid).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        let after = harness.fixture.service.current().map_err(failure)?;
        ensure!(
            after.revision == revision
                && after.resource_lifecycle.len() == count
                && after.resource_lifecycle.head(&command.key).is_none()
        );
    }
    let normalized = raw
        .replace("\"format\":2", "\"format\":2e0")
        .replace("\"schema_version\":2", "\"schema_version\":2.0")
        .replace("\"member_rpm\":7", "\"member_rpm\":7e0");
    let response = send(normalized).await?;
    ensure!(response.status().is_success());
    let original: bitrouter_ai::tenancy::operation::OperationResult = response.json().await?;
    let replay = harness
        .control(true, HttpVersion::Http1Only)?
        .put_resource_v2(&harness.fixture.tenant, &command)
        .await?;
    ensure!(
        original.resource_revision.is_some()
            && original.resource_revision == replay.resource_revision
    );
    ensure!(harness.observed()?.is_empty());
    harness.fixture.service.shutdown().await?;
    Ok(())
}

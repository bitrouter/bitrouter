use super::*;

#[tokio::test]
async fn profiled_observer_only_root_reads_real_shared_count_without_rate_or_health_admission()
-> Result<()> {
    let observer: AdmissionProfileV1 =
        serde_json::from_value(json!({"schema_version":1,"rate":null,
        "observe_rate_group_id":"shared-rpm","health":null,"fallback":"stop"}))?;
    let harness = OrderedHarness::with_resource_edits(
        2,
        8,
        None,
        Value::Null,
        Some(&[observer, rules(1, true)?]),
        |resources| {
            let route = resources
                .iter()
                .find(|entry| entry["reference"]["resource_kind"] == "route")
                .context("root")?;
            let mut separate = route.clone();
            separate["reference"]["resource_id"] = json!("observer-only");
            let definition = &mut separate["definition"];
            let endpoints = definition["routing"]["models"]["ordered-root"]["endpoints"]
                .as_array_mut()
                .context("endpoints")?;
            endpoints.truncate(1);
            definition["routing"]["providers"]
                .as_object_mut()
                .context("providers")?
                .retain(|name, _| name == "provider-0");
            definition["credentials"]
                .as_array_mut()
                .context("credentials")?
                .truncate(1);
            definition["attachments"]
                .as_array_mut()
                .context("attachments")?
                .retain(|row| row["candidate_index"] == 0);
            resources.push(separate);
            Ok(())
        },
    )
    .await?;
    let mut observer_route = harness.route.clone();
    observer_route.resource_id = "observer-only".try_into()?;
    let revision = draft_profiles(&harness.fixture, &observer_route).await?;
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let read = || {
        control.candidate_manifest(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &revision.revision_id,
            &observer_route,
            GenerationProtocol::ChatCompletions,
        )
    };
    let initial = read().await?;
    ensure!(initial.candidates.len() == 1);
    let first = &initial
        .runtime_observation
        .as_ref()
        .context("observation")?
        .candidates[0];
    ensure!(first.rate.is_none() && first.observed_shared_count == Some(0));
    let binding = control.ordered_capabilities().await?.bind(
        revision.revision_id.clone(),
        "observer-call".try_into()?,
        OrderedCandidateSelectionV1 {
            schema_version: 1,
            admission_mode: initial.admission_mode,
            route: observer_route.clone(),
            candidate_indices: vec![0],
            generation_defaults_digest: None,
        },
    )?;
    harness
        .controls
        .rejection_status
        .store(401, Ordering::SeqCst);
    let rated = harness
        .binding(vec![1], GenerationProtocol::ChatCompletions)
        .await?;
    ensure!(
        harness
            .invoke(
                &rated,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only
            )
            .await
            .is_err()
    );
    let platform = manifest(&harness)
        .await?
        .runtime_observation
        .context("platform observation")?
        .candidates
        .remove(1);
    ensure!(platform.rate.context("shared count")?.shared_count == 1);
    ensure!(platform.health == HealthObservation::Blocked);
    let client = GatewayClient::new(&harness.endpoint, None)?;
    for _ in 0..4 {
        let observed = read().await?;
        let candidate = &observed
            .runtime_observation
            .as_ref()
            .context("observation")?
            .candidates[0];
        ensure!(candidate.rate.is_none() && candidate.observed_shared_count == Some(1));
        ensure!(
            candidate.health == HealthObservation::Disabled
                && candidate.availability == AvailabilityObservation::Eligible
        );
        let input: ChatRequest = serde_json::from_value(
            json!({"model":"observer-only","messages":[{"role":"user","content":"hello"}]}),
        )?;
        let reply = client
            .ordered_tenant(harness.token()?, &binding)?
            .chat_reply(&input)
            .await?;
        let id = reply.metadata.execution_id.context("observer execution")?;
        let receipt = control
            .execution_v2(&harness.fixture.installation, &harness.fixture.tenant, &id)
            .await?;
        ensure!(
            receipt.outcome == Outcome::Completed
                && receipt.accepted_selection.route == observer_route
        );
    }
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observations")?
            .candidates[1]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 1
    );
    // A missing current authority is not an observed empty group.
    let authority = harness.fixture.service.current().map_err(failure)?.revision;
    control
        .revoke_resource(
            &harness.fixture.tenant,
            &bitrouter_ai::tenancy::resources::ResourceRevoke {
                format: 1,
                operation_id: "revoke-observer".try_into()?,
                installation_id: harness.fixture.installation.clone(),
                reference: reference(0)?,
                expected_authority_revision: authority,
            },
        )
        .await?;
    let denied = read()
        .await?
        .runtime_observation
        .context("observation")?
        .candidates
        .remove(0);
    ensure!(denied.observed_shared_count.is_none() && denied.rate.is_none());
    ensure!(denied.availability == AvailabilityObservation::AuthorityDenied);
    ensure!(harness.observed()? == ["model-1", "model-0", "model-0", "model-0", "model-0"]);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_actual_http_manifests_have_one_shared_snapshot_during_real_admissions()
-> Result<()> {
    let mut profile = rules(1000, false)?;
    profile.observe_rate_group_id = Some("shared-rpm".try_into()?);
    let harness =
        OrderedHarness::with_profiles(64, 64, None, Value::Null, Some(&vec![profile; 64])).await?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let sends = async {
        for _ in 0..32 {
            harness
                .invoke(
                    &binding,
                    GenerationProtocol::ChatCompletions,
                    false,
                    HttpVersion::Http1Only,
                )
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let reads = async {
        for _ in 0..32 {
            let observation = manifest(&harness)
                .await?
                .runtime_observation
                .context("observation")?;
            ensure!(observation.candidates.len() == 64);
            let count = observation.candidates[0]
                .rate
                .as_ref()
                .context("rate")?
                .shared_count;
            for candidate in observation.candidates {
                ensure!(
                    candidate.rate.context("rate")?.shared_count == count
                        && candidate.observed_shared_count == Some(count)
                );
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let (sent, read) = tokio::join!(sends, reads);
    sent?;
    read?;
    ensure!(harness.observed()?.len() == 32);
    ensure!(
        manifest(&harness)
            .await?
            .runtime_observation
            .context("observation")?
            .candidates[0]
            .observed_shared_count
            == Some(32)
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

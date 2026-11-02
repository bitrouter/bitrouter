//! Real reached-hop boundaries and advisory read-only group snapshots.
use super::*;
use bitrouter_ai::tenancy::{
    operation::OperationStatus,
    resources::{Definition, ResourcePut, ResourceRevoke},
};

#[tokio::test]
async fn profiled_equal_private_targets_use_original_ordinals_and_distinct_profiles() -> Result<()>
{
    let mut profiles = vec![rules(1000, false)?, rules(0, false)?, rules(1000, false)?];
    for (index, profile) in profiles.iter_mut().enumerate() {
        profile.rate.as_mut().context("rate")?.rate_group_id =
            format!("group-{index}").try_into()?;
    }
    let harness = OrderedHarness::with_resource_edits(
        3,
        8,
        None,
        Value::Null,
        Some(&profiles),
        |resources| {
            let route = resources
                .iter_mut()
                .find(|row| row["reference"]["resource_kind"] == "route")
                .context("route")?;
            let definition = &mut route["definition"];
            let same = definition["routing"]["models"]["ordered-root"]["endpoints"][1].clone();
            definition["routing"]["models"]["ordered-root"]["endpoints"][2] = same;
            let credential = definition["credentials"][1]["credential"].clone();
            for row in definition["attachments"]
                .as_array_mut()
                .context("attachments")?
            {
                if row["candidate_index"] == 2 {
                    row["credential"] = credential.clone();
                }
            }
            // The compiler requires an exact credential closure. Replacing
            // endpoint 2 also removes its now-unused provider/account slot.
            definition["credentials"]
                .as_array_mut()
                .context("credentials")?
                .truncate(2);
            definition["routing"]["providers"]
                .as_object_mut()
                .context("providers")?
                .remove("provider-2");
            Ok(())
        },
    )
    .await?;
    let manifest = manifest(&harness).await?;
    ensure!(manifest.candidates.len() == 3);
    ensure!(
        serde_json::to_value(&manifest.candidates[1].target)?
            == serde_json::to_value(&manifest.candidates[2].target)?
    );
    ensure!(manifest.candidates[1].credential == manifest.candidates[2].credential);
    let binding = harness
        .control(true, HttpVersion::Http1Only)?
        .ordered_capabilities()
        .await?
        .bind(
            harness.revision.revision_id.clone(),
            "equal-projections".try_into()?,
            OrderedCandidateSelectionV1 {
                schema_version: 1,
                admission_mode: manifest.admission_mode,
                route: harness.route.clone(),
                candidate_indices: vec![2, 1, 0],
                generation_defaults_digest: None,
            },
        )?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    ensure!(
        harness
            .receipt(&id)
            .await?
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == [2, 0]
    );
    ensure!(harness.observed()? == ["model-1", "model-0"]);
    let observed = super::manifest(&harness).await?;
    ensure!(
        (
            count(&observed, 0)?,
            count(&observed, 1)?,
            count(&observed, 2)?
        ) == (1, 0, 1)
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

async fn revoke(harness: &OrderedHarness, reference: ResourceRef) -> Result<()> {
    let revision = harness.fixture.service.current().map_err(failure)?.revision;
    let result = harness
        .control(true, HttpVersion::Http1Only)?
        .revoke_resource(
            &harness.fixture.tenant,
            &ResourceRevoke {
                format: 1,
                operation_id: "revoke-operational-resource".try_into()?,
                installation_id: harness.fixture.installation.clone(),
                reference,
                expected_authority_revision: revision,
            },
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    Ok(())
}

async fn rotate(
    harness: &OrderedHarness,
    rpm: u32,
    expected: Option<bitrouter_ai::tenancy::identity::RevisionId>,
) -> Result<bitrouter_ai::tenancy::identity::RevisionId> {
    let result = harness
        .control(true, HttpVersion::Http1Only)?
        .put_resource(
            &harness.fixture.tenant,
            &ResourcePut {
                format: 1,
                operation_id: format!("rotate-rpm-{rpm}").try_into()?,
                installation_id: harness.fixture.installation.clone(),
                key: reference(0)?.key(),
                expected_revision: expected,
                definition: Definition::AdmissionProfile {
                    profile: rules(rpm, false)?,
                },
            },
        )
        .await?;
    ensure!(result.status == OperationStatus::Applied);
    result.resource_revision.context("new profile revision")
}

fn count(
    observation: &bitrouter_ai::tenancy::ordered::CandidateManifestV1,
    index: usize,
) -> Result<u64> {
    Ok(observation
        .runtime_observation
        .as_ref()
        .context("observation")?
        .candidates[index]
        .rate
        .as_ref()
        .context("count")?
        .shared_count)
}

#[tokio::test]
async fn profiled_arbitrary_64_of_256_preserves_255_and_ignores_revoked_unselected_profiles()
-> Result<()> {
    capacity_case(false).await
}

#[tokio::test]
async fn envelope_profiled_64_of_256_keeps_digest_without_changing_bounds() -> Result<()> {
    capacity_case(true).await
}

async fn capacity_case(envelope: bool) -> Result<()> {
    let order: Vec<u16> = (0..63).map(|i| 255 - i * 4).chain([0]).collect();
    let selected: std::collections::HashSet<_> = order.iter().copied().collect();
    let profiles = (0..256)
        .map(|i| rules(if selected.contains(&i) { 1000 } else { 0 }, false))
        .collect::<Result<Vec<_>>>()?;
    let harness = OrderedHarness::with_resource_edits(
        256,
        64,
        None,
        Value::Null,
        Some(&profiles),
        |resources| {
            let route = resources
                .iter_mut()
                .find(|row| row["reference"]["resource_kind"] == "route")
                .context("route")?;
            let definition = &mut route["definition"];
            let credential = definition["credentials"][0]["credential"].clone();
            // Keep every target and ordinal, but share immutable credentials
            // and two profiles so this boundary fits the existing byte cap.
            for slot in definition["credentials"]
                .as_array_mut()
                .context("credentials")?
            {
                slot["credential"] = credential.clone();
            }
            for attachment in definition["attachments"]
                .as_array_mut()
                .context("attachments")?
            {
                let index =
                    u16::try_from(attachment["candidate_index"].as_u64().context("ordinal")?)?;
                attachment["credential"] = credential.clone();
                attachment["profile"] =
                    serde_json::to_value(reference(usize::from(!selected.contains(&index)))?)?;
            }
            resources.retain(|row| match row["reference"]["resource_kind"].as_str() {
                Some("credential") => row["reference"] == credential,
                Some("policy") => matches!(
                    row["reference"]["resource_id"].as_str(),
                    Some("profile-0" | "profile-1")
                ),
                _ => true,
            });
            Ok(())
        },
    )
    .await?;
    let expected_scope = GrantScope::Tenant {
        operator_pools: vec![reference(0)?.key(), reference(1)?.key()],
    };
    let binding = crate::tenancy::store::directory::binding_in(
        &harness.fixture.db,
        &harness.fixture.tenant,
        &harness.fixture.binding,
        64 * 1024,
    )
    .await?
    .context("shared-profile binding")?;
    let credential = crate::tenancy::store::directory::credential_in(
        &harness.fixture.db,
        &harness.credential.credential_id,
        64 * 1024,
    )
    .await?
    .context("shared-profile credential")?;
    ensure!(binding.scope == expected_scope && credential.scope == expected_scope);
    let census = manifest(&harness).await?;
    ensure!(census.candidates.len() == 256);
    let mut providers = std::collections::HashSet::new();
    for (index, candidate) in census.candidates.iter().enumerate() {
        ensure!(usize::from(candidate.candidate_index) == index);
        ensure!(candidate.target.model_id == format!("model-{index}"));
        ensure!(providers.insert(candidate.target.provider_id.clone()));
        let expected = crate::tenancy::receipts::journal::project_target(
            &harness.fixture.installation,
            &harness.route,
            &candidate.credential,
            &format!("provider-{index}"),
            &format!("model-{index}"),
            None,
        )
        .map_err(|error| anyhow::anyhow!("target projection: {error:?}"))?;
        ensure!(serde_json::to_value(&candidate.target)? == serde_json::to_value(expected)?);
    }
    let binding = harness
        .binding(order.clone(), GenerationProtocol::ChatCompletions)
        .await?;
    revoke(&harness, reference(1)?).await?;
    let values: bitrouter_ai::tenancy::generation_defaults::GenerationDefaultsEnvelopeV1 =
        serde_json::from_value(
            json!({"schema_version":1,"supplemental":{"vendor_knob":"bounded"}}),
        )?;
    let id = if envelope {
        let binding = binding.with_generation_defaults_envelope(&values)?;
        super::super::envelope::invoke(
            &harness,
            &binding,
            values.clone(),
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
            json!({}),
        )
        .await?
    } else {
        harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
            )
            .await?
    };
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.outcome == Outcome::Completed);
    if envelope {
        ensure!(receipt.accepted_selection.generation_defaults_digest == Some(values.digest()?));
        let bodies = harness
            .controls
            .bodies
            .lock()
            .map_err(|_| anyhow::anyhow!("provider bodies"))?;
        ensure!(bodies.len() == 64 && bodies.iter().all(|body| body["vendor_knob"] == "bounded"));
    }
    ensure!(receipt.accepted_selection.candidate_indices == order);
    ensure!(
        receipt
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == order
    );
    ensure!(
        harness.observed()?
            == order
                .iter()
                .map(|i| format!("model-{i}"))
                .collect::<Vec<_>>()
    );
    ensure!(count(&manifest(&harness).await?, 0)? == 64);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_success_prefix_never_charges_an_unreached_distinct_group() -> Result<()> {
    let mut profiles = vec![rules(5, false)?; 3];
    for (index, profile) in profiles.iter_mut().enumerate() {
        profile.rate.as_mut().context("rate")?.rate_group_id =
            format!("group-{index}").try_into()?;
    }
    let harness = OrderedHarness::with_profiles(3, 8, None, Value::Null, Some(&profiles)).await?;
    let binding = harness
        .binding(vec![1, 0, 2], GenerationProtocol::ChatCompletions)
        .await?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    ensure!(
        harness
            .receipt(&id)
            .await?
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == [1, 0]
    );
    let observed = manifest(&harness).await?;
    ensure!(
        (
            count(&observed, 0)?,
            count(&observed, 1)?,
            count(&observed, 2)?
        ) == (1, 1, 0)
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn profiled_current_authority_revocation_after_charge_never_arms_or_falls_back() -> Result<()>
{
    for change in ["profile", "credential", "grant"] {
        let harness = OrderedHarness::with_profiles(
            2,
            8,
            None,
            Value::Null,
            Some(&[rules(500, false)?, rules(500, false)?]),
        )
        .await?;
        let binding = harness
            .binding(vec![0, 1], GenerationProtocol::ChatCompletions)
            .await?;
        let gate = harness
            .fixture
            .service
            .hold_receipt_credential_read_for_test()?;
        let release = gate.release.clone().drop_guard();
        let call = barrier::submit(&harness, &binding)?;
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let id = gate
            .execution
            .get()
            .context("credential barrier identity")?
            .clone();
        ensure!(harness.observed()?.is_empty() && count(&manifest(&harness).await?, 1)? == 1);
        match change {
            "profile" => revoke(&harness, reference(0)?).await?,
            "credential" => {
                let credential = manifest(&harness).await?.candidates[0].credential.clone();
                revoke(&harness, credential).await?;
            }
            _ => authorization::narrow_binding(&harness, &[0, 1]).await?,
        }
        gate.release.cancel();
        let error = tokio::time::timeout(TIMEOUT, call)
            .await??
            .err()
            .context("revoked call succeeded")?;
        let bitrouter_sdk::gateway::tenancy::Error::Response(error) = error else {
            anyhow::bail!("expected authority refusal")
        };
        ensure!(error.metadata.execution_id.as_ref() == Some(&id));
        ensure!(
            error.tenancy_error.context("authority error")?.code
                == bitrouter_ai::tenancy::operation::ErrorCode::Forbidden
        );
        // Revoked tenant authority cannot be reused for audit readback. This
        // operator read grants no submission authority or fallback send.
        let audit = harness.control(true, HttpVersion::Http1Only)?;
        let receipt = audit
            .execution_v2(&harness.fixture.installation, &harness.fixture.tenant, &id)
            .await?;
        receipt.validate()?;
        audit
            .usage_evidence_v2(&receipt)
            .await?
            .validate_against(&receipt)?;
        if change == "grant" {
            let error = harness
                .control(false, HttpVersion::Http1Only)?
                .execution_v2(&harness.fixture.installation, &harness.fixture.tenant, &id)
                .await
                .err()
                .context("narrowed tenant read unexpectedly succeeded")?;
            ensure!(
                matches!(error, bitrouter_sdk::error::Error::Tenancy(failure) if failure.status == 403)
            );
        }
        ensure!(receipt.attempts.is_empty() && receipt.outcome == Outcome::Failed);
        ensure!(harness.observed()?.is_empty() && count(&manifest(&harness).await?, 1)? == 1);
        drop(release);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn profiled_rpm_rotation_is_not_retroactive_and_never_refunds_a_reached_hop() -> Result<()> {
    let harness =
        OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[rules(500, false)?])).await?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let gate = harness
        .fixture
        .service
        .hold_receipt_credential_read_for_test()?;
    let release = gate.release.clone().drop_guard();
    let call = barrier::submit(&harness, &binding)?;
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(harness.observed()?.is_empty() && count(&manifest(&harness).await?, 0)? == 1);
    let revision = rotate(&harness, 0, Some(reference(0)?.revision_id)).await?;
    gate.release.cancel();
    tokio::time::timeout(TIMEOUT, call).await???;
    ensure!(harness.observed()? == ["model-0"]);
    let id = gate.execution.get().context("execution")?;
    ensure!(harness.receipt(id).await?.outcome == Outcome::Completed);
    for expected in [None, Some(revision)] {
        if let Some(expected) = expected {
            rotate(&harness, 1, Some(expected)).await?;
        }
        let error = harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
            )
            .await
            .err()
            .context("charged window admitted")?;
        let Some(bitrouter_sdk::gateway::tenancy::Error::Response(error)) = error.downcast_ref()
        else {
            anyhow::bail!("expected rate error")
        };
        ensure!(error.metadata.status == 429 && error.metadata.execution_id.is_some());
        ensure!(count(&manifest(&harness).await?, 0)? == 1);
        ensure!(harness.observed()? == ["model-0"]);
    }
    drop(release);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

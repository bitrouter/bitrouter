//! Configured dispatch recovery retains original facts without retaining its graph.

use super::*;
use crate::tenancy::{
    receipts::{ordered::intent, owner::Identity, read::Request as ReceiptRequest, store},
    transaction::{Control, Disposition},
};
use bitrouter_ai::tenancy::{
    execution::{Provenance, ReceiptState},
    usage_evidence::{Capture, Estimation},
    usage_evidence_v2::ExecutionUsageEvidenceV2,
};

const OLD_PRIMARY: &str = "original-configured-primary-secret";
const OLD_BACKUP: &str = "original-configured-backup-secret";
const NEW_ROUTE: &str = "model.acme.replacement";

#[tokio::test]
async fn original_configured_intent_recovers_unknown_after_catalog_and_key_change() -> Result<()> {
    let original_provider = Provider::start().await?;
    let replacement_provider = Provider::start().await?;
    let mut harness = Harness::new(&original_provider.url).await?;
    let provider = harness
        .config
        .providers
        .get_mut("mock")
        .context("provider")?;
    provider.api_key.clear();
    provider.accounts = [("primary", OLD_PRIMARY), ("backup", OLD_BACKUP)]
        .into_iter()
        .map(|(label, secret)| crate::config::ProviderAccount {
            label: label.into(),
            api_key: secret.into(),
            api_base: String::new(),
        })
        .collect();
    let exposure = harness.config.tenancy.catalog.as_mut().context("catalog")?;
    let profile = exposure.profiles.remove(SOURCE).context("profile")?;
    for label in ["primary", "backup"] {
        exposure
            .profiles
            .insert(format!("mock/{label}/acme/shared"), profile.clone());
    }
    harness = harness.restart().await?;
    let descriptor = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    ensure!(descriptor.manifest.candidates.len() == 2);
    let GrantScope::Resources { resources } = scope(&descriptor) else {
        anyhow::bail!("configured dependency scope");
    };
    // The original ingress credential also retains ordinary tenant receipt read.
    let credential = grant(
        &mut harness.fixture,
        GrantScope::Tenant {
            operator_pools: resources,
        },
        TenantRole::TenantAdmin.permissions().to_vec(),
        "configured-recovery-reader",
    )
    .await?;
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let original = catalog(&harness.fixture, &authenticated, &harness.fixture.tenant).await?;
    let selection = OrderedCandidateSelectionV1 {
        schema_version: 1,
        admission_mode: original.manifest.admission_mode,
        route: original.manifest.route.clone(),
        candidate_indices: vec![1, 0],
        generation_defaults_digest: None,
    }
    .accept(GenerationProtocol::ChatCompletions)?;
    let original_revision = original.manifest.execution_revision.clone();
    let original_instance = harness.fixture.service.instance.clone();
    let original_origin = harness
        .fixture
        .service
        .startup
        .as_ref()
        .context("original owned origin")?
        .bootstrap_stamp(&harness.fixture.service.cleanup)?;
    let execution: ExecutionId = uuid::Uuid::new_v4().to_string().try_into()?;
    let identity = Identity {
        installation: harness.fixture.installation.clone(),
        tenant: harness.fixture.tenant.clone(),
        principal: harness.fixture.principal.clone(),
        execution: execution.clone(),
        correlation: "configured-recovery-correlation".try_into()?,
        revision: original_revision.clone(),
        ordered: Some((
            selection.clone(),
            harness
                .fixture
                .service
                .execution_settings
                .ordered_attempt_limit,
        )),
    };
    let pending = intent::Intent::new(&identity, original_origin.clone())?;
    let before = publication_counts(&harness.fixture).await?;
    ensure!(before[..4] == [0, 0, 0, 0]);
    ensure!(original_provider.calls() == 0 && replacement_provider.calls() == 0);

    // Quiesce the actor before its real owned transaction. No live execution
    // owner can gracefully terminalize this deliberate pre-send interruption.
    harness.fixture.service.shutdown().await?;
    let control = Control::new(harness.fixture.service.cleanup.clone());
    let transaction = control.begin_write(&harness.fixture.db).await?;
    intent::insert(&transaction, &pending).await?;
    control.commit(transaction).await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Committed);
    drop(control);
    ensure!(
        intent::inventory(&harness.fixture.db, &harness.fixture.installation)
            .await?
            .rows
            == 1
    );
    ensure!(
        store::inventory(&harness.fixture.db, &harness.fixture.installation)
            .await?
            .rows
            == 0
    );
    let row = harness.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT intent_json FROM tenancy_dispatch_intents WHERE installation_id=? AND tenant_id=? AND execution_id=?",
        [harness.fixture.installation.as_str().into(), harness.fixture.tenant.as_str().into(), execution.as_str().into()],
    )).await?.context("original persisted intent")?;
    let encoded: String = row.try_get("", "intent_json")?;
    let saved: intent::Intent = serde_json::from_str(&encoded)?;
    ensure!(serde_json::to_value(&saved)? == serde_json::to_value(&pending)?);
    let persisted: Value = serde_json::from_str(&encoded)?;
    ensure!(
        persisted["revision"] == serde_json::to_value(&original_revision)?
            && persisted["selection"] == serde_json::to_value(&selection)?
            && persisted["origin"] == serde_json::to_value(&original_origin)?
    );
    for confidential in [
        OLD_PRIMARY,
        OLD_BACKUP,
        original_provider.url.as_str(),
        "\"providers\"",
        "\"document\"",
    ] {
        ensure!(
            !encoded.contains(confidential),
            "intent retained a provider secret or graph"
        );
    }

    // The replacement graph has neither the old public root nor either key.
    let provider = harness
        .config
        .providers
        .get_mut("mock")
        .context("replacement provider")?;
    provider.api_base = replacement_provider.url.clone();
    for (account, secret) in provider.accounts.iter_mut().zip([
        "replacement-configured-primary-secret",
        "replacement-configured-backup-secret",
    ]) {
        account.api_key = secret.into();
    }
    let exposure = harness
        .config
        .tenancy
        .catalog
        .as_mut()
        .context("replacement catalog")?;
    exposure.routes.clear();
    exposure
        .routes
        .insert(NEW_ROUTE.into(), "acme/shared".into());
    harness = harness.restart().await?;
    let replacement_origin = harness
        .fixture
        .service
        .startup
        .as_ref()
        .context("replacement owned origin")?
        .bootstrap_stamp(&harness.fixture.service.cleanup)?;
    ensure!(
        replacement_origin != original_origin
            && harness.fixture.service.instance != original_instance
    );
    ensure!(
        serde_json::to_value(&replacement_origin)?["acquisition"]
            != serde_json::to_value(&original_origin)?["acquisition"]
    );
    let replacement = harness
        .fixture
        .service
        .catalog_candidates(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            NEW_ROUTE,
            GenerationProtocol::ChatCompletions,
        )
        .await
        .map_err(failure)?;
    ensure!(replacement.manifest.execution_revision != original_revision);
    ensure!(replacement.manifest.candidates.iter().all(|candidate| {
        original
            .manifest
            .candidates
            .iter()
            .all(|old| old.credential != candidate.credential)
    }));
    ensure!(
        !harness
            .fixture
            .service
            .resources
            .configured()
            .context("replacement graph")?
            .contains(&original.manifest.route)
    );
    ensure!(
        matches!(harness.fixture.service.catalog_candidates(&harness.fixture.operator,
        &harness.fixture.tenant, ROUTE, GenerationProtocol::ChatCompletions).await,
        Err(error) if error.0.code == ErrorCode::NotFound)
    );

    // Read using the original real tenant credential, without an old graph or key.
    let authenticated = caller(&harness.fixture.service, &credential).await?;
    let receipt_bytes = harness
        .fixture
        .service
        .read_receipt(
            authenticated.clone(),
            harness.fixture.tenant.clone(),
            ReceiptRequest::OneV2(execution.clone()),
        )
        .await
        .map_err(failure)?;
    let recovered: ExecutionReceiptV2 = serde_json::from_slice(&receipt_bytes)?;
    recovered.validate()?;
    ensure!(
        recovered.execution_id == execution
            && recovered.tenant_id == harness.fixture.tenant
            && recovered.principal_id == harness.fixture.principal
            && recovered.execution_revision == original_revision
            && recovered.accepted_selection == selection
    );
    ensure!(
        recovered.accepted_selection.candidate_indices == [1, 0]
            && recovered.state == ReceiptState::Unknown
            && recovered.outcome == Outcome::Unknown
            && recovered.attempts.is_empty()
            && recovered.usage.provenance == Provenance::Unknown
            && !recovered.usage.complete
            && recovered.usage.input_tokens.is_none()
            && recovered.usage.output_tokens.is_none()
    );
    ensure!(serde_json::to_value(&recovered.recorded_at)? == persisted["admitted_at"]);
    let evidence_bytes = harness
        .fixture
        .service
        .read_receipt(
            authenticated,
            harness.fixture.tenant.clone(),
            ReceiptRequest::UsageEvidenceV2(execution.clone()),
        )
        .await
        .map_err(failure)?;
    let evidence: ExecutionUsageEvidenceV2 = serde_json::from_slice(&evidence_bytes)?;
    evidence.validate_against(&recovered)?;
    ensure!(
        evidence.execution_id == execution
            && evidence.accepted_selection == selection
            && evidence.capture == Capture::Unavailable
            && evidence.estimation == Estimation::NotCaptured
            && evidence.estimate.is_none()
            && evidence.attempts.is_empty()
            && evidence.frames.is_empty()
    );
    let inventory = store::inventory(&harness.fixture.db, &harness.fixture.installation).await?;
    ensure!(inventory.rows == 1 && inventory.unknown_count == 1);
    ensure!(
        intent::inventory(&harness.fixture.db, &harness.fixture.installation)
            .await?
            .rows
            == 0
    );
    ensure!(publication_counts(&harness.fixture).await? == before);
    for bytes in [&receipt_bytes, &evidence_bytes] {
        let document = std::str::from_utf8(bytes)?;
        ensure!(
            !document.contains(OLD_PRIMARY)
                && !document.contains(OLD_BACKUP)
                && !document.contains(&original_provider.url)
        );
    }
    ensure!(
        original_provider.calls() == 0 && replacement_provider.calls() == 0,
        "startup replayed configured provider work"
    );
    harness.fixture.service.shutdown().await?;
    Ok(())
}

use super::*;
use crate::tenancy::receipts::{
    ordered::intent::{self, Intent},
    owner::Identity,
    store::format::Terminal,
};
use bitrouter_ai::tenancy::{
    admission::AdmissionMode,
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution::{Outcome, Provenance, ReceiptState},
    identity::ExecutionId,
    ordered::{AcceptedSelection, GenerationProtocol},
    usage_evidence::{Capture, Estimation, ExecutionUsageEvidence},
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn identity(fixture: &Fixture, route: ResourceRef, id: &str) -> Result<Identity> {
    Ok(Identity {
        installation: fixture.installation.clone(),
        tenant: fixture.tenant.clone(),
        principal: fixture.principal.clone(),
        execution: id.try_into()?,
        correlation: "recovered-correlation".try_into()?,
        revision: "recovered-configuration".try_into()?,
        ordered: Some((
            AcceptedSelection {
                generation_defaults_digest: None,
                admission_mode: AdmissionMode::Unprofiled,
                route,
                inbound_protocol: GenerationProtocol::ChatCompletions,
                candidate_indices: vec![0],
            },
            8,
        )),
    })
}

async fn recovered(
    service: &Service,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<(String, String)> {
    let installation = &service.current().map_err(failure)?.installation;
    let terminal = receipts::lookup_terminal(&service.db, installation, tenant, id)
        .await?
        .context("recovered receipt missing at ready")?;
    let Terminal::Ordered(candidate) = &terminal else {
        anyhow::bail!("recovered receipt lost ordered dispatch")
    };
    let receipt = &candidate.receipt;
    ensure!(
        receipt.state == ReceiptState::Unknown
            && receipt.outcome == Outcome::Unknown
            && receipt.usage.provenance == Provenance::Unknown
            && !receipt.usage.complete
            && receipt.attempts.is_empty()
    );
    let caller =
        Caller::Installation(crate::tenancy::operator_principal("test-operator").map_err(failure)?);
    let bytes = service
        .read_receipt(
            caller,
            tenant.clone(),
            crate::tenancy::receipts::read::Request::UsageEvidence(id.clone()),
        )
        .await
        .map_err(failure)?;
    let evidence: ExecutionUsageEvidence = serde_json::from_slice(&bytes)?;
    evidence.validate_against(receipt)?;
    ensure!(
        evidence.capture == Capture::Unavailable
            && evidence.estimation == Estimation::NotCaptured
            && evidence.estimate.is_none()
            && evidence.frames.is_empty()
            && evidence.attempts.is_empty()
    );
    ensure!(
        receipts::obligations::lookup(
            &service.db,
            installation,
            &receipts::obligations::Key {
                tenant: tenant.clone(),
                execution: id.clone(),
            },
            &|_, _| Ok(())
        )
        .await?
        .is_some(),
        "unknown recovery was not accounted"
    );
    Ok((
        terminal.digest().to_owned(),
        serde_json::to_string(&evidence)?,
    ))
}

#[tokio::test]
async fn unfinished_dispatch_recovers_once_before_ready_and_stays_unknown_on_restart() -> Result<()>
{
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let counted = provider_calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let router = axum::Router::new().route("/v1/chat/completions",axum::routing::post(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { axum::Json(serde_json::json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}})) }
    }));
    let _provider = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, router).await
    }));
    for rows in [3, 257] {
        let home = tempfile::tempdir()?;
        let path = home.path().join("recovery.sqlite");
        let (mut fixture, database) = fixture(&path).await?;
        let key = fixture.wrapping_key;
        history::seed(&mut fixture, rows).await?;
        let slot = history::reference(&fixture, "retained-slot-00000")?;
        let route = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "recovery-route".try_into()?,
            revision_id: "v1".try_into()?,
        };
        fixture.service.resources.provision_bytes(&fixture.db,&serde_json::to_vec(&serde_json::json!({
            "format":1,"installation_id":fixture.installation,"resources":[{
                "reference":route,"definition":{"kind":"route","selector":"bitrouter/recovery","policy":null,
                    "routing":{"providers":{"fixture":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"}]}},
                        "routers":{"recovery":{"selection":{"kind":"model","model":"fixture:model"}}}},
                    "credentials":[{"provider":"fixture","account":"default","credential":slot}]}
            }]
        }))?).await?;
        let identities = [
            identity(&fixture, route.clone(), "unfinished-a")?,
            identity(&fixture, route, "unfinished-b")?,
        ];
        let tenant = fixture.tenant.clone();
        stop_fixture(fixture, database).await?;
        let queries = Arc::new(Queries::default());
        let previous = open(&path, &queries, key).await?;
        previous.shutdown().await?;
        let origin = previous
            .startup
            .as_ref()
            .context("real startup owner missing")?
            .bootstrap_stamp(&previous.cleanup)?;
        // Preserve exactly what the original writer acknowledged before losing
        // its provider/terminal outcome; no recovery helper is invoked by tests.
        let tx = previous.db.begin().await?;
        let mut original = Vec::new();
        for identity in &identities {
            let value = Intent::new(identity, origin.clone())?;
            intent::insert(&tx, &value).await?;
            original.push(value);
        }
        tx.commit().await?;
        close(previous).await?;
        let mut expected = None;
        let mut expected_inventory = None;
        for restart in 0..2 {
            let service = open(&path, &queries, key).await?;
            let outcome: Result<()> = async {
                let statements = queries.require_bounded()?;
                queries.require_indexed_retained_reads(&service.db).await?;
                ensure!(service.current().is_ok() && service.admission.receipts_ready());
                let mut actual = Vec::new();
                for identity in &identities {
                    actual.push(recovered(&service, &tenant, &identity.execution).await?);
                }
                if let Some(expected) = &expected {
                    ensure!(
                        expected == &actual,
                        "restart rewrote unknown receipts/evidence"
                    );
                }
                expected = Some(actual);
                let pending = service
                    .db
                    .query_one(sql(
                        &service.db,
                        "SELECT COUNT(*) AS n FROM tenancy_dispatch_intents",
                    ))
                    .await?
                    .context("pending count")?;
                ensure!(
                    pending.try_get::<i64>("", "n")? == 0,
                    "unfinished intents survived ready"
                );
                let inventory =
                    receipts::inventory(&service.db, &identities[0].installation).await?;
                let persisted = crate::db::migration::capacity::load(&service.db).await?;
                ensure!(
                    persisted.live_bytes == inventory.live_bytes
                        && persisted.history_bytes == inventory.history_bytes
                        && persisted.history_rows == inventory.history_rows
                        && persisted.oauth_bytes == inventory.oauth_bytes
                        && persisted.technical_bytes == inventory.technical_bytes
                        && persisted.unknown_count == inventory.unknown_count,
                    "durable capacity accounting differs from recovered canonical receipts"
                );
                let totals = (
                    inventory.rows,
                    inventory.bytes,
                    inventory.live_bytes,
                    inventory.unknown_count,
                );
                ensure!(inventory.rows == u64::try_from(rows)? + 2 && inventory.unknown_count == 2);
                if let Some(expected) = expected_inventory {
                    ensure!(expected == totals, "restart repeated receipt accounting");
                }
                expected_inventory = Some(totals);
                if restart == 1 {
                    queries.require_no_history_writes()?;
                    ensure!(
                        !statements
                            .iter()
                            .any(|s| s.starts_with("insert into tenancy_execution_receipts")
                                || s.starts_with("insert into tenancy_usage_evidence")),
                        "second startup repeated terminal persistence"
                    );
                }
                for value in &original {
                    let tx = service.db.begin().await?;
                    let denied = intent::insert(&tx, value).await;
                    tx.rollback().await?;
                    ensure!(denied.is_err(), "retired intent resurrected execution");
                }
                ensure!(
                    provider_calls.load(Ordering::SeqCst) == 0,
                    "startup replayed a provider dispatch"
                );
                Ok(())
            }
            .await;
            let stopped = close(service).await;
            outcome.and(stopped)?;
        }
    }
    Ok(())
}

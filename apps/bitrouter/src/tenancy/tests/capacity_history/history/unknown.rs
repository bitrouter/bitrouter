use super::*;
use crate::tenancy::oauth::{crypto::Crypto, storage as oauth};

pub(super) struct Retained {
    pub receipts: Vec<Candidate>,
    pub inventory: obligations::Inventory,
    records: Vec<obligations::Record>,
    pending: oauth::Row,
}

fn crypto(h: &Harness) -> Result<Crypto> {
    Ok(Crypto::new(
        h.fixture.installation.clone(),
        crate::runtime_crypto::DeploymentKey::from_bytes(KEY)?,
    ))
}

pub(super) async fn seed(h: &Harness, reference: &ResourceRef) -> Result<Retained> {
    let execution: ExecutionId = "unknown-pending".try_into()?;
    let (template_crypto, template) = oauth::pending::tests::fixture(
        &h.fixture.installation,
        &h.fixture.tenant,
        &execution,
        "history-pending-slot",
        "2026-10-25T00:00:00Z",
    )?;
    let token = template.token(&template_crypto)?;
    let mut metadata = template.metadata.clone();
    metadata.seed = crypto(h)?.seed(&metadata.reference, &metadata.profile, &token)?;
    if let oauth::State::RefreshStarted(work) = &mut metadata.state {
        work.principal = h.fixture.principal.clone();
    }
    // A separate real OAuth resource keeps the uncertain refresh independent of
    // the healthy model credential. Import authenticates the immutable seed too.
    h.fixture
        .service
        .resources
        .provision_bytes(
            &h.fixture.db,
            &serde_json::to_vec(&serde_json::json!({
                "format":1,"installation_id":h.fixture.installation,"resources":[{
                    "reference":metadata.reference,"definition":{"kind":"credential",
                        "credential":{"type":"oauth","data":token},"oauth":metadata.profile}
                }]
            }))?,
        )
        .await?;
    // Reseal with this fixture's actual wrapping key and complete new binding.
    let pending = oauth::Row::new(&crypto(h)?, metadata, &token)?;
    let mut receipts = Vec::new();
    for index in 0..3 {
        let mut receipt = candidate(h, reference, &format!("unknown-{index}"))?.receipt;
        receipt.attempts.clear();
        receipt.usage = Usage {
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            provenance: Provenance::Unknown,
            complete: false,
        };
        receipt.state = ReceiptState::Unknown;
        receipt.outcome = Outcome::Unknown;
        if index == 1 {
            receipt.state = ReceiptState::Incomplete;
            receipt.outcome = Outcome::Failed;
            receipt.attempts.push(Attempt {
                attempt_id: "uncertain-attempt".try_into()?,
                target: None,
                outcome: Outcome::Unknown,
                usage: receipt.usage.clone(),
                native_usage: Vec::new(),
            });
        }
        let count = receipt.attempts.len();
        let mut value = Candidate::new(
            receipt,
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: vec![None; count],
                auxiliary_digest: None,
            },
        )?;
        if index == 2 {
            value = value.with_auxiliary(vec![auxiliary::Fact {
                credential: pending.metadata.reference.clone(),
                slot: pending.metadata.slot.clone(),
                generation: 0,
                work: uuid::Uuid::new_v4().to_string(),
                request_digest: Some(hex::encode(Sha256::digest(b"synthetic refresh request"))),
                http_status: None,
                outcome: auxiliary::Outcome::Unknown,
            }])?;
        }
        receipts.push(value);
    }
    let tx = h.fixture.db.begin().await?;
    for value in &receipts {
        insert(&tx, value).await?;
    }
    let ready = oauth::load(&tx, &h.fixture.installation, &pending.metadata.reference)
        .await?
        .context("imported OAuth seed")?;
    ensure!(matches!(ready.metadata.state, oauth::State::Ready));
    oauth::replace(&tx, &ready, &pending).await?;
    drop(
        obligations::reconcile(
            &tx,
            &h.fixture.installation,
            &obligations::Key {
                tenant: h.fixture.tenant.clone(),
                execution,
            },
            &|_, _| Ok(()),
        )
        .await?,
    );
    let mut records = obligations::page(&tx, &h.fixture.installation, None, &|_, _| Ok(())).await?;
    ensure!(records.len() == 4);
    let first = records.first_mut().context("unknown record")?;
    obligations::mark_escalated(&tx, &h.fixture.installation, first, &|_, _| Ok(())).await?;
    first.escalated = true;
    let inventory = obligations::validate(&tx, &h.fixture.installation, &|_, _| Ok(())).await?;
    ensure!(inventory.unknown_count == 4);
    ensure!(records.iter().any(|record| record.source_mask == 3));
    tx.commit().await?;
    Ok(Retained {
        receipts,
        inventory,
        records,
        pending,
    })
}

impl Retained {
    pub async fn verify(&self, db: &impl ConnectionTrait, h: &Harness) -> Result<()> {
        ensure!(
            obligations::validate(db, &h.fixture.installation, &|_, _| Ok(())).await?
                == self.inventory
        );
        for expected in &self.receipts {
            let value = receipts::lookup_candidate(
                db,
                &h.fixture.installation,
                &h.fixture.tenant,
                &expected.receipt.execution_id,
            )
            .await?
            .context("unknown receipt lost")?;
            ensure!(
                value.digest == expected.digest
                    && value.bytes() == expected.bytes()
                    && value.auxiliary_bytes() == expected.auxiliary_bytes()
                    && serde_json::to_vec(&value.receipt)?
                        == serde_json::to_vec(&expected.receipt)?
            );
            let row = db.query_one(Statement::from_sql_and_values(sea_orm::DatabaseBackend::Sqlite,
                "SELECT storage_kind FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id=?",
                [h.fixture.installation.as_str().into(), h.fixture.tenant.as_str().into(), expected.receipt.execution_id.as_str().into()]))
                .await?.context("unknown location")?;
            ensure!(row.try_get::<i64>("", "storage_kind")? == 0);
        }
        for expected in &self.records {
            let value =
                obligations::lookup(db, &h.fixture.installation, &expected.key, &|_, _| Ok(()))
                    .await?
                    .context("unknown projection lost")?;
            ensure!(
                value.source_mask == expected.source_mask
                    && value.first == expected.first
                    && value.due == expected.due
            );
            ensure!(
                !expected.escalated || value.escalated,
                "escalation was cleared"
            );
        }
        let saved = oauth::load(
            db,
            &h.fixture.installation,
            &self.pending.metadata.reference,
        )
        .await?
        .context("pending source lost")?;
        ensure!(
            serde_json::to_vec(&saved.metadata)? == serde_json::to_vec(&self.pending.metadata)?
        );
        ensure!(
            serde_json::to_vec(&saved.token(&crypto(h)?)?)?
                == serde_json::to_vec(&self.pending.token(&crypto(h)?)?)?
        );
        Ok(())
    }
}

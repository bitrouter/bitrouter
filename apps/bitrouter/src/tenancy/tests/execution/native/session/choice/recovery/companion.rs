//! Companion campaigns use the actual connector, facade, provider and H owner.
use super::*;
use std::collections::BTreeSet;
mod authority;
mod capacity;
mod capture;
mod custody;
#[cfg(feature = "otel-http")]
mod observation;
mod profiles;
mod scope;
mod startup;

struct Stopped {
    generation: Provider,
    choice: wire::Endpoint,
    spec: Spec,
    bound: Bound,
}
impl Stopped {
    async fn produce(root: &Path, mode: &str) -> Result<Self> {
        Self::produce_observed(
            root,
            mode,
            crate::telemetry::receipt_latency::Sink::default(),
        )
        .await
    }
    async fn produce_observed(
        root: &Path,
        mode: &str,
        observer: crate::telemetry::receipt_latency::Sink,
    ) -> Result<Self> {
        let generation = Provider::start(false).await?;
        let mut choice = wire::Endpoint::start(true, false).await?;
        std::fs::create_dir(root.join("workspace"))?;
        let initial = fixture::sqlite(&generation, &choice, root, 4).await?;
        let spec = Spec::capture(&initial, &generation)?;
        private_json(&root.join("spec.json"), &spec)?;
        initial.fixture.service.shutdown().await?;
        initial.fixture.db.clone().close().await?;
        drop(initial);
        wait_file(root).await?;
        let mut producer = Child::start("native_stopped_owner_producer", root, mode)?;
        let observed = async {
            producer.line("native-recovery-intent").await?;
            let count = if mode == "shared-parent" { 2 } else { 1 };
            for _ in 0..count {
                let sent = choice.receive().await?;
                ensure!(sent.path == "/v1/decisions" && sent.body["model"] == "judge-r1");
                ensure!(
                    sent.headers
                        .get("authorization")
                        .is_some_and(|v| v == "Bearer owned-choice-secret")
                );
            }
            ensure!(choice.calls() == count && generation.calls() == 0);
            locked(root)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let reaped = producer.reap(true).await;
        if let Err(error) = observed.and(reaped) {
            let choice_closed = choice.finish().await;
            let generation_closed = generation.finish().await;
            choice_closed?;
            generation_closed?;
            return Err(error);
        }
        let connected = spec.connect_observed(root, observer).await;
        match connected {
            Ok(bound) => Ok(Self {
                generation,
                choice,
                spec,
                bound,
            }),
            Err(error) => {
                let choice_closed = choice.finish().await;
                let generation_closed = generation.finish().await;
                choice_closed?;
                generation_closed?;
                Err(error)
            }
        }
    }
    async fn close(mut self, result: Result<()>) -> Result<()> {
        self.bound.fixture.service.cleanup.release_gate();
        self.choice.release();
        let choice = self.choice.finish().await;
        let finish = finish(&self.bound.fixture, self.generation, result).await;
        finish?;
        // The same Service has now drained H and its maintenance producer.
        self.bound.fixture.service.cleanup.ensure_clean()?;
        choice
    }
}
fn locked(root: &Path) -> Result<()> {
    let error = crate::tenancy::ownership::acquire_path(&root.join("owned.db"), false)
        .err()
        .context("real owner released its canonical file")?;
    ensure!(matches!(
        error.downcast_ref::<std::fs::TryLockError>(),
        Some(std::fs::TryLockError::WouldBlock)
    ));
    Ok(())
}
async fn wait_file(root: &Path) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::tenancy::ownership::acquire_path(&root.join("owned.db"), false) {
                Ok(owner) => {
                    drop(owner);
                    return Ok::<_, anyhow::Error>(());
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await?
}
async fn payload(db: &sea_orm::DatabaseConnection) -> Result<String> {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload,payload_bytes FROM bro_managed_sessions ORDER BY id",
        ))
        .await?;
    ensure!(
        rows.len() == 1,
        "fixture must retain exactly one native row"
    );
    let body: String = rows[0].try_get("", "payload")?;
    ensure!(rows[0].try_get::<i64>("", "payload_bytes")? == i64::try_from(body.len())?);
    Ok(body)
}
type Terminals = Vec<(String, String, String)>;
async fn terminals(db: &sea_orm::DatabaseConnection) -> Result<Terminals> {
    db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT execution_id,receipt_json,disposition_json FROM tenancy_execution_receipts ORDER BY execution_id"))
        .await?.iter().map(|row| Ok((row.try_get("", "execution_id")?, row.try_get("", "receipt_json")?, row.try_get("", "disposition_json")?))).collect()
}
async fn no_write(
    stopped: &Stopped,
    root: &Path,
    expected: &str,
    before: &Terminals,
) -> Result<()> {
    let opened = fixture::open(
        &stopped.bound,
        &stopped.generation,
        &root.join("workspace"),
        Arc::new(HostSpies::default()),
    )
    .await;
    if let Ok(session) = opened {
        session.close().await?;
        anyhow::bail!("unsupported predecessor published a native handle");
    }
    ensure!(payload(&stopped.bound.fixture.db).await? == expected);
    ensure!(terminals(&stopped.bound.fixture.db).await? == *before);
    ensure!(stopped.choice.calls() == 1 && stopped.generation.calls() == 0);
    tokio::time::timeout(TIMEOUT, async {
        while stopped.bound.fixture.service.host_slots_for_test() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    ensure!(
        stopped
            .bound
            .fixture
            .service
            .admission
            .receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .0
            == 0
    );
    Ok(())
}
async fn replace_negative(db: &sea_orm::DatabaseConnection, value: &Value) -> Result<String> {
    let body = serde_json::to_string(value)?;
    let update = db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE bro_managed_sessions SET payload=?,payload_bytes=?",
            [body.clone().into(), i64::try_from(body.len())?.into()],
        ))
        .await?;
    ensure!(update.rows_affected() == 1);
    Ok(body)
}

#[tokio::test]
#[ignore = "isolated actual native recovery companion; select this parent, no nested Cargo"]
async fn native_recovery_shared_parent_two_real_choices_are_deduplicated() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut controller = Child::start("companion::controller", root.path(), "shared-parent")?;
    let observed = controller.line("native-companion-checked").await;
    let reaped = controller.reap(observed.is_err()).await;
    observed?;
    reaped
}

async fn shared_parent(root: &Path) -> Result<()> {
    let stopped = Stopped::produce(root, "shared-parent").await?;
    let result = async {
        let before = fixture::journal(&stopped.bound).await?;
        let links = before["owned_choices"]
            .as_object()
            .context("two real intents")?;
        ensure!(links.len() == 2);
        let mut parents = BTreeSet::new();
        let mut children = BTreeSet::new();
        let mut decisions = BTreeSet::new();
        for (id, link) in links {
            parents.insert(
                link["parent_execution"]
                    .as_str()
                    .context("parent")?
                    .to_owned(),
            );
            ensure!(
                children.insert(
                    link["child_execution"]
                        .as_str()
                        .context("child")?
                        .to_owned()
                )
            );
            decisions.insert(id.clone());
            ensure!(link["version"] == 2 && link["outcome_digest"].is_null());
        }
        ensure!(parents.len() == 1 && parents.is_disjoint(&children));
        let expected: BTreeSet<_> = parents.union(&children).cloned().collect();
        ensure!(expected.len() == 3 && terminals(&stopped.bound.fixture.db).await?.is_empty());
        let session = fixture::open(
            &stopped.bound,
            &stopped.generation,
            &root.join("workspace"),
            Arc::new(HostSpies::default()),
        )
        .await?;
        let state = session.core().snapshot().await;
        for id in &decisions {
            let failure = state
                .context_store
                .decisions
                .get(id)
                .context("restored real decision")?
                .outcome
                .as_ref()
                .context("retirement result")?
                .as_ref()
                .err()
                .context("invented Choice answer")?;
            ensure!(
                failure.kind
                    == crate::gateway::model::decision::types::DecisionFailure::Interrupted
                    && failure.may_have_run
                    && failure.usage.is_none()
            );
        }
        ensure!(
            state
                .run
                .as_ref()
                .is_some_and(|r| r.status == RunStatus::Cancelled)
        );
        let actual = terminals(&stopped.bound.fixture.db).await?;
        ensure!(
            actual.len() == 3
                && actual
                    .iter()
                    .map(|row| row.0.clone())
                    .collect::<BTreeSet<_>>()
                    == expected
        );
        for (_, body, _) in &actual {
            let receipt: bitrouter_ai::tenancy::execution::ExecutionReceipt =
                serde_json::from_str(body)?;
            ensure!(
                receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                    && receipt.attempts.is_empty()
                    && receipt.usage.provenance == Provenance::Unknown
                    && !receipt.usage.complete
                    && receipt.execution_revision == stopped.bound.revision
                    && receipt.principal_id == stopped.bound.fixture.principal
            );
        }
        let saved = fixture::journal(&stopped.bound).await?;
        let mut parent_digests = BTreeSet::new();
        for id in decisions {
            let digests = saved["owned_choices"][&id]["terminal_digests"]
                .as_array()
                .context("terminal pair")?;
            ensure!(digests.len() == 2);
            parent_digests.insert(digests[0].as_str().context("parent terminal")?.to_owned());
        }
        ensure!(
            parent_digests.len() == 1
                && stopped.choice.calls() == 2
                && stopped.generation.calls() == 0
        );
        session.close().await?;
        tokio::time::timeout(
            TIMEOUT,
            stopped.bound.fixture.service.admission.wait_drained(),
        )
        .await?;
        ensure!(
            terminals(&stopped.bound.fixture.db).await? == actual
                && stopped.bound.fixture.service.host_slots_for_test() == 1
        );
        Ok(())
    }
    .await;
    stopped.close(result).await
}

#[tokio::test]
#[ignore = "internal companion controller; select public parent only"]
async fn controller() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    let mode = std::env::var(MODE)?;
    let root = Path::new(&root);
    match mode.as_str() {
        "shared-parent" => shared_parent(root).await?,
        "profile-capture" => capture::case(root).await?,
        mode if mode.starts_with("custody-") => custody::case(root, mode).await?,
        mode if mode.starts_with("scope-") || mode.starts_with("origin-") => {
            scope::case(root, mode).await?
        }
        mode if mode.starts_with("late-") => authority::case(root, mode).await?,
        mode if mode.starts_with("capacity-unknown-") => capacity::unknown(root, mode).await?,
        mode if mode.starts_with("capacity-bytes-") => capacity::bytes(root, mode).await?,
        "profile-generation" | "profile-count" | "profile-oauth" | "compat-v1-released" => {
            profiles::case(root, &mode).await?
        }
        "startup-cancel-pool" => {
            startup::cancelled_startup_retains_pool_and_transaction(root).await?
        }
        _ => anyhow::bail!("unknown companion mode"),
    }
    signal("native-companion-checked")
}
async fn campaign(modes: &[&str]) -> Result<()> {
    for mode in modes {
        let root = tempfile::tempdir()?;
        let mut child = Child::start("companion::controller", root.path(), mode)?;
        let deadline = if mode.starts_with("capacity-") {
            Duration::from_secs(180)
        } else {
            Duration::from_secs(40)
        };
        let observed = child
            .line_with_deadline("native-companion-checked", deadline)
            .await;
        let reaped = child.reap(observed.is_err()).await;
        observed?;
        reaped?;
    }
    Ok(())
}

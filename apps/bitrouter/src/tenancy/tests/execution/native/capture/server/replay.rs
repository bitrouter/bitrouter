use super::*;
use crate::{
    harness::{
        acp::capture::CapturePort,
        trajectory::owned::{Append, entities::connections},
    },
    tenancy::{
        capture::{
            Binding,
            queue::{Outcome, Reply, Request},
        },
        transaction::Disposition,
    },
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

async fn recorded(consumers: &mut Consumers, ready: &Ready) -> Result<(Arc<Binding>, Arc<Port>)> {
    // Actual issued headers -> Service host authority -> retained route and
    // current grant checks. No Scope::for_test, synthetic job proof or authority.
    let prepared = ready
        .fixture
        .service
        .prepare_capture(
            &ready.headers()?,
            "capture-fixture",
            "http://127.0.0.1:1",
            CancellationToken::new(),
        )
        .await
        .map_err(failure)?;
    let binding = prepared.binding;
    let port = Port::new(binding.clone());
    consumers.ports.push(port.clone());
    port.create().await?;
    for event in observations::sequence(ready.route.resource_id.as_str()) {
        port.record(event)
            .await
            .map_err(|error| anyhow::anyhow!("capture callback: {error}"))?;
    }
    let summary = port
        .checkpoint("actor-native")
        .await?
        .context("actual actor stopped prefix")?;
    ensure!(summary.observations == 7 && summary.watermark == 6);
    ensure!(ready.fixture.service.current().is_ok());
    Ok((binding, port))
}

async fn enqueue(binding: &Arc<Binding>, request: Request) -> Result<Outcome> {
    let receive = binding.enqueue(request).await.map_err(failure)?;
    Ok(tokio::time::timeout(TIMEOUT, receive)
        .await
        .context("actual capture actor reply deadline")??)
}

pub(super) async fn exact_then_conflicting(consumers: &mut Consumers, ready: &Ready) -> Result<()> {
    let (binding, port) = recorded(consumers, ready).await?;
    let graph = observations::Graph::read(&ready.fixture.db).await?;
    let row = graph.last(binding.association().connection())?;
    let append = Arc::new(Append::new(
        row.sequence,
        row.captured_at.clone(),
        serde_json::from_str(&row.event_json)?,
    )?);
    let result = enqueue(
        &binding,
        Request::Append {
            value: append,
            cleanup: false,
        },
    )
    .await?;
    ensure!(
        result.disposition == Disposition::RolledBack,
        "exact replay did not use read-only rollback"
    );
    match result.result.map_err(failure)? {
        Reply::Appended(value) => ensure!(value.duplicate && value.head == row.sequence),
        _ => anyhow::bail!("exact replay was not consumed as an append"),
    }
    ensure!(
        observations::Graph::read(&ready.fixture.db).await? == graph,
        "exact replay repaired or changed stored bytes"
    );
    ensure!(ready.fixture.service.current().is_ok());
    port.finish().await?;
    let graph = observations::Graph::read(&ready.fixture.db).await?;
    let row = graph.last(binding.association().connection())?;
    let event: crate::storage::capture::CaptureEvent = serde_json::from_str(&row.event_json)?;
    ensure!(event.kind == crate::storage::capture::CaptureKind::Disconnected);
    let changed = (chrono::DateTime::parse_from_rfc3339(&row.captured_at)?
        + chrono::Duration::seconds(1))
    .to_rfc3339();
    ensure!(changed != row.captured_at);
    let result = enqueue(
        &binding,
        Request::Append {
            value: Arc::new(Append::new(row.sequence, changed, event)?),
            cleanup: false,
        },
    )
    .await?;
    ensure!(
        result.disposition == Disposition::RolledBack,
        "conflicting immutable append did not roll back"
    );
    let error = result
        .result
        .err()
        .context("different retained timestamp was accepted")?;
    ensure!(error.0.code == ErrorCode::StorageUnavailable);
    ensure!(
        observations::Graph::read(&ready.fixture.db).await? == graph,
        "conflicting replay changed graph"
    );
    ensure!(
        ready.fixture.service.current().is_err(),
        "integrity rejection did not fail closed"
    );
    ready.fixture.service.cleanup.ensure_clean()?;
    Ok(())
}

pub(super) async fn foreign_namespace(
    consumers: &mut Consumers,
    ready: &Ready,
    foreign_tenant: &str,
) -> Result<()> {
    let (binding, port) = recorded(consumers, ready).await?;
    port.finish().await?;
    let connection = binding.association().connection();
    let original = observations::Graph::read(&ready.fixture.db).await?;
    let row = original
        .connections
        .iter()
        .find(|row| row.connection_id == connection)
        .context("owned connection")?;
    ensure!(
        row.tenant_id.as_deref() == Some(ready.fixture.tenant.as_str())
            && foreign_tenant != ready.fixture.tenant.as_str()
    );
    // Negative DB fixture only: the live checked binding is never rewritten.
    // Use an actually created tenant, retaining all other immutable row bytes.
    let changed = connections::Entity::update_many()
        .filter(connections::Column::ConnectionId.eq(connection))
        .col_expr(connections::Column::TenantId, Expr::value(foreign_tenant))
        .exec(&ready.fixture.db)
        .await?;
    ensure!(changed.rows_affected == 1);
    let before = observations::Graph::read(&ready.fixture.db).await?;
    let outcome = enqueue(&binding, Request::Freeze("actor-native".into())).await?;
    ensure!(
        outcome.disposition == Disposition::RolledBack,
        "foreign namespace read was not consumed and rolled back by actor"
    );
    let error = outcome
        .result
        .err()
        .context("foreign capture namespace was adopted")?;
    ensure!(error.0.code == ErrorCode::StorageUnavailable);
    ensure!(
        observations::Graph::read(&ready.fixture.db).await? == before,
        "foreign namespace was repaired or overwritten"
    );
    ensure!(ready.fixture.service.current().is_err());
    ready.fixture.service.cleanup.ensure_clean()?;
    Ok(())
}

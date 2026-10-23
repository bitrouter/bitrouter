//! Unsupported managed intent must never bind or reuse an ordinary execution.

use super::*;
use bitrouter_ai::core::protocol::{ErrorCode, ErrorEnvelope};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

pub(super) async fn exercise(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    tenant: &str,
    serving: &Arc<Serving>,
) -> Result<()> {
    let body = json!({"model":"ok","input":"hello","multi_agent":{"enabled":false}});
    let key = uuid::Uuid::new_v4().to_string();
    let correlation = "c".repeat(256);
    let ordinary = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/responses",
            body.clone(),
            Some(&key),
            &correlation,
        )?)
        .await?;
    ensure!(ordinary.status() == StatusCode::OK);
    ensure!(ordinary.headers()["x-bitrouter-request-id"] == correlation);
    let execution = ordinary.headers()["x-bitrouter-execution-id"]
        .to_str()?
        .to_owned();
    let output: Value = serde_json::from_slice(&to_bytes(ordinary.into_body(), 65536).await?)?;
    let continuation = output["id"].as_str().context("missing Responses id")?;
    ensure!(
        crate::gateway::model::protocol::responses::decode_gateway_continuation_id(continuation)?
            .as_deref()
            == Some(execution.as_str()),
        "continuation must identify the service execution, not caller correlation"
    );
    retire(serving).await?;

    let counts = durable_counts(store).await?;
    let rate = store
        .get::<Value>(Kind::Tenant, tenant)
        .await?
        .context("missing profile tenant")?;
    let calls = upstream.calls.load(Ordering::SeqCst);
    for beta in ["orchestrator_core=v1", "unknown", ""] {
        for key in [Some(key.as_str()), None] {
            let mut request = request(
                fixture,
                "/v1/responses",
                body.clone(),
                key,
                "unsupported-header",
            )?;
            request
                .headers_mut()
                .insert("bitrouter-beta", http::HeaderValue::from_str(beta)?);
            unsupported(serving.router().oneshot(request).await?).await?;
            retire(serving).await?;
        }
    }
    for (field, value) in [
        ("bitrouter", Value::Null),
        ("bitrouter", json!({})),
        ("multi_agent", json!({"enabled":true})),
    ] {
        let mut managed = body.clone();
        managed[field] = value;
        let response = serving
            .router()
            .oneshot(request(
                fixture,
                "/v1/responses",
                managed,
                Some(&uuid::Uuid::new_v4().to_string()),
                "unsupported-body",
            )?)
            .await?;
        unsupported(response).await?;
        retire(serving).await?;
    }

    // A managed header is sufficient to reject before polling any body, while
    // the capacity test separately proves reserve() still runs first.
    let polled = Arc::new(AtomicUsize::new(0));
    let mut pending = request(fixture, "/v1/responses", body, None, "unread")?;
    pending
        .headers_mut()
        .insert("bitrouter-beta", http::HeaderValue::from_static("unknown"));
    *pending.body_mut() = Body::new(ObservedBody(polled.clone()));
    unsupported(
        tokio::time::timeout(Duration::from_secs(1), serving.router().oneshot(pending)).await??,
    )
    .await?;
    ensure!(polled.load(Ordering::SeqCst) == 0);
    retire(serving).await?;
    ensure!(upstream.calls.load(Ordering::SeqCst) == calls);
    ensure!(durable_counts(store).await? == counts);
    let after = store
        .get::<Value>(Kind::Tenant, tenant)
        .await?
        .context("missing profile tenant")?;
    ensure!(after.revision()? == rate.revision()? && after.value == rate.value);
    Ok(())
}

async fn unsupported(response: axum::response::Response) -> Result<()> {
    ensure!(
        response.status() == StatusCode::BAD_REQUEST,
        "managed Responses returned {} instead of unsupported profile",
        response.status()
    );
    ensure!(!response.headers().contains_key("x-bitrouter-execution-id"));
    let error: ErrorEnvelope =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
    ensure!(error.error.code == ErrorCode::UnsupportedCapability);
    Ok(())
}

async fn durable_counts(store: &Store) -> Result<Vec<(String, i64)>> {
    // The fixture namespace is isolated and its other scenarios run serially.
    // Count every record kind to detect hidden execution/idempotency insertion.
    let rows = store
        .database()
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT kind, COUNT(*) AS count FROM gateway_shared_records \
             WHERE namespace = $1 GROUP BY kind ORDER BY kind",
            [store.namespace().into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| Ok((row.try_get("", "kind")?, row.try_get("", "count")?)))
        .collect()
}

//! Metering integration tests against an in-memory SQLite database.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use crate::gateway::caller::CallerContext;
use crate::gateway::error::Result;
use crate::gateway::pipeline::settlement::SettlementContext;
use crate::gateway::pipeline::settlement::SettlementRecorder;

use super::{
    MeteringRecorder, MeteringSettlementEvent, MeteringStore, ModelPricing, PricingTable,
    TimeWindow,
};
use crate::db;
use crate::session_identity::{
    IdentityEvidence, RequestOrigin, RouteLeaseObservation, SessionIdentityObserved,
};

async fn pool() -> DatabaseConnection {
    let db = db::connect("sqlite::memory:").await.unwrap();
    db::run_migrations(&db).await.unwrap();
    db
}

fn ctx(api_key: &str, prompt: u64, completion: u64) -> SettlementContext {
    let mut context = SettlementContext {
        operation: bitrouter_ai::types::ModelOperation::Generation,
        request_id: format!("r-{api_key}-{prompt}-{completion}"),
        caller: CallerContext::new(api_key, format!("u-{api_key}")),
        target: Some(crate::gateway::pipeline::types::RoutingTarget {
            registry: None,
            model_constraints: Default::default(),
            provider_name: "openai".into(),
            service_id: "gpt-5".into(),
            api_protocol: bitrouter_ai::types::ApiProtocol::ChatCompletions,
            api_base: "https://fixture.invalid/v1".into(),
            api_key: String::new(),
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        }),
        model_id: "gpt-5".into(),
        reasoning_effort: None,
        provider_id: "openai".into(),
        account_label: None,
        prompt_tokens: prompt,
        completion_tokens: completion,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        usage_origin: bitrouter_ai::types::UsageOrigin::ProviderReported,
        raw_usage: None,
        web_search_count: 0,
        streamed: false,
        request_duration_ms: 100,
        upstream_duration_ms: Some(80),
        finish_reason: None,
        error: None,
        events: crate::gateway::observability::event::EventBus::new(),
    };
    freeze_tariff(&mut context, &pricing());
    context
}

fn freeze_tariff(context: &mut SettlementContext, pricing: &PricingTable) {
    if let Some(target) = &context.target {
        let mut anchored = pricing.clone();
        anchored.configure_endpoint(&target.provider_name, None, &target.api_base);
        context.emit(anchored.snapshot(target));
    }
}

fn pricing() -> Arc<PricingTable> {
    let mut t = PricingTable::new();
    // 2 µ$/prompt token, 10 µ$/completion token
    t.insert("openai", "gpt-5", ModelPricing::new(2.0, 10.0));
    Arc::new(t)
}

#[tokio::test]
async fn recorder_writes_estimated_charge_from_pricing() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    // 10 prompt × 2 + 5 completion × 10 = 70 µ$
    recorder.record(&mut ctx("k1", 10, 5)).await?;
    let spend = store
        .get_enforceable_spend("k1", TimeWindow::ThisMonth)
        .await?;
    assert_eq!(spend, Some(70));
    Ok(())
}

#[tokio::test]
async fn recorder_correlates_normalized_acp_identity_without_prompt_content() -> anyhow::Result<()>
{
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store);
    let mut attributed = ctx("acp", 10, 5);
    attributed.request_id = "router-acp-request".to_string();
    attributed.emit(SessionIdentityObserved {
        router_request_id: attributed.request_id.clone(),
        origin: RequestOrigin::AcpHarnessRequest,
        api_principal_id: "key-acp".to_string(),
        harness: Some("codex".to_string()),
        claimed_controller_instance_id: Some("brc_metering".to_string()),
        acp_session_id: Some("acp-session".to_string()),
        native_root_session_id: Some("codex-root".to_string()),
        native_agent_thread_id: Some("codex-thread".to_string()),
        native_parent_agent_thread_id: Some("codex-parent".to_string()),
        native_turn_id: Some("codex-turn".to_string()),
        legacy_workflow_session_id: None,
        api_continuation_id: None,
        evidence: vec![IdentityEvidence {
            transport: "header".to_string(),
            field: "thread-id".to_string(),
            source: "codex".to_string(),
            used_for_route_match: true,
            value_representation: "raw".to_string(),
            value: Some("codex-thread".to_string()),
        }],
        conflicts: Vec::new(),
        attributed: true,
        route_scope: "session".to_string(),
        route_lease_id: Some("brlease_metering".to_string()),
        route_lease_outcome: Some(RouteLeaseObservation {
            lease_id: "brlease_metering".to_string(),
            matched_session_id: "codex-thread".to_string(),
            route: "openai:gpt-5".to_string(),
            applied: true,
            reason: "applied".to_string(),
        }),
    });

    recorder.record(&mut attributed).await?;
    let mut ordinary = ctx("ordinary", 1, 1);
    ordinary.request_id = "ordinary-request".to_string();
    recorder.record(&mut ordinary).await?;

    let row = pool
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT agent_harness, controller_instance_id, acp_session_id, native_root_session_id, native_agent_thread_id, native_parent_agent_thread_id, native_turn_id, route_lease_id, session_identity_json FROM requests WHERE request_id = 'router-acp-request'".to_owned(),
        ))
        .await?
        .expect("attributed request row");
    assert_eq!(row.try_get::<String>("", "agent_harness")?, "codex");
    assert_eq!(
        row.try_get::<String>("", "controller_instance_id")?,
        "brc_metering"
    );
    assert_eq!(row.try_get::<String>("", "acp_session_id")?, "acp-session");
    assert_eq!(
        row.try_get::<String>("", "native_agent_thread_id")?,
        "codex-thread"
    );
    assert_eq!(row.try_get::<String>("", "native_turn_id")?, "codex-turn");
    assert_eq!(
        row.try_get::<String>("", "route_lease_id")?,
        "brlease_metering"
    );
    let identity_json = row.try_get::<String>("", "session_identity_json")?;
    assert!(identity_json.contains("thread-id"));
    assert!(identity_json.contains("key-acp"));
    for forbidden in ["prompt", "messages", "authorization", "cookie"] {
        assert!(!identity_json.to_ascii_lowercase().contains(forbidden));
    }

    let ordinary = pool
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT controller_instance_id, session_identity_json FROM requests WHERE request_id = 'ordinary-request'".to_owned(),
        ))
        .await?
        .expect("ordinary request row");
    assert_eq!(
        ordinary.try_get::<Option<String>>("", "controller_instance_id")?,
        None
    );
    assert_eq!(
        ordinary.try_get::<Option<String>>("", "session_identity_json")?,
        None
    );

    let span = attributed
        .get_event::<crate::gateway::observability::span_attributes::SpanAttributes>()
        .expect("session span attributes");
    assert_eq!(
        span.0
            .get("bitrouter.acp.api_principal_id")
            .and_then(serde_json::Value::as_str),
        Some("key-acp")
    );
    assert_eq!(
        span.0
            .get("bitrouter.acp.controller_instance_id")
            .and_then(serde_json::Value::as_str),
        Some("brc_metering")
    );
    assert_eq!(
        span.0
            .get("bitrouter.agent.thread_id")
            .and_then(serde_json::Value::as_str),
        Some("codex-thread")
    );
    assert_eq!(
        span.0
            .get("bitrouter.acp.route_lease_applied")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        span.0
            .get("bitrouter.acp.route_lease_reason")
            .and_then(serde_json::Value::as_str),
        Some("applied")
    );
    Ok(())
}

#[tokio::test]
async fn recorder_persists_router_identity_and_leaves_direct_requests_unknown() -> anyhow::Result<()>
{
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store);
    let mut routed = ctx("router", 3, 2);
    routed.request_id = "routed-request".to_string();
    routed.emit(bitrouter_ai::routing::RouterRequestIdentity {
        router_id: "coding".to_string(),
        original_selector: "bitrouter/coding".to_string(),
        binding_digest: "router-v1:sha256:abc".to_string(),
    });
    recorder.record(&mut routed).await?;

    let mut direct = ctx("direct", 3, 2);
    direct.request_id = "direct-request".to_string();
    recorder.record(&mut direct).await?;

    let routed = pool
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT router_id, binding_digest, original_selector FROM requests WHERE request_id = 'routed-request'".to_owned(),
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("routed request row is missing"))?;
    assert_eq!(routed.try_get::<String>("", "router_id")?, "coding");
    assert_eq!(
        routed.try_get::<String>("", "binding_digest")?,
        "router-v1:sha256:abc"
    );
    assert_eq!(
        routed.try_get::<String>("", "original_selector")?,
        "bitrouter/coding"
    );

    let direct = pool
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT router_id, binding_digest, original_selector FROM requests WHERE request_id = 'direct-request'".to_owned(),
        ))
        .await?
        .ok_or_else(|| anyhow::anyhow!("direct request row is missing"))?;
    for column in ["router_id", "binding_digest", "original_selector"] {
        assert_eq!(
            direct.try_get::<Option<String>>("", column)?,
            None,
            "{column}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn recorder_emits_authoritative_content_free_settlement_after_persisting() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool);
    let recorder = MeteringRecorder::new(store);
    let mut settlement = ctx("event", 10, 5);

    recorder.record(&mut settlement).await?;

    let event = settlement
        .get_event::<MeteringSettlementEvent>()
        .expect("metering settlement event");
    assert_eq!(event.request_id, "r-event-10-5");
    assert_eq!(event.provider_id, "openai");
    assert_eq!(event.model_id, "gpt-5");
    assert_eq!(event.total_tokens, Some(15));
    assert_eq!(event.cost_micro_usd, Some(70));
    assert_eq!(event.duration_ms, 100);
    assert_eq!(event.error_code, None);
    assert_eq!(event.finish_reason, None);
    Ok(())
}

#[tokio::test]
async fn routing_failure_keeps_unknown_target_usage_and_charge_absent() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool);
    let recorder = MeteringRecorder::new(store.clone());
    let mut settlement = ctx("unknown", 0, 0);
    settlement.target = None;
    settlement.provider_id.clear();
    settlement.model_id.clear();
    settlement.usage_origin = bitrouter_ai::types::UsageOrigin::Unknown;
    settlement.error = Some(crate::gateway::error::BitrouterError::NotFound(
        "no active provider declares the selected model".into(),
    ));

    recorder.record(&mut settlement).await?;

    let event = settlement
        .get_event::<MeteringSettlementEvent>()
        .expect("metering settlement event");
    assert_eq!(event.provider_id, "");
    assert_eq!(event.model_id, "");
    assert_eq!(
        event.usage_origin,
        bitrouter_ai::types::UsageOrigin::Unknown
    );
    assert_eq!(event.prompt_tokens, None);
    assert_eq!(event.completion_tokens, None);
    assert_eq!(event.reasoning_tokens, None);
    assert_eq!(event.cache_read_tokens, None);
    assert_eq!(event.cache_write_tokens, None);
    assert_eq!(event.total_tokens, None);
    assert_eq!(event.cost_micro_usd, None);
    assert_eq!(event.error_code.as_deref(), Some("not_found"));

    let records = stored_rows(&store).await?;
    let record = records.first().expect("one routing failure row");
    assert_eq!(
        record.usage_origin,
        bitrouter_ai::types::UsageOrigin::Unknown.as_str()
    );
    assert_eq!(record.raw_usage_json, None);
    assert_eq!(
        record.charge_status,
        super::pricing::ChargeStatus::Unknown.as_str()
    );
    assert_eq!(evidence(record)?.charge_micro_usd, None);
    let evidence = evidence(record)?;
    assert_eq!(evidence.status, super::pricing::ChargeStatus::Unknown);
    assert_eq!(evidence.charge_micro_usd, None);
    assert_eq!(
        evidence.pricing_source,
        super::pricing::PricingSource::Unknown
    );
    assert_eq!(
        evidence.unknown_reason.as_deref(),
        Some("usage_unavailable")
    );
    assert_eq!(
        store
            .get_enforceable_spend("unknown", TimeWindow::ThisMonth)
            .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn recorder_failure_does_not_emit_settlement_authority() -> anyhow::Result<()> {
    let pool = pool().await;
    pool.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TABLE requests".to_owned(),
    ))
    .await?;
    let store = MeteringStore::new(pool);
    let recorder = MeteringRecorder::new(store);
    let mut settlement = ctx("failed-write", 10, 5);

    assert!(recorder.record(&mut settlement).await.is_err());
    assert!(settlement.get_event::<MeteringSettlementEvent>().is_none());
    Ok(())
}

#[tokio::test]
async fn recorder_persists_cache_aware_charge_evidence() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let mut table = PricingTable::new();
    table.insert(
        "openai",
        "gpt-5",
        ModelPricing::cache_aware(Some(2.0), Some(0.2), Some(2.5), Some(10.0)),
    );
    let recorder = MeteringRecorder::new(store.clone());
    let raw = serde_json::json!({
        "prompt_tokens": 100,
        "completion_tokens": 30,
        "prompt_tokens_details": { "cached_tokens": 40 },
        "cache_write_tokens": 20
    });
    let mut settlement = ctx("cache", 100, 30);
    freeze_tariff(&mut settlement, &table);
    settlement.reasoning_tokens = 10;
    settlement.cache_read_tokens = 40;
    settlement.cache_write_tokens = 20;
    settlement.raw_usage = Some(raw.clone());

    recorder.record(&mut settlement).await?;
    let records = stored_rows(&store).await?;
    let record = records.first().expect("one usage record");

    assert_eq!(record.uncached_input_tokens, 40);
    assert_eq!(record.cache_read_tokens, 40);
    assert_eq!(record.cache_write_tokens, 20);
    assert_eq!(record.output_tokens, 20);
    assert_eq!(record.reasoning_tokens, 10);
    assert_eq!(evidence(record)?.charge_micro_usd, Some(438));
    assert_eq!(
        record.charge_status,
        super::pricing::ChargeStatus::Computed.as_str()
    );
    assert_eq!(
        record.raw_usage_json.as_deref(),
        Some(raw.to_string().as_str())
    );
    let evidence = evidence(record)?;
    assert_eq!(evidence.charge_micro_usd, Some(438));
    assert!(evidence.pricing_version.starts_with("sha256:"));
    Ok(())
}

#[tokio::test]
async fn recorder_marks_charge_unknown_when_pricing_is_missing() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let empty = Arc::new(PricingTable::new());
    let recorder = MeteringRecorder::new(store.clone());
    let mut settlement = ctx("k1", 10, 5);
    freeze_tariff(&mut settlement, &empty);
    recorder.record(&mut settlement).await?;
    let spend = store
        .get_enforceable_spend("k1", TimeWindow::ThisMonth)
        .await?;
    assert_eq!(spend, None);
    // The row was still written — count is 1.
    let count = stored_rows(&store).await?.len();
    assert_eq!(count, 1);
    let records = stored_rows(&store).await?;
    assert_eq!(evidence(&records[0])?.charge_micro_usd, None);
    assert_eq!(
        records[0].charge_status,
        super::pricing::ChargeStatus::Unknown.as_str()
    );
    assert_eq!(
        evidence(&records[0])?.unknown_reason.as_deref(),
        Some("pricing_not_found")
    );
    Ok(())
}

#[tokio::test]
async fn recorder_never_computes_zero_charge_from_unknown_usage() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    let mut settlement = ctx("unknown", 0, 0);
    settlement.usage_origin = bitrouter_ai::types::UsageOrigin::Unknown;

    recorder.record(&mut settlement).await?;
    let records = stored_rows(&store).await?;

    assert_eq!(evidence(&records[0])?.charge_micro_usd, None);
    assert_eq!(
        records[0].charge_status,
        super::pricing::ChargeStatus::Unknown.as_str()
    );
    assert_eq!(
        evidence(&records[0])?.unknown_reason.as_deref(),
        Some("usage_unavailable")
    );
    Ok(())
}

#[tokio::test]
async fn policy_rejection_records_sanitized_zero_usage_evidence() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    let mut settlement = ctx("policy", 0, 0);
    settlement.usage_origin = bitrouter_ai::types::UsageOrigin::Unknown;
    settlement.raw_usage = None;
    settlement.error = Some(
        crate::gateway::error::BitrouterError::UpstreamPolicyViolation {
            message: "provider detail must not persist".to_string(),
        },
    );

    recorder.record(&mut settlement).await?;
    let records = stored_rows(&store).await?;
    let record = records.first().expect("one usage record");

    assert_eq!(
        record.usage_origin,
        bitrouter_ai::types::UsageOrigin::ProviderReported.as_str()
    );
    assert_eq!(
        record
            .raw_usage_json
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|error| crate::gateway::error::BitrouterError::internal(error.to_string()))?,
        Some(serde_json::json!({
            "error": { "code": "upstream_policy_violation" },
            "usage": null
        }))
    );
    assert_eq!(evidence(record)?.charge_micro_usd, Some(0));
    assert_eq!(
        record.charge_status,
        super::pricing::ChargeStatus::Computed.as_str()
    );
    assert_eq!(record.error.as_deref(), Some("upstream_policy_violation"));
    let event = settlement
        .get_event::<MeteringSettlementEvent>()
        .expect("metering settlement event");
    assert_eq!(
        event.usage_origin,
        bitrouter_ai::types::UsageOrigin::Unknown
    );
    assert_eq!(event.total_tokens, None);
    assert_eq!(event.cost_micro_usd, None);
    Ok(())
}

#[tokio::test]
async fn upstream_rate_limit_records_sanitized_zero_usage_evidence() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    let mut settlement = ctx("rate-limited", 0, 0);
    settlement.usage_origin = bitrouter_ai::types::UsageOrigin::Unknown;
    settlement.raw_usage = None;
    settlement.error = Some(crate::gateway::error::BitrouterError::UpstreamRateLimited {
        retry_after: None,
        detail: None,
    });

    recorder.record(&mut settlement).await?;
    let records = stored_rows(&store).await?;
    let Some(record) = records.first() else {
        return Err(crate::gateway::error::BitrouterError::internal(
            "missing rate-limit usage record",
        ));
    };

    assert_eq!(
        record.usage_origin,
        bitrouter_ai::types::UsageOrigin::ProviderReported.as_str()
    );
    assert_eq!(
        record
            .raw_usage_json
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|error| crate::gateway::error::BitrouterError::internal(error.to_string()))?,
        Some(serde_json::json!({
            "error": { "code": "upstream_rate_limited" },
            "usage": null
        }))
    );
    assert_eq!(evidence(record)?.charge_micro_usd, Some(0));
    assert_eq!(
        record.charge_status,
        super::pricing::ChargeStatus::Computed.as_str()
    );
    assert_eq!(record.error.as_deref(), Some("upstream_rate_limited"));
    Ok(())
}

#[tokio::test]
async fn spend_aggregates_across_requests() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    recorder.record(&mut ctx("k1", 10, 5)).await?; // 70
    recorder.record(&mut ctx("k1", 100, 0)).await?; // 200
    recorder.record(&mut ctx("k1", 0, 50)).await?; // 500
    let spend = store
        .get_enforceable_spend("k1", TimeWindow::ThisMonth)
        .await?;
    assert_eq!(spend, Some(70 + 200 + 500));
    Ok(())
}

#[tokio::test]
async fn spend_isolates_by_api_key() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    recorder.record(&mut ctx("k1", 10, 5)).await?;
    recorder.record(&mut ctx("k2", 100, 0)).await?;
    assert_eq!(
        store
            .get_enforceable_spend("k1", TimeWindow::ThisMonth)
            .await?,
        Some(70)
    );
    assert_eq!(
        store
            .get_enforceable_spend("k2", TimeWindow::ThisMonth)
            .await?,
        Some(200)
    );
    Ok(())
}

/// Old metering stores are refused without renaming or rewriting money fields.
#[tokio::test]
async fn old_metering_store_is_refused_without_mutation_and_current_baseline_records_spend()
-> anyhow::Result<()> {
    use anyhow::Context;
    let pool = db::connect("sqlite::memory:").await?;
    pool.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "CREATE TABLE requests (\
            request_id                      TEXT PRIMARY KEY,\
            user_id                         TEXT NOT NULL,\
            api_key_id                      TEXT NOT NULL,\
            model_id                        TEXT NOT NULL,\
            provider_id                     TEXT NOT NULL,\
            prompt_tokens                   INTEGER NOT NULL,\
            completion_tokens               INTEGER NOT NULL,\
            reasoning_tokens                INTEGER NOT NULL,\
            cache_read_tokens               INTEGER NOT NULL DEFAULT 0,\
            cache_write_tokens              INTEGER NOT NULL DEFAULT 0,\
            final_charge_micro_usd          INTEGER NOT NULL DEFAULT 0,\
            streamed                        INTEGER NOT NULL DEFAULT 0,\
            latency_ms                      INTEGER NOT NULL DEFAULT 0,\
            generation_time_ms              INTEGER NOT NULL DEFAULT 0,\
            error                           TEXT,\
            created_at                      TEXT NOT NULL\
         )",
    ))
    .await?;

    // A rejection fixture retains the original integer money field.
    pool.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO requests VALUES (\
            'r-legacy', 'u', 'k', 'm', 'p', 1, 2, 0, 0, 0, 42, 0, 0, 0, NULL, ?\
         )",
        [chrono::Utc::now().to_rfc3339().into()],
    ))
    .await?;

    let before = pool
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name",
        ))
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String>("", "type")?,
                row.try_get::<String>("", "name")?,
                row.try_get::<String>("", "tbl_name")?,
                row.try_get::<Option<String>>("", "sql")?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for _ in 0..2 {
        assert!(db::run_migrations(&pool).await.is_err());
        let after = pool
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name",
            ))
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String>("", "type")?,
                    row.try_get::<String>("", "name")?,
                    row.try_get::<String>("", "tbl_name")?,
                    row.try_get::<Option<String>>("", "sql")?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(after, before);
        let row = pool.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT final_charge_micro_usd,prompt_tokens,completion_tokens FROM requests WHERE request_id='r-legacy'")).await?.context("original money row disappeared")?;
        assert_eq!(row.try_get::<i64>("", "final_charge_micro_usd")?, 42);
        assert_eq!(row.try_get::<i64>("", "prompt_tokens")?, 1);
        assert_eq!(row.try_get::<i64>("", "completion_tokens")?, 2);
    }
    pool.close().await?;
    let current = db::connect("sqlite::memory:").await?;
    db::run_migrations(&current).await?;
    let store = MeteringStore::new(current.clone());
    let recorder = MeteringRecorder::new(store.clone());
    recorder.record(&mut ctx("current", 10, 5)).await?;
    assert_eq!(
        store
            .get_enforceable_spend("current", TimeWindow::ThisMonth)
            .await?,
        Some(70)
    );
    db::run_migrations(&current).await?;
    assert_eq!(
        store
            .get_enforceable_spend("current", TimeWindow::ThisMonth)
            .await?,
        Some(70)
    );
    let records = stored_rows(&store).await?;
    assert_eq!(records.len(), 1);
    assert_eq!(evidence(&records[0])?.charge_micro_usd, Some(70));
    current.close().await?;
    Ok(())
}

#[tokio::test]
async fn failed_request_still_records_with_error() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    let mut c = ctx("k1", 5, 0);
    c.error = Some(crate::gateway::error::BitrouterError::internal("boom"));
    recorder.record(&mut c).await?;
    let count = stored_rows(&store).await?.len();
    assert_eq!(count, 1, "failed requests still get a metering row");
    Ok(())
}

#[tokio::test]
async fn recent_requests_returns_newest_first_and_honors_the_limit() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    // Distinct request ids (ctx keys them off the token counts) settled in
    // ascending order, so "newest first" is observable.
    for n in 1..=5u64 {
        recorder.record(&mut ctx("k", n, n)).await?;
    }

    let rows = store
        .recent_requests_filtered(TimeWindow::ThisMonth, None, None, 3)
        .await?
        .rows;
    assert_eq!(rows.len(), 3, "the limit bounds what the view ever loads");

    // Operator rows include timestamps, latency, and the estimated charge.
    let first = &rows[0];
    assert!(
        !first.created_at.is_empty(),
        "timestamp is rendered per row"
    );
    assert_eq!(first.latency_ms, 100, "latency is rendered per row");
    assert!(
        first.estimated_charge_micro_usd > 0,
        "cost is rendered per row"
    );
    assert_eq!(first.model_id, "gpt-5");
    assert_eq!(first.provider_id, "openai");

    // Newest first: the descending order must be stable against the tie in
    // `created_at` that a fast test run produces, which the request_id tiebreak
    // provides.
    let all = store
        .recent_requests_filtered(TimeWindow::ThisMonth, None, None, 100)
        .await?
        .rows;
    assert_eq!(all.len(), 5);
    let mut sorted = all.clone();
    sorted.sort_by(|a, b| (&b.created_at, &b.request_id).cmp(&(&a.created_at, &a.request_id)));
    assert_eq!(all, sorted, "recent_requests must return newest-first");
    Ok(())
}

#[tokio::test]
async fn filtered_request_page_and_summary_apply_the_same_bounds_before_limit() -> anyhow::Result<()>
{
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());

    for (request_id, model, provider) in [
        ("matched-old", "gpt-5", "openai"),
        ("matched-new", "gpt-5", "openai"),
        ("wrong-provider", "gpt-5", "anthropic"),
        ("wrong-model", "gpt-4", "openai"),
        ("at-exclusive-end", "gpt-5", "openai"),
    ] {
        let mut request = ctx("filtered", 10, 5);
        request.request_id = request_id.to_string();
        request.model_id = model.to_string();
        request.provider_id = provider.to_string();
        recorder.record(&mut request).await?;
    }

    pool.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE requests SET created_at = CASE request_id
            WHEN 'matched-old' THEN '2026-09-01T00:00:00+00:00'
            WHEN 'matched-new' THEN '2026-09-01T00:01:00+00:00'
            WHEN 'wrong-provider' THEN '2026-09-01T00:02:00+00:00'
            WHEN 'wrong-model' THEN '2026-09-01T00:03:00+00:00'
            WHEN 'at-exclusive-end' THEN '2026-09-02T00:00:00+00:00'
            ELSE created_at
        END"
        .to_string(),
    ))
    .await?;

    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|_| {
            crate::gateway::error::BitrouterError::internal("invalid test start timestamp")
        })?;
    let end = chrono::DateTime::parse_from_rfc3339("2026-09-02T00:00:00Z")
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .map_err(|_| {
            crate::gateway::error::BitrouterError::internal("invalid test end timestamp")
        })?;
    let window = TimeWindow::Custom { start, end };
    let page = store
        .recent_requests_filtered(window, Some("gpt-5"), Some("openai"), 1)
        .await?;
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].request_id, "matched-new");
    assert!(page.truncated, "the second matching row must be observable");

    let summary = store
        .spend_summary_filtered(window, Some("gpt-5"), Some("openai"))
        .await?;
    assert_eq!(summary.requests, 2, "filters precede the page limit");
    assert_eq!(
        summary.spend_micro_usd, 140,
        "the exclusive end is excluded"
    );
    assert_eq!(summary.unpriced, 0);
    Ok(())
}

#[tokio::test]
async fn get_total_rate_counts_every_caller_not_one_key() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    // Two distinct callers — the shape of a default `skip_auth` install that
    // also serves a real key.
    recorder.record(&mut ctx("local", 10, 5)).await?;
    recorder.record(&mut ctx("brk_real", 20, 7)).await?;

    let scoped = store.get_rate("local").await?;
    assert_eq!(scoped.requests_per_minute, 1.0, "per-key stays per-key");

    let total = store.get_total_rate().await?;
    assert_eq!(total.requests_per_minute, 2.0);
    assert_eq!(total.tokens_per_minute, (10 + 5 + 20 + 7) as f64);
    Ok(())
}

#[tokio::test]
async fn an_untagged_caller_is_still_recorded_just_not_attributed() -> Result<()> {
    // "Requests made outside a launch are still attributed sanely (not
    // silently dropped)" — the acceptance list's easiest item to break.
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    recorder.record(&mut ctx("brk_direct", 10, 5)).await?;

    assert_eq!(
        store
            .get_enforceable_spend("brk_direct", TimeWindow::ThisMonth)
            .await?,
        Some(70)
    );
    let rows = store
        .recent_requests_filtered(TimeWindow::ThisMonth, None, None, 10)
        .await?
        .rows;
    assert_eq!(rows.len(), 1, "the row exists");
    Ok(())
}

// ── session-attributed spend ─────────────────────────────────────────────────

/// The identity event the session hook emits for one routed model request,
/// varying only the fields `spend_summary_for_acp_session` keys on. No
/// `acp_session_id`: no maintained adapter sends the dynamic header, so the
/// persisted binding is the native root/thread pair.
fn acp_identity(
    request_id: &str,
    controller: Option<&str>,
    harness: &str,
    root: Option<&str>,
    thread: Option<&str>,
) -> SessionIdentityObserved {
    SessionIdentityObserved {
        router_request_id: request_id.to_string(),
        origin: if controller.is_some() {
            RequestOrigin::AcpHarnessRequest
        } else {
            RequestOrigin::PureModelApi
        },
        // The public API-key identity, which reaches spans. It is **not** what
        // the spend query filters on — that is the route scope, derived from
        // the caller — so varying it here would separate nothing.
        api_principal_id: "local".to_string(),
        harness: Some(harness.to_string()),
        claimed_controller_instance_id: controller.map(str::to_string),
        acp_session_id: None,
        native_root_session_id: root.map(str::to_string),
        native_agent_thread_id: thread.map(str::to_string),
        native_parent_agent_thread_id: None,
        native_turn_id: None,
        legacy_workflow_session_id: None,
        api_continuation_id: None,
        evidence: Vec::new(),
        conflicts: Vec::new(),
        attributed: root.is_some() || thread.is_some(),
        route_scope: "default".to_string(),
        route_lease_id: None,
        route_lease_outcome: None,
    }
}

/// Record one priced request (`prompt × 2 + completion × 10` µ$) carrying
/// `identity`, under an explicit request id so equal token counts never
/// collide.
async fn record_attributed(
    recorder: &MeteringRecorder,
    request_id: &str,
    prompt: u64,
    completion: u64,
    identity: SessionIdentityObserved,
) -> Result<()> {
    record_attributed_as("acp", recorder, request_id, prompt, completion, identity).await
}

/// The same, under a named caller — whose api-key identity becomes the route
/// scope the spend query filters on.
async fn record_attributed_as(
    api_key: &str,
    recorder: &MeteringRecorder,
    request_id: &str,
    prompt: u64,
    completion: u64,
    identity: SessionIdentityObserved,
) -> Result<()> {
    let mut request = ctx(api_key, prompt, completion);
    request.request_id = request_id.to_string();
    request.emit(identity);
    recorder.record(&mut request).await
}

#[tokio::test]
async fn acp_session_spend_is_scoped_to_its_controller() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());

    // Two controllers whose harnesses happen to report the same session id.
    // Spec §5.5: neither may read the other's spend through it.
    record_attributed(
        &recorder,
        "one",
        10,
        5,
        acp_identity("one", Some("brc_one"), "claude_code", Some("shared"), None),
    )
    .await?;
    record_attributed(
        &recorder,
        "two",
        20,
        7,
        acp_identity("two", Some("brc_two"), "claude_code", Some("shared"), None),
    )
    .await?;

    let one = store
        .spend_summary_for_acp_session("acp", "brc_one", "shared", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((one.requests, one.spend_micro_usd), (1, 70));
    let two = store
        .spend_summary_for_acp_session("acp", "brc_two", "shared", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((two.requests, two.spend_micro_usd), (1, 110));

    // An unknown controller is empty, not an error and not everything.
    let none = store
        .spend_summary_for_acp_session("acp", "brc_never", "shared", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((none.requests, none.spend_micro_usd), (0, 0));
    Ok(())
}

/// Two callers that declare the *same* controller and session — the case a
/// claim-only key cannot separate.
///
/// The controller id is caller-declared and nothing verifies it, so two API
/// principals can name the same one, deliberately or by collision. Route
/// leases are namespaced by the route scope precisely so one cannot reach the
/// other's; the spend behind attributed cost has to match, or a cost line
/// would quietly sum two callers.
///
/// The isolation comes from the **caller**, not from a field on the identity
/// event: the recorder derives the scope exactly as the session hook does, so
/// an event that merely claimed a different principal would not separate
/// anything.
#[tokio::test]
async fn one_caller_never_reads_another_callers_spend() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());

    let same_claim = |request_id: &str| {
        acp_identity(
            request_id,
            Some("brc_same"),
            "claude_code",
            Some("same-session"),
            None,
        )
    };
    record_attributed_as("alice", &recorder, "alice", 10, 5, same_claim("alice")).await?;
    record_attributed_as("bob", &recorder, "bob", 20, 7, same_claim("bob")).await?;

    let alice = store
        .spend_summary_for_acp_session("alice", "brc_same", "same-session", TimeWindow::ThisMonth)
        .await?;
    assert_eq!(
        (alice.requests, alice.spend_micro_usd),
        (1, 70),
        "alice sees her own row and not bob's, despite identical claims"
    );

    let bob = store
        .spend_summary_for_acp_session("bob", "brc_same", "same-session", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((bob.requests, bob.spend_micro_usd), (1, 110));

    // And a third scope making the same claim sees neither.
    let stranger = store
        .spend_summary_for_acp_session("carol", "brc_same", "same-session", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((stranger.requests, stranger.spend_micro_usd), (0, 0));
    Ok(())
}

#[tokio::test]
async fn child_agent_spend_counts_toward_its_root_session() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());

    // Claude (§4.2): the ACP id is the native session id; a subagent keeps
    // it as root and adds its own agent id.
    record_attributed(
        &recorder,
        "claude-root",
        10,
        5,
        acp_identity(
            "claude-root",
            Some("brc_c"),
            "claude_code",
            Some("claude-session"),
            None,
        ),
    )
    .await?;
    record_attributed(
        &recorder,
        "claude-child",
        20,
        7,
        acp_identity(
            "claude-child",
            Some("brc_c"),
            "claude_code",
            Some("claude-session"),
            Some("agent-1"),
        ),
    )
    .await?;
    // Codex (§4.2): a root thread carries equal session-id and thread-id; a
    // child or fork keeps the root session-id under a new thread-id.
    record_attributed(
        &recorder,
        "codex-root",
        1,
        1,
        acp_identity(
            "codex-root",
            Some("brc_c"),
            "codex",
            Some("thread-root"),
            Some("thread-root"),
        ),
    )
    .await?;
    record_attributed(
        &recorder,
        "codex-child",
        2,
        2,
        acp_identity(
            "codex-child",
            Some("brc_c"),
            "codex",
            Some("thread-root"),
            Some("thread-child"),
        ),
    )
    .await?;
    record_attributed(
        &recorder,
        "codex-fork",
        3,
        3,
        acp_identity(
            "codex-fork",
            Some("brc_c"),
            "codex",
            Some("thread-root"),
            Some("thread-fork"),
        ),
    )
    .await?;

    // The root's figure is the tree's: 70 + 110.
    let claude = store
        .spend_summary_for_acp_session("acp", "brc_c", "claude-session", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((claude.requests, claude.spend_micro_usd), (2, 180));
    // 12 + 24 + 36.
    let codex = store
        .spend_summary_for_acp_session("acp", "brc_c", "thread-root", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((codex.requests, codex.spend_micro_usd), (3, 72));
    // A fork is a session the manager holds by its own id, and its own
    // traffic is reachable through it.
    let fork = store
        .spend_summary_for_acp_session("acp", "brc_c", "thread-fork", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((fork.requests, fork.spend_micro_usd), (1, 36));
    Ok(())
}

#[tokio::test]
async fn unattributed_rows_never_match_a_session() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());

    // No identity at all: a direct API client.
    recorder.record(&mut ctx("direct", 100, 100)).await?;
    // A controller's request whose harness sent no session evidence.
    record_attributed(
        &recorder,
        "no-session",
        100,
        100,
        acp_identity("no-session", Some("brc_u"), "claude_code", None, None),
    )
    .await?;
    // Pure model API traffic carrying a Claude session header that happens
    // to equal the controller's session id: no credential bound it, so it
    // belongs to no controller.
    record_attributed(
        &recorder,
        "leaky",
        100,
        100,
        acp_identity("leaky", None, "claude_code", Some("session-x"), None),
    )
    .await?;

    let all = store.spend_summary(TimeWindow::ThisMonth).await?;
    assert_eq!(all.requests, 3, "every row is still recorded");

    let scoped = store
        .spend_summary_for_acp_session("acp", "brc_u", "session-x", TimeWindow::ThisMonth)
        .await?;
    assert_eq!((scoped.requests, scoped.spend_micro_usd), (0, 0));
    Ok(())
}

#[tokio::test]
async fn acp_session_spend_honours_the_custom_window_end() -> Result<()> {
    let pool = pool().await;
    let store = MeteringStore::new(pool.clone());
    let recorder = MeteringRecorder::new(store.clone());
    record_attributed(
        &recorder,
        "windowed",
        10,
        5,
        acp_identity(
            "windowed",
            Some("brc_w"),
            "claude_code",
            Some("session-w"),
            None,
        ),
    )
    .await?;

    let now = chrono::Utc::now();
    let closed_before = TimeWindow::Custom {
        start: now - chrono::Duration::hours(1),
        end: now - chrono::Duration::minutes(30),
    };
    let excluded = store
        .spend_summary_for_acp_session("acp", "brc_w", "session-w", closed_before)
        .await?;
    assert_eq!(
        (excluded.requests, excluded.spend_micro_usd),
        (0, 0),
        "a row after the window's end must not be counted"
    );

    let open_now = TimeWindow::Custom {
        start: now - chrono::Duration::hours(1),
        end: now + chrono::Duration::hours(1),
    };
    let included = store
        .spend_summary_for_acp_session("acp", "brc_w", "session-w", open_now)
        .await?;
    assert_eq!((included.requests, included.spend_micro_usd), (1, 70));
    Ok(())
}

pub(super) async fn stored_rows(
    store: &MeteringStore,
) -> Result<Vec<super::entities::requests::Model>> {
    use sea_orm::{EntityTrait, QueryOrder};
    super::entities::requests::Entity::find()
        .order_by_asc(super::entities::requests::Column::RequestId)
        .all(&store.db)
        .await
        .map_err(|error| crate::gateway::error::BitrouterError::internal(error.to_string()))
}

pub(super) fn evidence(
    row: &super::entities::requests::Model,
) -> Result<super::pricing::ChargeEvidence> {
    let json = row.charge_evidence_json.as_deref().ok_or_else(|| {
        crate::gateway::error::BitrouterError::internal("missing persisted charge evidence")
    })?;
    serde_json::from_str(json)
        .map_err(|error| crate::gateway::error::BitrouterError::internal(error.to_string()))
}

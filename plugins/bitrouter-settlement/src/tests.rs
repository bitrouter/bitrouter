//! Phase-3 settlement tests: the v0/cloud bug-regression suite and the
//! `ChargeStrategy` chain (008 §3.2 / Phase 3 exit criteria).
//!
//! These are integration-style: they build a real `language_model::Pipeline`
//! with the settlement hooks + a `MockExecutor`, run requests, and inspect the
//! resulting database rows / events.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::Result as SdkResult;
use bitrouter_sdk::caller::{CallerContext, PaymentMethod};
use bitrouter_sdk::language_model::{
    ApiProtocol, Content, FinishReason, GenerateResult, GenerationParams, Message, MockExecutor,
    MockResponse, PipelineBuilder, PipelineContext, PipelineRequest, Prompt, Role, RouteHook,
    RoutingTarget, StaticRoutingTable, Usage,
};

use crate::byok::{ByokRouteHook, insert_byok_key};
use crate::charge::{ByokCharge, CreditCharge, MppCharge, add_credits, credit_balance};
use crate::db;
use crate::metrics_store::SqliteMetricsStore;
use crate::mpp::MppState;
use crate::pricing::{ModelPricing, PricingTable};
use crate::recorder::ReceiptRecorder;

async fn pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    db::migrate(&pool).await.unwrap();
    pool
}

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "openai".to_string(),
        service_id: "gpt-5".to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "provider-key".to_string(),
        api_protocol: ApiProtocol::Openai,
        api_key_override: None,
        api_base_override: None,
    }
}

fn routing_table() -> Arc<StaticRoutingTable> {
    let rt = Arc::new(StaticRoutingTable::new());
    rt.insert("gpt-5", vec![target()]);
    rt
}

/// A mock executor returning a fixed-usage result (100 prompt / 50 completion).
fn executor() -> Arc<MockExecutor> {
    Arc::new(MockExecutor::new(vec![MockResponse::Generate(
        GenerateResult {
            content: vec![Content::Text {
                text: "ok".to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                reasoning_tokens: 0,
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::Stop),
        },
    )]))
}

fn pricing() -> PricingTable {
    let mut t = PricingTable::new();
    // 2 µ$/input tok, 10 µ$/output tok → 100*2 + 50*10 = 700 µ$
    t.insert("openai", "gpt-5", ModelPricing::new(2.0, 10.0));
    t
}

fn request(caller: CallerContext) -> PipelineRequest {
    let prompt = Prompt {
        model: "gpt-5".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    };
    PipelineRequest::new("gpt-5", caller, prompt)
}

async fn receipt(pool: &SqlitePool, request_id: &str) -> Option<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM requests WHERE request_id = ?")
        .bind(request_id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

// ===== ChargeStrategy chain — mutual exclusion =====

/// A `RouteHook` that injects an `api_key_override` **without** emitting a BYOK
/// event — simulating an anonymous-router / registry injection (cloud #235).
struct AnonOverrideRouteHook;
#[async_trait]
impl RouteHook for AnonOverrideRouteHook {
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        _ctx: &mut PipelineContext,
    ) -> SdkResult<()> {
        for t in chain.iter_mut() {
            t.api_key_override = Some("anon-router-injected-key".to_string());
        }
        Ok(())
    }
}

#[tokio::test]
async fn byok_charge_claims_and_credit_charge_does_not_run() {
    let pool = pool().await;
    insert_byok_key(&pool, "bk1", "u1", "openai", "user-own-key", None)
        .await
        .unwrap();
    add_credits(&pool, "u1", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .route_hook(ByokRouteHook::new(pool.clone()))
        .charge_strategy(ByokCharge)
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k1", "u1", PaymentMethod::Byok));
    let resp = pipeline.execute(req).await.unwrap();

    // BYOK claimed: charge 0, and the credit balance was NOT touched.
    assert_eq!(resp.final_charge_micro_usd, 0);
    assert_eq!(credit_balance(&pool, "u1").await.unwrap(), 1_000_000);

    let row = receipt(&pool, &resp.request_id).await.unwrap();
    assert_eq!(row.get::<String, _>("funding_source"), "byok");
    assert_eq!(row.get::<i64, _>("byok_used"), 1);
    assert_eq!(row.get::<i64, _>("final_charge_micro_usd"), 0);
}

// ===== cloud #235 — free billing on every request =====

#[tokio::test]
async fn regression_cloud_235_anon_override_without_byok_row_still_charges() {
    let pool = pool().await;
    add_credits(&pool, "u2", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    // The route hook injects api_key_override (like an anonymous router) but
    // there is NO byok_provider_keys row → no ByokKeyApplied event.
    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .route_hook(AnonOverrideRouteHook)
        .route_hook(ByokRouteHook::new(pool.clone()))
        .charge_strategy(ByokCharge)
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k2", "u2", PaymentMethod::Credits));
    let resp = pipeline.execute(req).await.unwrap();

    // The request is charged NORMALLY despite the injected override, and
    // byok_used is false — byok_used comes from the event, not the override.
    assert_eq!(resp.final_charge_micro_usd, 700);
    assert_eq!(credit_balance(&pool, "u2").await.unwrap(), 1_000_000 - 700);

    let row = receipt(&pool, &resp.request_id).await.unwrap();
    assert_eq!(row.get::<i64, _>("byok_used"), 0, "byok_used must be false");
    assert_eq!(row.get::<String, _>("funding_source"), "credits");
}

// ===== #180 / #440 / #443 — missing pricing is not free =====

#[tokio::test]
async fn regression_180_missing_pricing_skips_charge_not_silently_zero() {
    let pool = pool().await;
    add_credits(&pool, "u3", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    // Empty pricing table → the (openai, gpt-5) target is *unconfigured*.
    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .charge_strategy(CreditCharge::new(pool.clone(), PricingTable::new()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k3", "u3", PaymentMethod::Credits));
    let resp = pipeline.execute(req).await.unwrap();

    // Charge is skipped (0) — a *deliberate* skip — the credit balance is
    // untouched, and CreditCharge `Pass`es rather than claiming (004 §1.5), so
    // the request is left explicitly Unsettled, never silently "credits, 0".
    assert_eq!(resp.final_charge_micro_usd, 0);
    assert_eq!(credit_balance(&pool, "u3").await.unwrap(), 1_000_000);
    let row = receipt(&pool, &resp.request_id).await.unwrap();
    assert_eq!(row.get::<String, _>("funding_source"), "unsettled");
}

// ===== cloud #207 / #198 — receipts carry full context, failures recorded =====

#[tokio::test]
async fn regression_cloud_207_198_receipt_has_full_context() {
    let pool = pool().await;
    add_credits(&pool, "u4", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k4", "u4", PaymentMethod::Credits));
    let resp = pipeline.execute(req).await.unwrap();

    let row = receipt(&pool, &resp.request_id).await.unwrap();
    // identity + billing columns all populated (cloud #207 / #198)
    assert_eq!(row.get::<String, _>("user_id"), "u4");
    assert_eq!(row.get::<String, _>("api_key_id"), "k4");
    assert_eq!(row.get::<String, _>("model_id"), "gpt-5");
    assert_eq!(row.get::<String, _>("provider_id"), "openai");
    assert_eq!(row.get::<i64, _>("prompt_tokens"), 100);
    assert_eq!(row.get::<i64, _>("final_charge_micro_usd"), 700);
    assert!(row.get::<Option<String>, _>("error").is_none());
}

#[tokio::test]
async fn regression_cloud_198_failed_request_is_still_recorded() {
    let pool = pool().await;
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    // An executor that always errors.
    let failing = Arc::new(MockExecutor::new(vec![MockResponse::Error(
        bitrouter_sdk::BitrouterError::Upstream {
            status: 500,
            message: "upstream boom".to_string(),
        },
    )]));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(failing)
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k5", "u5", PaymentMethod::Credits));
    let request_id = req.request_id.clone();
    let err = pipeline.execute(req).await.unwrap_err();
    assert_eq!(err.status(), 502);

    // The failed request still produced a receipt, with a non-empty error.
    let row = receipt(&pool, &request_id).await.unwrap();
    let error = row.get::<Option<String>, _>("error");
    assert!(error.is_some(), "failed request must still be recorded");
    assert!(error.unwrap().contains("upstream"));
}

// ===== ByokRouteHook + event =====

#[tokio::test]
async fn byok_route_hook_injects_key_and_emits_event() {
    let pool = pool().await;
    insert_byok_key(
        &pool,
        "bk2",
        "u6",
        "openai",
        "u6-key",
        Some("https://u6.example"),
    )
    .await
    .unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .route_hook(ByokRouteHook::new(pool.clone()))
        .charge_strategy(ByokCharge)
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k6", "u6", PaymentMethod::Byok));
    let resp = pipeline.execute(req).await.unwrap();
    assert_eq!(resp.final_charge_micro_usd, 0);
    let row = receipt(&pool, &resp.request_id).await.unwrap();
    assert_eq!(row.get::<i64, _>("byok_used"), 1);
}

// ===== MetricsStore =====

#[tokio::test]
async fn metrics_store_aggregates_spend() {
    let pool = pool().await;
    add_credits(&pool, "u7", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(Arc::new(MockExecutor::new(vec![
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text { text: "a".into() }],
                usage: Some(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    reasoning_tokens: 0,
                    ..Default::default()
                }),
                finish_reason: Some(FinishReason::Stop),
            }),
            MockResponse::Generate(GenerateResult {
                content: vec![Content::Text { text: "b".into() }],
                usage: Some(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    reasoning_tokens: 0,
                    ..Default::default()
                }),
                finish_reason: Some(FinishReason::Stop),
            }),
        ])))
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics.clone()));
    let pipeline = Arc::new(b.build().unwrap());

    for _ in 0..2 {
        pipeline
            .clone()
            .execute(request(CallerContext::new(
                "k7",
                "u7",
                PaymentMethod::Credits,
            )))
            .await
            .unwrap();
    }

    use bitrouter_sdk::metrics::{MetricsStore, TimeWindow};
    let spend = metrics.get_spend("k7", TimeWindow::Today).await.unwrap();
    assert_eq!(spend, 1_400, "two 700µ$ requests aggregated");
    let count = metrics
        .get_request_count("k7", TimeWindow::Today)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn record_request_upsert_preserves_original_created_at() {
    // The receipt write is keyed by request_id and uses ON CONFLICT DO UPDATE
    // so a second write for the same id (streaming finalisation, retry) keeps
    // the first arrival's created_at. INSERT OR REPLACE used to reset it.
    use bitrouter_sdk::caller::FundingSource;
    use bitrouter_sdk::metrics::{MetricsStore, RequestMetric};
    let pool = pool().await;
    let metrics = SqliteMetricsStore::new(pool.clone());

    let base = RequestMetric {
        request_id: "rq1".to_string(),
        user_id: "u1".to_string(),
        api_key_id: "k1".to_string(),
        model_id: "gpt-5".to_string(),
        provider_id: "openai".to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        reasoning_tokens: 0,
        final_charge_micro_usd: 100,
        funding_source: FundingSource::Credits,
        byok_used: false,
        stream: false,
        latency_ms: 50,
        generation_time_ms: 40,
        error: None,
    };
    metrics.record_request(base.clone()).await.unwrap();
    let first_created_at: String =
        sqlx::query_scalar("SELECT created_at FROM requests WHERE request_id = ?")
            .bind("rq1")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Sleep just long enough that a fresh Utc::now() would render a different
    // RFC 3339 string, then overwrite with a different final charge.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut updated = base.clone();
    updated.final_charge_micro_usd = 200;
    updated.error = Some("oops".to_string());
    metrics.record_request(updated).await.unwrap();

    let second_created_at: String =
        sqlx::query_scalar("SELECT created_at FROM requests WHERE request_id = ?")
            .bind("rq1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let final_charge: i64 =
        sqlx::query_scalar("SELECT final_charge_micro_usd FROM requests WHERE request_id = ?")
            .bind("rq1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let error_col: Option<String> =
        sqlx::query_scalar("SELECT error FROM requests WHERE request_id = ?")
            .bind("rq1")
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(
        first_created_at, second_created_at,
        "created_at must not be reset on conflict"
    );
    assert_eq!(final_charge, 200, "mutable fields are refreshed");
    assert_eq!(error_col.as_deref(), Some("oops"));
}

// ===== BalanceCheckHook — cloud #225 =====

#[tokio::test]
async fn regression_cloud_225_byok_caller_not_balance_gated() {
    use crate::balance::BalanceCheckHook;
    use bitrouter_sdk::language_model::{HookDecision, PreRequestHook};

    let pool = pool().await;
    // a BYOK caller with ZERO credit balance — must NOT be rejected (#225)
    let hook = BalanceCheckHook::new(pool.clone(), None);
    let mut ctx =
        PipelineContext::new(request(CallerContext::new("k8", "u8", PaymentMethod::Byok)));
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Allow
    ));

    // a Credits caller with zero balance IS rejected with 402
    let mut ctx2 = PipelineContext::new(request(CallerContext::new(
        "k9",
        "u9",
        PaymentMethod::Credits,
    )));
    match hook.check(&mut ctx2).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 402);
        }
        HookDecision::Allow => panic!("credits caller with no balance must be denied"),
    }
}

// ===== MppCharge — Tempo only, Solana not wired =====

#[tokio::test]
async fn mpp_solana_channel_is_not_wired() {
    let pool = pool().await;
    // constructing a Solana MPP channel is an explicit error in v1.0
    assert!(MppState::solana(pool).is_err());
}

#[tokio::test]
async fn mpp_charge_settles_against_tempo_channel() {
    let pool = pool().await;
    let mpp = MppState::tempo(pool.clone());
    mpp.open_session("sess1", "u10", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .charge_strategy(ByokCharge)
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .charge_strategy(MppCharge::new(mpp.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let req = request(CallerContext::new("k10", "u10", PaymentMethod::Mpp));
    let resp = pipeline.execute(req).await.unwrap();
    assert_eq!(resp.final_charge_micro_usd, 700);
    // the Tempo channel balance was debited
    assert_eq!(mpp.balance("sess1").await.unwrap(), 1_000_000 - 700);

    let row = receipt(&pool, &resp.request_id).await.unwrap();
    assert_eq!(row.get::<String, _>("funding_source"), "mpp");
}

// ===== plugin DB isolation =====

#[tokio::test]
async fn settlement_owns_only_its_own_tables() {
    // The settlement migration creates exactly its five tables and no others —
    // in particular, never auth's `users` / `api_keys`.
    let pool = pool().await;
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let tables: Vec<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();
    assert_eq!(
        tables,
        vec![
            "byok_provider_keys",
            "credit_accounts",
            "credit_ledger_entries",
            "mpp_sessions",
            "requests",
        ],
    );
}

// ===== credit ledger idempotency (004 §7.5) =====

#[tokio::test]
async fn credit_charge_through_pipeline_writes_one_ledger_entry() {
    let pool = pool().await;
    add_credits(&pool, "u-led", 1_000_000).await.unwrap();
    let metrics: Arc<SqliteMetricsStore> = Arc::new(SqliteMetricsStore::new(pool.clone()));

    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(executor())
        .charge_strategy(CreditCharge::new(pool.clone(), pricing()))
        .settlement_recorder(ReceiptRecorder::new(metrics));
    let pipeline = Arc::new(b.build().unwrap());

    let resp = pipeline
        .execute(request(CallerContext::new(
            "k",
            "u-led",
            PaymentMethod::Credits,
        )))
        .await
        .unwrap();
    assert_eq!(resp.final_charge_micro_usd, 700);
    // one ledger row from add_credits (+1M) + one from the charge (-700) = 2
    assert_eq!(
        crate::charge::credit_ledger_count(&pool, "u-led")
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn deduct_credits_is_idempotent_on_key() {
    let pool = pool().await;
    add_credits(&pool, "u-idem", 1_000_000).await.unwrap();

    // First deduction with key "req-1" — debits the balance, writes a ledger row.
    let charged = crate::charge::deduct_credits(&pool, "u-idem", 700, "req-1")
        .await
        .unwrap();
    assert!(charged, "first deduction is applied");
    assert_eq!(
        credit_balance(&pool, "u-idem").await.unwrap(),
        1_000_000 - 700
    );

    // Retry with the SAME key — must be a no-op (no double-debit, no new row).
    let retried = crate::charge::deduct_credits(&pool, "u-idem", 700, "req-1")
        .await
        .unwrap();
    assert!(!retried, "duplicate deduction reports 'not charged'");
    assert_eq!(
        credit_balance(&pool, "u-idem").await.unwrap(),
        1_000_000 - 700,
        "balance is debited exactly once"
    );

    // A different key debits again.
    let charged2 = crate::charge::deduct_credits(&pool, "u-idem", 300, "req-2")
        .await
        .unwrap();
    assert!(charged2);
    assert_eq!(
        credit_balance(&pool, "u-idem").await.unwrap(),
        1_000_000 - 1000
    );

    // Ledger: add_credits(1) + req-1(1) + req-2(1) = 3 rows; the retry added none.
    assert_eq!(
        crate::charge::credit_ledger_count(&pool, "u-idem")
            .await
            .unwrap(),
        3
    );
}

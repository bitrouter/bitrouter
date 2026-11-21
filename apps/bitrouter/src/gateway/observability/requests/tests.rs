use super::*;
use crate::storage::gateway_records::GatewayRecords;
use anyhow::{Context, ensure};
use bitrouter_ai::types::{GenerationParams, Message, Prompt, Role};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::caller::CallerContext;
use crate::gateway::observability::binding::CanonicalRequestBinding;
use crate::gateway::pipeline::builder::PipelineBuilder;
use crate::gateway::pipeline::execution::Pipeline;
use crate::gateway::pipeline::executor::HttpExecutor;
use crate::gateway::pipeline::hooks::RouteHook;
use crate::gateway::pipeline::types::PipelineRequest;
use crate::metering::{MeteringRecorder, MeteringStore};
use crate::policy_source::PolicyRuntime;
use crate::session_identity::{NativeSessionIdentity, RequestOrigin};
use crate::storage::gateway_records::records;

mod binding;
mod storage;

/// The tracker consumes an authenticated, normalized scope from earlier hooks.
/// Full assembly tests exercise the actual auth and session extraction paths.
struct NormalizedScope;

#[async_trait]
impl PreRequestHook for NormalizedScope {
    async fn check(
        &self,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        let scope = if ctx.caller().api_key_id() == "key" {
            "scope"
        } else {
            "other-scope"
        };
        ctx.emit(ApiPrincipalEstablished {
            route_scope_id: scope.into(),
        });
        ctx.insert_extension(Arc::new(RequestSessionContext {
            origin: RequestOrigin::AcpHarnessRequest,
            api_principal_id: scope.into(),
            claimed_controller_instance_id: ctx
                .headers()
                .get("x-bitrouter-controller-id")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            acp_session_id: ctx
                .headers()
                .get("x-bitrouter-acp-session-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            native: NativeSessionIdentity::default(),
            legacy_workflow_session_id: None,
            api_continuation_id: None,
            evidence: Vec::new(),
            conflicts: Vec::new(),
            route_lease: None,
        }));
        Ok(HookDecision::Allow)
    }
}

struct RejectRequest;

#[async_trait]
impl PreRequestHook for RejectRequest {
    async fn check(
        &self,
        _ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        Err(BitrouterError::internal("fixture policy rejection"))
    }
}

#[async_trait]
impl RouteHook for RejectRequest {
    async fn resolve(
        &self,
        _chain: &mut Vec<RoutingTarget>,
        _ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<()> {
        Err(BitrouterError::internal("fixture route rejection"))
    }
}

enum Failure {
    None,
    Preparation,
    Upstream,
    Route,
    Change(binding::Change),
    Hold(HoldWinner),
}

struct Fixture {
    db: DatabaseConnection,
    tracker: GatewayRequestTracker,
    binding: CanonicalRequestBinding,
    routing: Arc<ConfigRoutingTable>,
    policies: Arc<PolicyRuntime>,
    upstream: MockServer,
    pipeline: Pipeline,
}

async fn fixture(failure: Failure) -> Result<Fixture> {
    fixture_with_storage(failure, None).await
}

async fn fixture_with_storage(
    failure: Failure,
    storage: Option<Arc<OnceLock<crate::tenancy::storage::Budget>>>,
) -> Result<Fixture> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let tracker = GatewayRequestTracker::new(db.clone());
    let bro_tables = db.query_one(sea_orm::Statement::from_string(
        sea_orm::DbBackend::Sqlite,
        "SELECT count(*) AS n FROM sqlite_schema WHERE type='table' AND (name LIKE 'acp_capture_%' OR name='acp_canonical_sessions')"
    )).await?.context("no gateway schema")?.try_get::<i64>("","n")?;
    assert_eq!(bro_tables, 0, "fresh gateway database contains BRO tables");
    let upstream = MockServer::start().await;
    if matches!(failure, Failure::Upstream) {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&upstream)
            .await;
    } else {
        Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"response", "object":"chat.completion", "model":"fixture-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"response"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
        }))).mount(&upstream).await;
    }
    let config = crate::config::parse_with(
        &format!(
            r#"
inherit_defaults: false
registry:
  enabled: false
providers:
  fixture:
    api_base: "{}"
    api_key: fixture-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: fixture-model
models:
  model:
    endpoints:
      - provider: fixture
        service_id: fixture-model
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    let routing = Arc::new(ConfigRoutingTable::from_config(config.clone()));
    let policies = PolicyRuntime::new(&config, None, None, Default::default()).await?;
    let binding = CanonicalRequestBinding::new(db.clone(), routing.clone(), policies.clone());
    let (tracker, binding) = match storage {
        Some(storage) => (
            tracker.with_storage_budget(storage.clone()),
            binding.with_storage_budget(storage),
        ),
        None => (tracker, binding),
    };
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(routing.clone())
        .executor(Arc::new(HttpExecutor::with_defaults()?))
        .pre_resolution_hook(NormalizedScope)
        .router_preparation_hook(tracker.clone())
        .router_preparation_hook(binding.clone())
        .route_hook(binding.clone())
        .observe_hook(tracker.clone())
        .settlement_recorder(MeteringRecorder::new(MeteringStore::new(db.clone())))
        .settlement_recorder(tracker.clone());
    match failure {
        Failure::None | Failure::Upstream => {}
        Failure::Preparation => {
            builder.pre_request_hook(RejectRequest);
        }
        Failure::Route => {
            builder.route_hook(RejectRequest);
        }
        Failure::Hold(hold) => {
            builder.pre_request_hook(hold);
        }
        Failure::Change(change) => {
            builder.pre_request_hook(binding::ChangeAfterAdmission {
                change,
                db: db.clone(),
                routing: routing.clone(),
                policies: policies.clone(),
            });
        }
    }
    Ok(Fixture {
        db,
        tracker,
        binding,
        routing,
        policies,
        upstream,
        pipeline: builder.build()?,
    })
}

fn request(id: &str, bound: bool) -> Result<PipelineRequest> {
    let prompt = Prompt {
        model: "model".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hello")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let mut request = PipelineRequest::new("model", CallerContext::new("key", "caller"), prompt);
    request.request_id = id.into();
    request
        .headers
        .insert("x-bitrouter-controller-id", "controller".parse()?);
    if bound {
        request
            .headers
            .insert("x-bitrouter-acp-session-id", "session".parse()?);
    }
    Ok(request)
}

async fn saved(fixture: &Fixture, id: &str) -> Result<GatewayRequest> {
    fixture
        .tracker
        .inventory()
        .request("scope", id)
        .await?
        .context("inventory missing")
}

#[tokio::test]
async fn tracked_requests_settle_and_reject_replay_without_learning() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    for (id, bound) in [("bound", true), ("unresolved", false)] {
        fixture.pipeline.execute(request(id, bound)?).await?;
        fixture
            .pipeline
            .drain_required_pending_settlements()
            .await?;
        let before = saved(&fixture, id).await?;
        let settlement = before.settlement.as_ref().context("settlement missing")?;
        assert_eq!(settlement.outcome, Some(ExecutionOutcome::Completed));
        assert_eq!(settlement.hops.len(), 1);
        assert_eq!(settlement.hops[0].status, "completed");
        assert_eq!(settlement.final_metered_cost_micro_usd, None);
        assert_eq!(settlement.total_cost_micro_usd, None);
        let charge_before = crate::metering::entities::requests::Entity::find_by_id(id)
            .one(&fixture.db)
            .await?;
        assert!(charge_before.is_some());
        assert!(fixture.pipeline.execute(request(id, bound)?).await.is_err());
        assert_eq!(
            serde_json::to_value(saved(&fixture, id).await?)?,
            serde_json::to_value(before)?
        );
        assert_eq!(
            crate::metering::entities::requests::Entity::find_by_id(id)
                .one(&fixture.db)
                .await?,
            charge_before
        );
    }
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn route_failure_retains_terminal_inventory_without_a_learning_consumer() -> Result<()> {
    let fixture = fixture(Failure::Route).await?;
    let error = fixture
        .pipeline
        .execute(request("denied", true)?)
        .await
        .err()
        .context("request admitted")?;
    assert!(error.to_string().contains("fixture route rejection"));
    let before = saved(&fixture, "denied").await?;
    let settlement = before.settlement.as_ref().context("settlement missing")?;
    assert_eq!(settlement.outcome, Some(ExecutionOutcome::Failed));
    assert!(settlement.hops.is_empty());
    assert_eq!(settlement.total_cost_micro_usd, Some(0));
    assert!(
        fixture
            .pipeline
            .execute(request("denied", true)?)
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(saved(&fixture, "denied").await?)?,
        serde_json::to_value(before)?
    );
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn interrupted_admission_keeps_its_identity_without_inventing_settlement() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let inventory = fixture.tracker.inventory();
    let BeginResult::Tracked(original) = inventory
        .begin("scope", "controller", "interrupted")
        .await?
    else {
        anyhow::bail!("fresh request was not admitted");
    };
    assert!(original.settlement.is_none());
    assert!(
        fixture
            .pipeline
            .execute(request("interrupted", true)?)
            .await
            .is_err()
    );
    let after = inventory
        .request("scope", "interrupted")
        .await?
        .context("admission lost")?;
    assert_eq!(
        serde_json::to_value(after)?,
        serde_json::to_value(&original)?
    );
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .is_empty()
    );
    assert!(
        crate::metering::entities::requests::Entity::find_by_id("interrupted")
            .one(&fixture.db)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn preparation_failure_retains_unsettled_inventory_without_inventing_usage() -> Result<()> {
    let fixture = fixture(Failure::Preparation).await?;
    assert!(
        fixture
            .pipeline
            .execute(request("preparation", true)?)
            .await
            .is_err()
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let before = saved(&fixture, "preparation").await?;
    assert!(before.settlement.is_none());
    assert!(
        fixture
            .pipeline
            .execute(request("preparation", true)?)
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(saved(&fixture, "preparation").await?)?,
        serde_json::to_value(before)?
    );
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .is_empty()
    );
    assert!(
        crate::metering::entities::requests::Entity::find_by_id("preparation")
            .one(&fixture.db)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn context_revalidation_retains_the_original_request_and_attempt() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let mut ctx = PipelineContext::new(request("rebuild", true)?);
    let retained = ctx.require_generation_prompt()?.messages.clone();
    let mut initial = ctx.require_generation_prompt()?.clone();
    initial
        .messages
        .insert(0, Message::text(Role::User, "old history"));
    ctx.apply_routing_prompt(initial)?;
    NormalizedScope.check(&mut ctx).await?;
    fixture.tracker.check(&mut ctx).await?;
    fixture.binding.check(&mut ctx).await?;
    let before = saved(&fixture, "rebuild").await?;
    let observation = serde_json::to_value(ctx.get_event::<GatewayRequestObserved>())?;

    ctx.replace_managed_messages(retained)?;
    for _ in 0..2 {
        assert!(matches!(
            fixture.tracker.revalidate_context(&ctx).await?,
            HookDecision::Allow
        ));
        assert!(matches!(
            PreRequestHook::revalidate_context(&fixture.binding, &ctx).await?,
            HookDecision::Allow
        ));
        RouteHook::revalidate_context(&fixture.binding, &[], &ctx).await?;
    }
    assert_eq!(
        serde_json::to_value(saved(&fixture, "rebuild").await?)?,
        serde_json::to_value(before)?
    );
    assert_eq!(
        serde_json::to_value(ctx.get_event::<GatewayRequestObserved>())?,
        observation
    );
    assert_eq!(
        records::Entity::find()
            .filter(records::Column::Kind.eq(super::super::inventory::INVENTORY_KIND))
            .count(&fixture.db)
            .await?,
        1
    );
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn provider_failure_retains_failed_hop_and_never_replays_it() -> Result<()> {
    let fixture = fixture(Failure::Upstream).await?;
    assert!(
        fixture
            .pipeline
            .execute(request("after-send", false)?)
            .await
            .is_err()
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let original = saved(&fixture, "after-send").await?;
    let settlement = original.settlement.as_ref().context("settlement missing")?;
    assert_eq!(settlement.outcome, Some(ExecutionOutcome::Failed));
    assert_eq!(settlement.hops.len(), 1);
    assert_eq!(settlement.hops[0].status, "failed");
    assert!(settlement.total_cost_micro_usd.is_none());
    assert!(
        fixture
            .pipeline
            .execute(binding::second_request("after-send")?)
            .await
            .is_err()
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    assert_eq!(
        serde_json::to_value(saved(&fixture, "after-send").await?)?,
        serde_json::to_value(original)?
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn global_charge_prevents_new_admission_without_an_inventory_row() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let inventory = fixture.tracker.inventory();
    let mut ordinary = request("paid-before-inventory", false)?;
    ordinary.headers.remove("x-bitrouter-controller-id");
    fixture.pipeline.execute(ordinary).await?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    // A retained metering charge owns the ID even when admission data is missing.
    let inventory_store = crate::storage::gateway_records::GatewayRecords::new(
        fixture.db.clone(),
        "gateway-request-admission",
    )?;
    let key = crate::storage::gateway_records::digest(&"paid-before-inventory")?;
    ensure!(
        records::Entity::delete_by_id(inventory_store.id("gateway_request", &key)?)
            .exec(&fixture.db)
            .await?
            .rows_affected
            == 1
    );
    let charges = crate::metering::entities::requests::Entity::find_by_id("paid-before-inventory");
    let original = charges
        .clone()
        .one(&fixture.db)
        .await?
        .context("missing charge")?;
    assert!(
        inventory
            .request("scope", "paid-before-inventory")
            .await?
            .is_none()
    );
    assert!(
        fixture
            .pipeline
            .execute(binding::second_request("paid-before-inventory")?)
            .await
            .is_err()
    );
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    assert_eq!(charges.one(&fixture.db).await?, Some(original));
    assert!(
        inventory
            .request("scope", "paid-before-inventory")
            .await?
            .is_none()
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn request_namespace_excludes_foreign_rows_before_decoding() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let store = GatewayRecords::new(fixture.db.clone(), "gateway-request-admission")?;
    let key = crate::storage::gateway_records::digest(&"marked")?;
    store
        .initialize("gateway_request", &key, None, &json!({}))
        .await?;
    let record = store.id("gateway_request", &key)?;
    use sea_orm::{DbBackend, Statement};
    fixture.db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
        "UPDATE gateway_request_records SET body='poison', creator_principal_id='other' WHERE record_id=?",
        [record.clone().into()])).await?;
    let inventory = fixture.tracker.inventory();
    assert!(inventory.request("scope", "marked").await?.is_none());
    assert!(
        inventory
            .begin("scope", "controller", "marked")
            .await
            .is_err()
    );
    assert_eq!(
        records::Entity::find_by_id(record)
            .one(&fixture.db)
            .await?
            .context("occupied key lost")?
            .body,
        "poison"
    );
    assert!(matches!(
        inventory.begin("scope", "controller", "fresh").await?,
        BeginResult::Tracked(_)
    ));
    assert!(inventory.request("other-scope", "fresh").await?.is_none());
    assert!(matches!(
        inventory
            .begin("other-scope", "controller", "fresh")
            .await?,
        BeginResult::Duplicate
    ));
    ensure!(inventory.request("scope", "fresh").await?.is_some());
    Ok(())
}

#[derive(Clone, Default)]
struct HoldWinner {
    admitted: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl PreRequestHook for HoldWinner {
    async fn check(
        &self,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<HookDecision> {
        if ctx.caller().api_key_id() == "key" {
            self.admitted.notify_one();
            self.release.notified().await;
        }
        Ok(HookDecision::Allow)
    }
}

#[tokio::test]
async fn global_claim_excludes_foreign_billed_requests_before_settlement() -> Result<()> {
    let hold = HoldWinner::default();
    let fixture = fixture(Failure::Hold(hold.clone())).await?;
    let check = async {
        hold.admitted.notified().await;
        let mut other = request("in-flight", true)?;
        other.caller = CallerContext::new("second-key", "second-user");
        // Ordinary HTTP and controller-tagged clients share the same ledger.
        other.headers.remove("x-bitrouter-controller-id");
        other
            .extensions
            .insert(crate::gateway::metering::replay::ClientRequestIdClaim);
        ensure!(fixture.pipeline.execute(other).await.is_err());
        ensure!(
            fixture
                .tracker
                .inventory()
                .request("other-scope", "in-flight")
                .await?
                .is_none()
        );
        ensure!(
            crate::metering::entities::requests::Entity::find_by_id("in-flight")
                .one(&fixture.db)
                .await?
                .is_none()
        );
        hold.release.notify_one();
        Ok::<_, anyhow::Error>(())
    };
    let winner = fixture.pipeline.execute(request("in-flight", true)?);
    let (winner, checked) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(winner, check)
    })
    .await?;
    winner?;
    checked?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let row = crate::metering::entities::requests::Entity::find_by_id("in-flight")
        .one(&fixture.db)
        .await?
        .context("missing charge")?;
    ensure!(row.user_id == "caller" && row.api_key_id == "key");
    ensure!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("provider observation")?
            .len()
            == 1
    );
    Ok(())
}

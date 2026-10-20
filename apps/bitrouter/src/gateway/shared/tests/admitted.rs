//! Admission identity remains fixed across key edits and detached execution.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{ModelOperation, Prompt};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::auth::{
    db::{NewApiKey, insert_api_key, upsert_user},
    entities::api_keys,
    events::Authenticated,
    hook::AuthHook,
};
use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::{
    context::PipelineContext,
    hooks::{HookDecision, PreRequestHook},
    types::PipelineRequest,
};
use crate::gateway::shared::{
    admitted::SharedAdmission,
    authority::Authority,
    crypto::DeploymentKey,
    engine::{Admission, AdmissionResult, Engine, TenantLimits},
    store::{Kind, Store},
};

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    let authority = Authority::new(first.clone(), DeploymentKey::from_bytes([9; 32])?);
    let revision = authority.head().await?.revision();
    let engine = Engine::join(authority, 30_000).await?;
    let tenant = uuid::Uuid::new_v4().to_string();
    let principal_id = uuid::Uuid::new_v4().to_string();
    let key = crate::auth::keys::generate();
    upsert_user(first.database(), &tenant).await?;
    insert_api_key(
        first.database(),
        &NewApiKey {
            id: principal_id.clone(),
            key_hash: key.hash.clone(),
            user_id: tenant.clone(),
            spend_limit_micro_usd: None,
            rpm_limit: Some(2),
            policy_id: Some("original-policy".into()),
        },
    )
    .await?;
    engine
        .set_tenant(
            &tenant,
            None,
            &TenantLimits {
                concurrent_executions: 2,
                requests_per_minute: 2,
                resource_pools: Default::default(),
            },
        )
        .await?;
    let (execution, principal) = match engine
        .admit(&Admission {
            key_hash: &key.hash,
            correlation_id: "caller-supplied-id",
            idempotency_key: None,
            fingerprint: &"a".repeat(64),
            operation: ModelOperation::Generation,
            revision,
        })
        .await?
    {
        AdmissionResult::Accepted(execution, principal) => (execution, principal),
        AdmissionResult::Existing(_) => anyhow::bail!("unexpected fixture duplicate"),
    };
    ensure!(principal.api_key_id == principal_id && principal.user_id == tenant);
    ensure!(principal.policy_id.as_deref() == Some("original-policy"));
    let snapshot = SharedAdmission::new(engine.clone(), key.hash, execution.clone(), principal)?;
    let prompt = Prompt {
        model: "fixture".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![],
        tools: vec![],
        params: Default::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let mut request = PipelineRequest::new("fixture", CallerContext::anonymous(), prompt);
    request.headers.insert(
        "authorization",
        http::HeaderValue::from_str(&format!("Bearer {}", key.secret))?,
    );
    snapshot.attach(&mut request);
    ensure!(request.request_id != execution.correlation_id);

    // The auth hook's local backend is deliberately unusable. It must consume
    // the shared admission and its original database authority instead.
    let hook = AuthHook::new(sea_orm::DatabaseConnection::Disconnected);
    let detached = request.clone();
    let mut ctx = tokio::spawn(async move { PipelineContext::new(detached) }).await?;
    let carried = ctx
        .extension::<SharedAdmission>()
        .context("detached admission lost")?;
    ensure!(Arc::ptr_eq(&snapshot, &carried));
    let streamed = ctx
        .stream_context()
        .extension::<SharedAdmission>()
        .context("stream admission lost")?;
    ensure!(Arc::ptr_eq(&snapshot, &streamed));
    ensure!(matches!(hook.check(&mut ctx).await?, HookDecision::Allow));
    ensure!(matches!(
        hook.revalidate_context(&ctx).await?,
        HookDecision::Allow
    ));
    ensure!(matches!(
        hook.revalidate_context(&ctx).await?,
        HookDecision::Allow
    ));
    let quota = first
        .get::<serde_json::Value>(Kind::Tenant, &tenant)
        .await?
        .context("tenant quota missing")?;
    ensure!(quota.value["accepted"] == 1 && quota.value["active"] == 1);
    let rate = first
        .get::<serde_json::Value>(Kind::Principal, &principal_id)
        .await?
        .context("principal rate missing")?;
    ensure!(rate.value["accepted"] == 1);

    let mut mutated = request.clone();
    mutated.request_id = "forged-execution".into();
    ensure!(
        hook.check(&mut PipelineContext::new(mutated))
            .await
            .is_err()
    );
    let mut mutated = request.clone();
    mutated.caller = CallerContext::local();
    ensure!(
        hook.check(&mut PipelineContext::new(mutated))
            .await
            .is_err()
    );
    let mut mutated = request.clone();
    mutated.headers.insert(
        "authorization",
        http::HeaderValue::from_static("Bearer different-key"),
    );
    ensure!(
        hook.check(&mut PipelineContext::new(mutated))
            .await
            .is_err()
    );

    api_keys::Entity::update_many()
        .col_expr(
            api_keys::Column::PolicyId,
            Expr::value("replacement-policy"),
        )
        .filter(api_keys::Column::Id.eq(&principal_id))
        .exec(second.database())
        .await?;
    ensure!(hook.revalidate_context(&ctx).await.is_err());
    let mut late = PipelineContext::new(request.clone());
    ensure!(hook.check(&mut late).await.is_err());
    ensure!(late.get_event::<Authenticated>().is_none());
    ensure!(
        ctx.get_event::<Authenticated>()
            .context("original identity lost")?
            .policy_id
            .as_deref()
            == Some("original-policy")
    );

    api_keys::Entity::update_many()
        .col_expr(api_keys::Column::Active, Expr::value(0))
        .filter(api_keys::Column::Id.eq(&principal_id))
        .exec(second.database())
        .await?;
    let revoked = hook
        .check(&mut PipelineContext::new(request))
        .await
        .err()
        .context("revoked principal admitted")?;
    ensure!(revoked.status() == 401);
    engine
        .finish(
            &execution.execution_id,
            &bitrouter_ai::gateway::execution::TerminalOutcome::Failed {
                code: "fixture_no_dispatch".into(),
            },
        )
        .await?;
    Ok(())
}

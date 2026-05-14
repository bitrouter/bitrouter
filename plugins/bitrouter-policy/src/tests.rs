//! Phase-4 policy tests: `PolicyHook` enforcement + combination semantics.

use std::sync::Arc;

use bitrouter_sdk::PluginId;
use bitrouter_sdk::caller::{CallerContext, PaymentMethod};
use bitrouter_sdk::language_model::{
    GenerationParams, HookDecision, Message, PipelineContext, PipelineRequest, PreRequestHook,
    Prompt, Role,
};

use crate::hook::PolicyHook;
use crate::policy::Policy;
use crate::store::PolicyStore;

fn ctx(model: &str, policy_id: Option<&str>) -> PipelineContext {
    let prompt = Prompt {
        model: model.to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    };
    let req = PipelineRequest::new(
        model,
        CallerContext::new("k1", "u1", PaymentMethod::Credits),
        prompt,
    );
    let mut ctx = PipelineContext::new(req);
    // simulate what AuthHook writes into the context metadata
    if let Some(pid) = policy_id {
        ctx.set_metadata(
            &PluginId::new("bitrouter-auth"),
            serde_json::json!({ "api_key_id": "k1", "user_id": "u1", "policy_id": pid }),
        );
    }
    ctx
}

#[tokio::test]
async fn no_policy_bound_permits_everything() {
    let store = Arc::new(PolicyStore::new());
    let hook = PolicyHook::new(store, None);
    let mut c = ctx("any-model", None);
    assert!(matches!(
        hook.check(&mut c).await.unwrap(),
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn denied_model_is_forbidden() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        denied_models: vec!["forbidden-model".into()],
        ..Default::default()
    }]));
    let hook = PolicyHook::new(store, None);

    let mut allowed = ctx("ok-model", Some("p1"));
    assert!(matches!(
        hook.check(&mut allowed).await.unwrap(),
        HookDecision::Allow
    ));

    let mut denied = ctx("forbidden-model", Some("p1"));
    match hook.check(&mut denied).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 403);
        }
        HookDecision::Allow => panic!("denied model must be forbidden"),
    }
}

#[tokio::test]
async fn allowlist_restricts_models() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        allowed_models: Some(vec!["gpt-5".into()]),
        ..Default::default()
    }]));
    let hook = PolicyHook::new(store, None);

    let mut ok = ctx("gpt-5", Some("p1"));
    assert!(matches!(
        hook.check(&mut ok).await.unwrap(),
        HookDecision::Allow
    ));
    let mut nope = ctx("claude", Some("p1"));
    assert!(matches!(
        hook.check(&mut nope).await.unwrap(),
        HookDecision::Deny(_)
    ));
}

#[tokio::test]
async fn expired_policy_is_forbidden() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..Default::default()
    }]));
    let hook = PolicyHook::new(store, None);
    let mut c = ctx("gpt-5", Some("p1"));
    assert!(matches!(
        hook.check(&mut c).await.unwrap(),
        HookDecision::Deny(_)
    ));
}

#[tokio::test]
async fn store_loads_from_yaml_dir() {
    let dir = std::env::temp_dir().join(format!("brpolicy-{}", uuid_like()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("team.yaml"),
        "id: team\nallowed_models: [gpt-5, claude]\nmax_spend_micro_usd: 5000\n",
    )
    .await
    .unwrap();
    let store = PolicyStore::load_dir(&dir).await.unwrap();
    assert_eq!(store.len(), 1);
    let p = store.get("team").unwrap();
    assert_eq!(p.max_spend_micro_usd, Some(5000));
    assert_eq!(p.allowed_models.as_ref().unwrap().len(), 2);
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

fn uuid_like() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

// ===== chain / tool / rate checks (004 §4.1) =====

use async_trait::async_trait;
use bitrouter_sdk::language_model::Tool;
use bitrouter_sdk::metrics::{MetricsStore, RateMetrics, RequestMetric, TimeWindow, TokenUsage};

/// Build a context for an MPP caller (so chain checks engage) with optional
/// auth metadata.
fn mpp_ctx(policy_id: Option<&str>) -> PipelineContext {
    let prompt = Prompt {
        model: "m".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    };
    let req = PipelineRequest::new(
        "m",
        CallerContext::new("sess-1", "u1", PaymentMethod::Mpp),
        prompt,
    );
    let mut ctx = PipelineContext::new(req);
    if let Some(pid) = policy_id {
        ctx.set_metadata(
            &PluginId::new("bitrouter-auth"),
            serde_json::json!({ "api_key_id": "sess-1", "user_id": "u1", "policy_id": pid }),
        );
    }
    ctx
}

/// Build a context whose request declares the given tools.
fn ctx_with_tools(tools: &[&str], policy_id: Option<&str>) -> PipelineContext {
    let prompt = Prompt {
        model: "m".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: tools
            .iter()
            .map(|name| Tool {
                name: name.to_string(),
                description: None,
                parameters: serde_json::json!({}),
            })
            .collect(),
        params: GenerationParams::default(),
        stream: false,
    };
    let req = PipelineRequest::new(
        "m",
        CallerContext::new("k1", "u1", PaymentMethod::Credits),
        prompt,
    );
    let mut ctx = PipelineContext::new(req);
    if let Some(pid) = policy_id {
        ctx.set_metadata(
            &PluginId::new("bitrouter-auth"),
            serde_json::json!({ "api_key_id": "k1", "user_id": "u1", "policy_id": pid }),
        );
    }
    ctx
}

/// A `MetricsStore` mock returning a fixed spend + rate.
struct MockMetrics {
    rpm: f64,
}
#[async_trait]
impl MetricsStore for MockMetrics {
    async fn get_spend(&self, _key: &str, _w: TimeWindow) -> bitrouter_sdk::Result<u64> {
        Ok(0)
    }
    async fn get_request_count(&self, _k: &str, _w: TimeWindow) -> bitrouter_sdk::Result<u64> {
        Ok(0)
    }
    async fn get_token_usage(
        &self,
        _k: &str,
        _m: &str,
        _w: TimeWindow,
    ) -> bitrouter_sdk::Result<TokenUsage> {
        Ok(TokenUsage::default())
    }
    async fn get_rate(&self, _key: &str) -> bitrouter_sdk::Result<RateMetrics> {
        Ok(RateMetrics {
            requests_per_minute: self.rpm,
            tokens_per_minute: 0.0,
        })
    }
    async fn record_request(&self, _r: RequestMetric) -> bitrouter_sdk::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn chain_limit_gates_mpp_callers() {
    // policy allows only `solana`; the v1.0 MPP caller is on `tempo` → denied.
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        allowed_chains: Some(vec!["solana".into()]),
        ..Default::default()
    }]));
    let hook = PolicyHook::new(store, None);
    let mut c = mpp_ctx(Some("p1"));
    match hook.check(&mut c).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 403);
        }
        HookDecision::Allow => panic!("MPP caller on a disallowed chain must be denied"),
    }

    // policy allowing `tempo` lets the same caller through.
    let store_ok = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        allowed_chains: Some(vec!["tempo".into()]),
        ..Default::default()
    }]));
    let hook_ok = PolicyHook::new(store_ok, None);
    let mut c2 = mpp_ctx(Some("p1"));
    assert!(matches!(
        hook_ok.check(&mut c2).await.unwrap(),
        HookDecision::Allow
    ));
}

#[tokio::test]
async fn tool_rules_gate_requested_tools() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        allowed_tools: Some(vec!["search".into()]),
        ..Default::default()
    }]));
    let hook = PolicyHook::new(store, None);

    // a request using only the allowed tool passes
    let mut ok = ctx_with_tools(&["search"], Some("p1"));
    assert!(matches!(
        hook.check(&mut ok).await.unwrap(),
        HookDecision::Allow
    ));

    // a request using a tool not in the allowlist is forbidden
    let mut nope = ctx_with_tools(&["search", "filesystem"], Some("p1"));
    match hook.check(&mut nope).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 403);
        }
        HookDecision::Allow => panic!("disallowed tool must be forbidden"),
    }
}

#[tokio::test]
async fn rate_limit_is_enforced_via_metrics_store() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        max_requests_per_minute: Some(60),
        ..Default::default()
    }]));

    // under the limit → allowed
    let under: Arc<dyn MetricsStore> = Arc::new(MockMetrics { rpm: 30.0 });
    let hook_under = PolicyHook::new(store.clone(), Some(under));
    let mut c1 = ctx("m", Some("p1"));
    assert!(matches!(
        hook_under.check(&mut c1).await.unwrap(),
        HookDecision::Allow
    ));

    // at/over the limit → 429 RateLimited
    let over: Arc<dyn MetricsStore> = Arc::new(MockMetrics { rpm: 75.0 });
    let hook_over = PolicyHook::new(store, Some(over));
    let mut c2 = ctx("m", Some("p1"));
    match hook_over.check(&mut c2).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 429);
        }
        HookDecision::Allow => panic!("over-rate request must be rate-limited"),
    }
}

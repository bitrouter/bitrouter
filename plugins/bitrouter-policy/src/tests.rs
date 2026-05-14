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

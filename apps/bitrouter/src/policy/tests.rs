//! PolicyHook enforcement + combination-semantics tests.

use std::sync::Arc;

use bitrouter_sdk::PluginId;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{
    GenerationParams, HookDecision, Message, PipelineContext, PipelineRequest, PreRequestHook,
    Prompt, Role, Tool,
};
use sqlx::SqlitePool;

use crate::metering::{MeteringStore, RequestMetric, migrate as metering_migrate};
use crate::policy::hook::PolicyHook;
use crate::policy::policy::Policy;
use crate::policy::store::PolicyStore;

fn ctx(model: &str, policy_id: Option<&str>) -> PipelineContext {
    let prompt = Prompt {
        model: model.to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    };
    let req = PipelineRequest::new(model, CallerContext::new("k1", "u1"), prompt);
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
    let req = PipelineRequest::new("m", CallerContext::new("k1", "u1"), prompt);
    let mut ctx = PipelineContext::new(req);
    if let Some(pid) = policy_id {
        ctx.set_metadata(
            &PluginId::new("bitrouter-auth"),
            serde_json::json!({ "api_key_id": "k1", "user_id": "u1", "policy_id": pid }),
        );
    }
    ctx
}

async fn fresh_metering() -> MeteringStore {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    metering_migrate(&pool).await.unwrap();
    MeteringStore::new(pool)
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
    store.with_policy("team", |p| {
        let p = p.expect("team policy loaded");
        assert_eq!(p.max_spend_micro_usd, Some(5000));
        assert_eq!(p.allowed_models.as_ref().unwrap().len(), 2);
    });
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn reload_re_reads_the_policy_dir() {
    let dir = std::env::temp_dir().join(format!("brpolicy-reload-{}", uuid_like()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("a.yaml"),
        "id: team-a\nmax_spend_micro_usd: 5000\n",
    )
    .await
    .unwrap();
    let store = PolicyStore::load_dir(&dir).await.unwrap();
    assert_eq!(store.len(), 1);

    // Add a second policy file and drop the first.
    tokio::fs::remove_file(dir.join("a.yaml")).await.unwrap();
    tokio::fs::write(
        dir.join("b.yaml"),
        "id: team-b\nmax_spend_micro_usd: 9000\n",
    )
    .await
    .unwrap();

    store.reload().await.unwrap();
    assert_eq!(store.len(), 1, "old policy gone, new one in");
    store.with_policy("team-a", |p| {
        assert!(p.is_none(), "deleted policy must not survive reload");
    });
    store.with_policy("team-b", |p| {
        let p = p.expect("team-b loaded");
        assert_eq!(p.max_spend_micro_usd, Some(9000));
    });
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

fn uuid_like() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
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
async fn spend_cap_is_enforced_via_metering_store() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        max_spend_micro_usd: Some(100),
        ..Default::default()
    }]));
    let metering = fresh_metering().await;

    // Seed an existing 60µ$ row for the caller — under the cap.
    metering
        .record_request(RequestMetric {
            request_id: "r1".into(),
            user_id: "u1".into(),
            api_key_id: "k1".into(),
            model_id: "gpt-5".into(),
            provider_id: "openai".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            estimated_charge_micro_usd: 60,
            latency_ms: 100,
            generation_time_ms: 80,
            streamed: false,
            error: None,
        })
        .await
        .unwrap();
    let hook = PolicyHook::new(store.clone(), Some(metering.clone()));
    let mut c = ctx("gpt-5", Some("p1"));
    assert!(
        matches!(hook.check(&mut c).await.unwrap(), HookDecision::Allow),
        "60µ$ < 100µ$ cap → allow"
    );

    // Push the rolling spend over the cap.
    metering
        .record_request(RequestMetric {
            request_id: "r2".into(),
            user_id: "u1".into(),
            api_key_id: "k1".into(),
            model_id: "gpt-5".into(),
            provider_id: "openai".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            estimated_charge_micro_usd: 50,
            latency_ms: 100,
            generation_time_ms: 80,
            streamed: false,
            error: None,
        })
        .await
        .unwrap();
    let mut c2 = ctx("gpt-5", Some("p1"));
    match hook.check(&mut c2).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 403);
        }
        HookDecision::Allow => panic!("110µ$ > 100µ$ cap should deny"),
    }
}

#[tokio::test]
async fn rate_limit_is_enforced_via_metering_store() {
    let store = Arc::new(PolicyStore::from_policies([Policy {
        id: "p1".into(),
        max_requests_per_minute: Some(2),
        ..Default::default()
    }]));
    let metering = fresh_metering().await;

    // Two requests recorded in the trailing minute (this minute) → at limit.
    for i in 0..2 {
        metering
            .record_request(RequestMetric {
                request_id: format!("r{i}"),
                user_id: "u1".into(),
                api_key_id: "k1".into(),
                model_id: "gpt-5".into(),
                provider_id: "openai".into(),
                prompt_tokens: 1,
                completion_tokens: 1,
                reasoning_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                estimated_charge_micro_usd: 0,
                latency_ms: 0,
                generation_time_ms: 0,
                streamed: false,
                error: None,
            })
            .await
            .unwrap();
    }
    let hook = PolicyHook::new(store, Some(metering));
    let mut c = ctx("gpt-5", Some("p1"));
    match hook.check(&mut c).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 429);
        }
        HookDecision::Allow => panic!("at-limit request must be rate-limited"),
    }
}

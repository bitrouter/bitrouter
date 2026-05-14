//! Phase-3 auth tests: the `skip_auth` truth table and the `brvk_` validation
//! flow (008 Phase 3 exit criteria).

use sqlx::SqlitePool;

use bitrouter_sdk::caller::{CallerContext, PaymentMethod};
use bitrouter_sdk::language_model::{
    GenerationParams, HookDecision, Message, PipelineContext, PipelineRequest, PreRequestHook,
    Prompt, Role,
};

use crate::db::{self, NewApiKey};
use crate::events::Authenticated;
use crate::hook::AuthHook;
use crate::keys;

async fn pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    db::migrate(&pool).await.unwrap();
    pool
}

fn prompt() -> Prompt {
    Prompt {
        model: "m".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    }
}

/// Build a context whose caller is the pre-auth anonymous placeholder, with an
/// optional bearer credential header.
fn ctx_with(caller: CallerContext, bearer: Option<&str>) -> PipelineContext {
    let mut req = PipelineRequest::new("m", caller, prompt());
    if let Some(token) = bearer {
        req.headers
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    PipelineContext::new(req)
}

/// Insert a fresh active key, returning its plaintext secret + id.
async fn insert_active_key(pool: &SqlitePool, user: &str) -> (String, String) {
    db::upsert_user(pool, user).await.unwrap();
    let key = keys::generate();
    let id = format!("key_{user}");
    db::insert_api_key(
        pool,
        &NewApiKey {
            id: id.clone(),
            key_hash: key.hash.clone(),
            user_id: user.to_string(),
            payment_method: PaymentMethod::Credits,
            spend_limit_micro_usd: Some(1_000_000),
            rpm_limit: Some(60),
            policy_id: Some("pol_default".to_string()),
        },
    )
    .await
    .unwrap();
    (key.secret, id)
}

#[tokio::test]
async fn valid_key_authenticates_and_emits_event() {
    let pool = pool().await;
    let (secret, key_id) = insert_active_key(&pool, "u1").await;
    let hook = AuthHook::new(pool);

    let mut ctx = ctx_with(CallerContext::anonymous(), Some(&secret));
    let decision = hook.check(&mut ctx).await.unwrap();
    assert!(matches!(decision, HookDecision::Allow));

    // caller upgraded from anonymous to the real identity
    assert_eq!(ctx.caller().api_key_id(), key_id);
    assert_eq!(ctx.caller().user_id(), "u1");
    assert_eq!(ctx.caller().payment_method(), PaymentMethod::Credits);
    assert_eq!(ctx.caller().spend_limit(), Some(1_000_000));
    assert!(!ctx.caller().is_anonymous());

    // the Authenticated event is broadcast for downstream hooks
    let event = ctx.get_event::<Authenticated>().expect("event emitted");
    assert_eq!(event.api_key_id, key_id);
    assert_eq!(event.policy_id.as_deref(), Some("pol_default"));
}

#[tokio::test]
async fn unknown_key_is_denied_401() {
    let pool = pool().await;
    let hook = AuthHook::new(pool);
    let fresh = keys::generate(); // never inserted
    let mut ctx = ctx_with(CallerContext::anonymous(), Some(&fresh.secret));
    match hook.check(&mut ctx).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 401);
        }
        HookDecision::Allow => panic!("unknown key must be denied"),
    }
}

#[tokio::test]
async fn non_virtual_key_is_denied() {
    let pool = pool().await;
    let hook = AuthHook::new(pool);
    // an OpenAI-style key — v1 has no JWT / sk- path
    let mut ctx = ctx_with(CallerContext::anonymous(), Some("sk-not-a-brvk-key"));
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Deny(_)
    ));
}

// ===== skip_auth truth table (004 §3.4) =====

#[tokio::test]
async fn truth_table_no_skip_auth_no_credential_denies() {
    // skip_auth=false → caller is anonymous; no credential → 401
    let pool = pool().await;
    let hook = AuthHook::new(pool);
    let mut ctx = ctx_with(CallerContext::anonymous(), None);
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Deny(_)
    ));
}

#[tokio::test]
async fn truth_table_skip_auth_no_credential_allows_local() {
    // skip_auth=true → server synthesised a local caller; no credential → Allow
    let pool = pool().await;
    let hook = AuthHook::new(pool);
    let mut ctx = ctx_with(CallerContext::local(), None);
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Allow
    ));
    // the local caller is left intact
    assert!(ctx.caller().is_local());
}

#[tokio::test]
async fn truth_table_skip_auth_with_credential_still_validates() {
    // skip_auth=true but a credential IS present → it is still validated, and
    // the credential's identity wins over the synthesised local one.
    let pool = pool().await;
    let (secret, key_id) = insert_active_key(&pool, "u2").await;
    let hook = AuthHook::new(pool);
    let mut ctx = ctx_with(CallerContext::local(), Some(&secret));
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Allow
    ));
    assert_eq!(ctx.caller().api_key_id(), key_id);
    assert!(!ctx.caller().is_local());
}

#[tokio::test]
async fn truth_table_skip_auth_with_bad_credential_still_rejected() {
    // skip_auth=true does NOT excuse a *bad* credential — it is still rejected.
    let pool = pool().await;
    let hook = AuthHook::new(pool);
    let mut ctx = ctx_with(CallerContext::local(), Some("sk-bad"));
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Deny(_)
    ));
}

#[tokio::test]
async fn inactive_key_is_denied() {
    let pool = pool().await;
    db::upsert_user(&pool, "u3").await.unwrap();
    let key = keys::generate();
    db::insert_api_key(
        &pool,
        &NewApiKey {
            id: "key_inactive".to_string(),
            key_hash: key.hash.clone(),
            user_id: "u3".to_string(),
            payment_method: PaymentMethod::Credits,
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await
    .unwrap();
    // flip it inactive
    sqlx::query("UPDATE api_keys SET active = 0 WHERE id = 'key_inactive'")
        .execute(&pool)
        .await
        .unwrap();

    let hook = AuthHook::new(pool);
    let mut ctx = ctx_with(CallerContext::anonymous(), Some(&key.secret));
    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Deny(_)
    ));
}

// ===== MPP credential path (004 §3.1 / §3.3) =====

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::{MppVerification, MppVerifier};

use crate::events::MppVerified;

/// A scripted `MppVerifier` — resolves exactly one credential to one channel.
struct MockMppVerifier {
    credential: String,
    verification: MppVerification,
}

#[async_trait]
impl MppVerifier for MockMppVerifier {
    async fn verify(&self, credential: &str) -> bitrouter_sdk::Result<Option<MppVerification>> {
        Ok((credential == self.credential).then(|| self.verification.clone()))
    }
}

/// Build a context with a `payment-signature` header (no API key).
fn ctx_with_payment(caller: CallerContext, payment: &str) -> PipelineContext {
    let mut req = PipelineRequest::new("m", caller, prompt());
    req.headers
        .insert("payment-signature", payment.parse().unwrap());
    PipelineContext::new(req)
}

fn mock_verifier(credential: &str) -> Arc<dyn MppVerifier> {
    Arc::new(MockMppVerifier {
        credential: credential.to_string(),
        verification: MppVerification {
            session_id: "sess-1".to_string(),
            user_id: "mpp-user".to_string(),
            channel_balance_micro_usd: 50_000,
        },
    })
}

#[tokio::test]
async fn mpp_credential_authenticates_and_emits_events() {
    let hook = AuthHook::new(pool().await).with_mpp_verifier(mock_verifier("session=sess-1"));
    let mut ctx = ctx_with_payment(CallerContext::anonymous(), "session=sess-1");

    assert!(matches!(
        hook.check(&mut ctx).await.unwrap(),
        HookDecision::Allow
    ));
    // caller upgraded to an MPP identity
    assert_eq!(ctx.caller().payment_method(), PaymentMethod::Mpp);
    assert_eq!(ctx.caller().user_id(), "mpp-user");
    assert_eq!(ctx.caller().api_key_id(), "sess-1");

    // both the Authenticated and MppVerified events are broadcast
    let auth = ctx
        .get_event::<Authenticated>()
        .expect("Authenticated emitted");
    assert_eq!(auth.payment_method, PaymentMethod::Mpp);
    let mpp = ctx.get_event::<MppVerified>().expect("MppVerified emitted");
    assert_eq!(mpp.session_id, "sess-1");
    assert_eq!(mpp.channel_balance, 50_000);
}

#[tokio::test]
async fn mpp_unknown_credential_is_payment_required() {
    let hook = AuthHook::new(pool().await).with_mpp_verifier(mock_verifier("session=sess-1"));
    let mut ctx = ctx_with_payment(CallerContext::anonymous(), "session=unknown");
    match hook.check(&mut ctx).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 402);
        }
        HookDecision::Allow => panic!("unknown MPP credential must be denied"),
    }
}

#[tokio::test]
async fn mpp_credential_without_verifier_is_payment_required() {
    // No MPP verifier wired — a payment credential cannot be honoured → 402.
    let hook = AuthHook::new(pool().await);
    let mut ctx = ctx_with_payment(CallerContext::anonymous(), "session=sess-1");
    match hook.check(&mut ctx).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 402);
        }
        HookDecision::Allow => panic!("MPP credential needs a verifier"),
    }
}

#[tokio::test]
async fn api_key_and_payment_credential_together_is_400() {
    let pool = pool().await;
    let (secret, _) = insert_active_key(&pool, "u-both").await;
    let hook = AuthHook::new(pool).with_mpp_verifier(mock_verifier("session=sess-1"));

    // present BOTH an API key and an MPP payment credential — mutual exclusion
    let mut req = PipelineRequest::new("m", CallerContext::anonymous(), prompt());
    req.headers
        .insert("authorization", format!("Bearer {secret}").parse().unwrap());
    req.headers
        .insert("payment-signature", "session=sess-1".parse().unwrap());
    let mut ctx = PipelineContext::new(req);

    match hook.check(&mut ctx).await.unwrap() {
        HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 400);
        }
        HookDecision::Allow => panic!("both credentials present must be 400"),
    }
}

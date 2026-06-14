//! Route-hook behavior: Record tags without dropping, Enforce drops unverified
//! confidential targets, and non-confidential providers are untouched.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_attestation::{
    AttestationVerdict, ConfidentialVerifier, ExchangeInput, VerifiedExchange, VerifierRegistry,
    VerifyError,
};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{
    ApiProtocol, GenerationParams, Message, PipelineContext, PipelineRequest, Prompt, Role,
    RouteHook, RoutingTarget,
};

use crate::hooks::AttestationOutcome;
use crate::{AttestationConfig, AttestationPolicy, AttestationRouteHook};

/// A verdict with only the fields the hook reads (`model`, `verified`) set.
fn verdict(model: &str, verified: bool) -> AttestationVerdict {
    // Nonce derived from `model` (not a hard-coded literal); the hook only
    // reads `model`/`verified`, never the recorded nonce.
    let mut v = AttestationVerdict::unverified(model, format!("test-nonce-{model}"), 1);
    v.verified = verified;
    v
}

/// A `near-ai` verifier that returns a fixed `verified` flag.
struct StubVerifier {
    verified: bool,
}

#[async_trait]
impl ConfidentialVerifier for StubVerifier {
    fn provider(&self) -> &str {
        "near-ai"
    }
    async fn verify_attestation(
        &self,
        model: &str,
        _nonce: &str,
        _now_unix: u64,
    ) -> Result<AttestationVerdict, VerifyError> {
        Ok(verdict(model, self.verified))
    }
    async fn verify_exchange(
        &self,
        _ex: &ExchangeInput<'_>,
    ) -> Result<VerifiedExchange, VerifyError> {
        Err(VerifyError::Malformed {
            what: "exchange",
            detail: "not used in this test".to_string(),
        })
    }
}

fn config(policy: AttestationPolicy, verified: bool) -> AttestationConfig {
    let registry = VerifierRegistry::new().with(Arc::new(StubVerifier { verified }));
    AttestationConfig::new(policy, Arc::new(registry))
}

fn target(provider: &str, model: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.to_string(),
        service_id: model.to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "k".to_string(),
        api_protocol: ApiProtocol::ChatCompletions,
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
    }
}

fn ctx() -> PipelineContext {
    let prompt = Prompt {
        model: "m".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "hi")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    PipelineContext::new(PipelineRequest::new(
        "m",
        CallerContext::new("k", "u"),
        prompt,
    ))
}

#[tokio::test]
async fn record_tags_the_context_and_never_drops() {
    // Record + an UNVERIFIED verdict: the confidential target must stay, and the
    // verdict must be deposited for downstream stages.
    let hook = AttestationRouteHook::new(config(AttestationPolicy::Record, false));
    let mut chain = vec![target("near-ai", "near-model"), target("openai", "gpt-4o")];
    let mut c = ctx();

    hook.resolve(&mut chain, &mut c).await.unwrap();

    assert_eq!(chain.len(), 2, "Record never drops a target");
    let outcome = c
        .extension::<AttestationOutcome>()
        .expect("verdict deposited");
    assert_eq!(outcome.verdicts.len(), 1);
    assert_eq!(outcome.verdicts[0].model, "near-model");
    assert!(!outcome.verdicts[0].verified);
    assert!(
        c.get_metadata(&bitrouter_sdk::PluginId::new("bitrouter-attestation"))
            .is_some()
    );
}

#[tokio::test]
async fn enforce_drops_an_unverified_confidential_target() {
    let hook = AttestationRouteHook::new(config(AttestationPolicy::Enforce, false));
    let mut chain = vec![target("near-ai", "near-model"), target("openai", "gpt-4o")];
    let mut c = ctx();

    hook.resolve(&mut chain, &mut c).await.unwrap();

    assert_eq!(chain.len(), 1, "unverified near-ai target dropped");
    assert_eq!(chain[0].provider_name, "openai", "non-confidential kept");
}

#[tokio::test]
async fn enforce_keeps_a_verified_confidential_target() {
    let hook = AttestationRouteHook::new(config(AttestationPolicy::Enforce, true));
    let mut chain = vec![target("near-ai", "near-model"), target("openai", "gpt-4o")];
    let mut c = ctx();

    hook.resolve(&mut chain, &mut c).await.unwrap();

    assert_eq!(chain.len(), 2, "verified target kept under Enforce");
}

#[tokio::test]
async fn non_confidential_providers_are_untouched() {
    // No near-ai target: the hook does nothing — no verdict, no extension.
    let hook = AttestationRouteHook::new(config(AttestationPolicy::Enforce, false));
    let mut chain = vec![target("openai", "gpt-4o"), target("anthropic", "claude")];
    let mut c = ctx();

    hook.resolve(&mut chain, &mut c).await.unwrap();

    assert_eq!(chain.len(), 2);
    assert!(c.extension::<AttestationOutcome>().is_none());
}

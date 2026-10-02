use super::*;
use crate::language_model::auth::CredentialAuthority;
use crate::language_model::types::PipelineRequest;
use crate::language_model::types::{ApiProtocol, AuthScheme, FinishReason, Message, Role, Usage};

struct FailAfterWrite;
impl NativePrivateContextPolicy for FailAfterWrite {
    fn validate_history(
        &self,
        _: &Prompt,
        _: &CallerContext,
    ) -> std::result::Result<(), PrivateContextFailure> {
        Ok(())
    }
    fn validate_target(
        &self,
        _: &Prompt,
        _: &CallerContext,
        _: &RoutingTarget,
    ) -> std::result::Result<(), PrivateContextFailure> {
        Ok(())
    }
    fn validate_authority(
        &self,
        _: &Prompt,
        _: &CallerContext,
        _: &RoutingTarget,
        _: &ContinuationAuthority,
    ) -> std::result::Result<(), PrivateContextFailure> {
        Ok(())
    }
    fn seal(
        &self,
        content: &mut [Content],
        _: &CallerContext,
        _: &RoutingTarget,
        _: &ContinuationAuthority,
    ) -> std::result::Result<(), PrivateContextFailure> {
        metadata_mut(&mut content[0]).insert(
            ORIGIN_NAMESPACE.into(),
            serde_json::json!({ORIGIN_FIELD:"partial"}),
        );
        Err(PrivateContextFailure::KeyUnavailable)
    }
}

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "fixture".into(),
        service_id: "served".into(),
        api_base: "https://example.invalid".into(),
        api_key: "fixture-key".into(),
        api_protocol: ApiProtocol::Messages,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        reasoning_effort: None,
        model_constraints: Default::default(),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: AuthScheme::XApiKey,
        headers: Vec::new(),
    }
}

fn context() -> PipelineContext {
    PipelineContext::new(PipelineRequest {
        request_id: "request".into(),
        original_model: "served".into(),
        model: "served".into(),
        caller: CallerContext::local(),
        headers: Default::default(),
        inbound_protocol: None,
        prompt: Prompt {
            model: "served".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "task")],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        },
    })
}

fn result() -> GenerateResult {
    GenerateResult {
        content: vec![Content::Reasoning {
            text: "thought".into(),
            provider_metadata: [
                (
                    "anthropic".into(),
                    serde_json::json!({"signature":"fixture-signature"}),
                ),
                (
                    ORIGIN_NAMESPACE.into(),
                    serde_json::json!({ORIGIN_FIELD:"untrusted"}),
                ),
            ]
            .into(),
        }],
        usage: Some(Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            ..Default::default()
        }),
        finish_reason: Some(FinishReason::Stop),
        response_id: Some("fixture-response".into()),
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

fn authority() -> ContinuationAuthority {
    ContinuationAuthority::new(
        CredentialAuthority::derive("fixture", "principal"),
        AuthScheme::XApiKey,
    )
}

#[test]
fn private_output_never_promotes_unproven_mutated_or_stale_attempts() {
    for case in [
        "custom_executor",
        "changed_result",
        "new_attempt",
        "missing_authority",
        "partial_seal",
    ] {
        let runtime = NativePrivateContextRuntime::new(Some(Arc::new(FailAfterWrite)));
        let mut output = result();
        let usage = output.usage.clone();
        if case != "custom_executor" {
            runtime.succeeded(
                context().prompt(),
                &target(),
                (case != "missing_authority").then(authority),
                &output,
            );
        }
        if case == "changed_result" {
            output.content.push(Content::Text {
                text: "injected".into(),
                provider_metadata: Default::default(),
            });
        }
        if case == "new_attempt" {
            runtime.begin_attempt();
        }
        runtime.seal_output(&context(), &target(), &mut output);
        assert_eq!(output.usage, usage);
        assert_eq!(output.response_id.as_deref(), Some("fixture-response"));
        assert!(
            output
                .content
                .iter()
                .all(|part| metadata(part).get(ORIGIN_NAMESPACE).is_none())
        );
        let reason = match case {
            "missing_authority" => PrivateContextFailure::AuthorityUnavailable,
            "partial_seal" => PrivateContextFailure::KeyUnavailable,
            _ => PrivateContextFailure::AttemptUnverified,
        };
        assert_eq!(
            runtime.observation().output,
            PrivateContextEvidence::Unverified { reason }
        );
        assert_eq!(runtime.observation().input, PrivateContextEvidence::Unknown);
    }
}

#[test]
fn private_message_commitment_ignores_all_markers_but_binds_all_other_content()
-> std::result::Result<(), PrivateContextFailure> {
    let mut parts = result().content;
    parts.push(parts[0].clone());
    let original = message_commitment(&parts)?;
    clear_origins(&mut parts);
    assert_eq!(message_commitment(&parts)?, original);
    metadata_mut(&mut parts[1]).insert(
        ORIGIN_NAMESPACE.into(),
        serde_json::json!({ORIGIN_FIELD:"another"}),
    );
    assert_eq!(message_commitment(&parts)?, original);
    parts.push(Content::Text {
        text: "altered assistant".into(),
        provider_metadata: Default::default(),
    });
    assert_ne!(message_commitment(&parts)?, original);
    Ok(())
}

#[tokio::test]
async fn native_continuation_binds_the_actual_http_prompt_and_redacts_unverified_output()
-> Result<()> {
    use crate::language_model::executor::{Executor, HttpExecutor};
    use crate::language_model::native::NativeManagedRequest;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"resp_native_private", "status":"completed", "store":true,
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],
            "usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}
        }))).mount(&upstream).await;
    let mut route = target();
    route.api_protocol = ApiProtocol::Responses;
    route.api_base = upstream.uri();
    let runtime = Arc::new(NativePrivateContextRuntime::new(Some(Arc::new(
        FailAfterWrite,
    ))));
    let mut ctx = context();
    ctx.insert_extension(runtime.clone());
    ctx.insert_extension(Arc::new(NativeManagedRequest));
    // A custom wrapper can change the explicit prompt while retaining ctx. The
    // built-in HTTP execution must not certify ctx's untransmitted prefix.
    let mut dispatched = ctx.prompt().clone();
    dispatched.messages = vec![Message::text(Role::User, "different actual input")];
    let mut output = HttpExecutor::with_defaults()?
        .execute(&route, &dispatched, &ctx)
        .await?
        .result;
    runtime.seal_output(&ctx, &route, &mut output);
    assert_eq!(
        runtime.continuation_observation().output,
        NativeContinuationOutput::Unverified {
            reason: ContinuationFailure::BindingChanged
        }
    );
    assert!(output.response_id.is_none());
    assert!(runtime.response_terminal_valid());
    assert_eq!(
        output.usage.as_ref().map(|usage| usage.prompt_tokens),
        Some(10)
    );
    let requests = upstream
        .received_requests()
        .await
        .ok_or_else(|| BitrouterError::internal("missing requests"))?;
    assert_eq!(requests.len(), 1);
    assert!(String::from_utf8_lossy(&requests[0].body).contains("different actual input"));
    // No authenticated source, including a stream bridge/custom executor, must
    // still erase the provider ID without claiming a proven continuation.
    runtime.begin_attempt();
    output.response_id = Some("resp_custom_private".into());
    runtime.seal_output(&ctx, &route, &mut output);
    assert!(output.response_id.is_none());
    assert_eq!(
        runtime.continuation_observation().output,
        NativeContinuationOutput::Unverified {
            reason: ContinuationFailure::AttemptUnverified
        }
    );
    assert!(runtime.response_terminal_valid());
    runtime.begin_attempt();
    runtime.seal_output(&ctx, &route, &mut output);
    assert!(!runtime.response_terminal_valid());
    Ok(())
}

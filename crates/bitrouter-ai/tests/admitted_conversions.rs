//! Equivalent-effect reports cannot waive refusals or expose source payloads.
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::conversion::{
    ConversionDisposition, ConversionEffect, ConversionLocation, ConversionReason,
    ConversionReport, request_admission,
};
use bitrouter_ai::protocol::{InboundAdapter, chat_completions::ChatCompletionsAdapter};
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::{
    ApiProtocol, Content, Message, NativeReasoning, Prompt, ProviderMetadata, Role, Tool,
    ToolResultOutput,
};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
fn source() -> bitrouter_ai::error::Result<Prompt> {
    let mut prompt = ChatCompletionsAdapter
        .parse_request(json!({"model":"fixture","messages":[{"role":"user","content":"keep"}]}))?;
    prompt.messages = vec![Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            call_id: "call-secret".into(),
            tool_name: Some("f".into()),
            output: ToolResultOutput::Json {
                value: json!({"key-secret":[1,2]}),
            },
            dynamic: false,
            provider_metadata: ProviderMetadata::new(),
        }],
    }];
    Ok(prompt)
}
fn safe(report: &ConversionReport) -> TestResult {
    assert!(!format!("{report:?}").contains("secret"));
    assert!(!serde_json::to_string(report)?.contains("secret"));
    for effect in &report.admitted {
        assert_eq!(effect.effect, ConversionEffect::EquivalentRepresentation);
        assert_eq!(effect.disposition, ConversionDisposition::Allow);
    }
    Ok(())
}

#[test]
fn json_encoding_is_reported_without_becoming_a_refusal() -> TestResult {
    let prompt = source()?;
    let original = prompt.clone();
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
        ApiProtocol::Messages,
    ] {
        let report = request_admission(&prompt, &protocol);
        assert!(report.issues.is_empty());
        assert_eq!(report.admitted.len(), 1);
        assert_eq!(
            report.admitted[0].reason,
            ConversionReason::ToolResultJsonEncoding
        );
        assert_eq!(
            report.admitted[0].location,
            ConversionLocation::MessageContent {
                message: 0,
                block: 0
            }
        );
        report.require_admitted()?;
        safe(&report)?;
    }
    assert!(
        request_admission(&prompt, &ApiProtocol::GenerateContent)
            .admitted
            .is_empty()
    );
    assert_eq!(prompt, original);
    Ok(())
}

#[test]
fn schema_normalization_is_reported_only_for_a_classified_actual_rewrite() -> TestResult {
    let mut prompt = source()?;
    prompt.tools = vec![Tool::Function {
        name: "f".into(),
        description: None,
        parameters: json!({"type":"object","properties":{"key-secret":{"type":["integer","null"]}}}),
        strict: None,
        provider_metadata: ProviderMetadata::new(),
    }];
    let report = request_admission(&prompt, &ApiProtocol::GenerateContent);
    report.require_admitted()?;
    assert_eq!(report.admitted.len(), 1);
    assert_eq!(
        report.admitted[0].reason,
        ConversionReason::GeminiSchemaNormalization
    );
    assert_eq!(
        report.admitted[0].location,
        ConversionLocation::ToolDefinition { tool: 0 }
    );
    safe(&report)?;
    if let Tool::Function { parameters, .. } = &mut prompt.tools[0] {
        *parameters = json!({"type":"object"});
    }
    assert!(
        request_admission(&prompt, &ApiProtocol::GenerateContent)
            .admitted
            .is_empty()
    );
    if let Tool::Function { parameters, .. } = &mut prompt.tools[0] {
        *parameters = json!({"type":["integer","null"],"exclusiveMinimum":0});
    }
    let refused = request_admission(&prompt, &ApiProtocol::GenerateContent);
    assert!(!refused.issues.is_empty());
    assert!(refused.admitted.is_empty());
    Ok(())
}

#[test]
fn an_equivalent_effect_cannot_waive_native_replay_or_task_refusals() -> TestResult {
    let mut prompt = source()?;
    prompt.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::Reasoning {
            text: "visible-secret".into(),
            native: Some(NativeReasoning::Responses(
                json!({"type":"reasoning","id":"item-secret","encrypted_content":"opaque-secret"}),
            )),
            provider_metadata: ProviderMetadata::new(),
        }],
    });
    let report = request_admission(&prompt, &ApiProtocol::Responses);
    assert_eq!(report.admitted.len(), 1);
    assert_eq!(report.issues.len(), 1);
    assert!(report.require_admitted().is_err());
    safe(&report)?;
    let encoded = serde_json::to_value(&report)?;
    assert_eq!(serde_json::from_value::<ConversionReport>(encoded)?, report);
    let legacy: ConversionReport = serde_json::from_value(json!({"issues":[]}))?;
    assert!(legacy.admitted.is_empty());
    Ok(())
}

#[test]
fn direct_selected_target_preparation_retains_the_same_assessment() -> TestResult {
    let prompt = source()?;
    let target = ModelTarget {
        provider_name: "provider-secret".into(),
        service_id: "model-secret".into(),
        api_base: "https://example.invalid/secret".into(),
        api_key: "credential-secret".into(),
        credential_priority: Default::default(),
        auth_scheme: Default::default(),
        api_protocol: ApiProtocol::Responses,
        account_label: Some("account-secret".into()),
        compatibility: Default::default(),
    };
    let client = ModelClient::new(HttpTimeouts::default())?;
    let (body, report) = client.render_request_with_report(&target, &prompt, false)?;
    assert_eq!(report, request_admission(&prompt, &ApiProtocol::Responses));
    assert_eq!(report.admitted.len(), 1);
    assert_eq!(body["model"], "model-secret");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            body["input"][0]["output"]
                .as_str()
                .ok_or("missing JSON encoding")?
        )?,
        json!({"key-secret":[1,2]})
    );
    safe(&report)?;
    Ok(())
}

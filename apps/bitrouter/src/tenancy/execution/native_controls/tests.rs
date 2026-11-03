use super::*;
use anyhow::{Context, Result};
use serde_json::{Value, json};

#[test]
fn actual_post_parse_merge_suppresses_source_present_keys_before_target_projection() -> Result<()> {
    for source in [Value::Null, json!("source-id")] {
        let mut input = crate::gateway::http::parse_input(
            &ApiProtocol::Responses,
            json!({"model":"route","input":"next","previous_response_id":source}),
        )?;
        let prompt = input.generation_prompt_mut().context("generation")?;
        let values = std::collections::BTreeMap::from([(
            "previous_response_id".to_owned(),
            json!("default-id"),
        )]);
        crate::gateway::pipeline::context::apply_parameter_defaults(
            &mut prompt.params,
            values.iter(),
        );
        assert_eq!(
            prompt.params.extra.get("previous_response_id"),
            Some(&source)
        );
        assert!(
            !prompt
                .params
                .supplemental_extra
                .contains_key("previous_response_id")
        );
        assert_eq!(
            extra_value_for_protocol(
                &prompt.params,
                &ApiProtocol::Messages,
                "previous_response_id"
            ),
            None
        );
        assert_eq!(compatible(prompt, &ApiProtocol::Messages), source.is_null());
    }
    Ok(())
}

#[test]
fn controls_use_actual_target_precedence_without_foreign_default_leakage() -> Result<()> {
    for (source, supplemental, responses, messages) in [
        (Some(Value::Null), Some(json!("provider-id")), true, false),
        (Some(Value::Null), Some(Value::Null), true, true),
        (None, Some(json!("provider-id")), true, false),
        (
            Some(json!("source-id")),
            Some(json!("default-id")),
            true,
            false,
        ),
    ] {
        let mut body = json!({"model":"route","input":"next"});
        if let Some(source) = &source {
            body["previous_response_id"] = source.clone();
        }
        let mut input = crate::gateway::http::parse_input(&ApiProtocol::Responses, body)?;
        let prompt = input.generation_prompt_mut().context("generation")?;
        if let Some(supplemental) = supplemental {
            prompt
                .params
                .supplemental_extra
                .insert("previous_response_id".into(), supplemental);
        }
        validate_input(Some(prompt), true).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(compatible(prompt, &ApiProtocol::Responses), responses);
        assert_eq!(compatible(prompt, &ApiProtocol::Messages), messages);
        if let Some(source) = source {
            assert_eq!(
                extra_value_for_protocol(
                    &prompt.params,
                    &ApiProtocol::Responses,
                    "previous_response_id"
                ),
                Some(&source)
            );
        }
    }
    Ok(())
}

#[test]
fn opaque_values_do_not_activate_managed_decoding_or_cross_protocol_stripping() -> Result<()> {
    for value in [
        json!("brc_unknown"),
        json!("provider-id"),
        json!({"provider":"value"}),
        json!(17),
    ] {
        let input = crate::gateway::http::parse_input(
            &ApiProtocol::Responses,
            json!({"model":"route","input":"next","previous_response_id":value}),
        )?;
        let prompt = input.generation_prompt().context("generation")?;
        assert!(validate_input(Some(prompt), false).is_err());
        assert!(validate_input(Some(prompt), true).is_ok());
        assert!(compatible(prompt, &ApiProtocol::Responses));
        assert!(!compatible(prompt, &ApiProtocol::Messages));
        assert!(!compatible(prompt, &ApiProtocol::ChatCompletions));
    }
    Ok(())
}

#[test]
fn messages_context_and_supplemental_protocol_ownership_are_distinct() -> Result<()> {
    let mut input = crate::gateway::http::parse_input(
        &ApiProtocol::Messages,
        json!({"model":"route","max_tokens":17,"messages":[],
            "context_management":{"edits":[{"type":"clear_tool_uses_20250919"}]}}),
    )?;
    let prompt = input.generation_prompt_mut().context("generation")?;
    assert!(validate_input(Some(prompt), true).is_ok());
    assert!(compatible(prompt, &ApiProtocol::Messages));
    assert!(!compatible(prompt, &ApiProtocol::Responses));
    prompt
        .params
        .supplemental_extra
        .insert("previous_response_id".into(), json!("other-wire"));
    assert!(!compatible(prompt, &ApiProtocol::Messages));
    prompt
        .params
        .supplemental_extra
        .insert("cachedContent".into(), json!("cachedContents/value"));
    assert!(validate_input(Some(prompt), true).is_err());
    Ok(())
}

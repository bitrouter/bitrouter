//! Execution is selected from an exact admitted provider/interface, never a
//! union of the model's other protocols or editorial recommendation metadata.

use crate::config::{ProviderConfig, ProviderModel};
use bitrouter_ai::types::{ApiProtocol, ModelOperation};
use bitrouter_registry::document::{ReaderPolicy, timestamp};
use registry_json as serde_json;
use serde_json::Value;
use std::sync::Arc;

/// Immutable public metadata captured when a concrete route is selected.
#[derive(Clone, Debug)]
pub struct SelectedInterface {
    /// Exact provider/native identity admitted with this interface.
    pub provider_id: String,
    pub provider_model_id: String,
    /// Source-declared endpoint to which ordinary rates apply.
    pub tariff_endpoint: Option<String>,
    /// Deployment profile explicitly verified by the local operator.
    pub verified_profile: Option<String>,
    /// Unknown billing and subscription never become free usage tariffs.
    pub billing: Option<String>,
    /// Exact interface contract, including its independent tariff.
    pub interface: Value,
    /// Explicit provider auth declaration. Contains names, never key values.
    pub auth: Value,
    /// Canonical identity, when verified; independent of recommendation status.
    pub canonical_id: Option<String>,
}

/// Content-free request requirements used before choosing an interface/offer.
#[derive(Clone, Debug, Default)]
pub struct RequestRequirements {
    effort: Option<bitrouter_ai::types::ReasoningEffort>,
    questions: Vec<&'static str>,
    inputs: Vec<&'static str>,
    outputs: Vec<&'static str>,
    explicit_inputs: bool,
}

impl RequestRequirements {
    pub(crate) fn generation(prompt: &bitrouter_ai::types::Prompt) -> Self {
        let text = prompt.system.is_some()
            || prompt
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|c| matches!(c, bitrouter_ai::types::Content::Text { .. }));
        Self {
            effort: prompt.params.reasoning_effort,
            inputs: if text { vec!["text"] } else { vec![] },
            outputs: if prompt.params.response_modalities.is_empty()
                || prompt
                    .params
                    .response_modalities
                    .contains(&bitrouter_ai::types::Modality::Text)
            {
                vec!["text"]
            } else {
                vec![]
            },
            ..Default::default()
        }
    }

    pub(crate) fn decisions(request: &bitrouter_ai::decisions::DecisionRequest) -> Self {
        let mut inputs = vec![];
        let mut text = false;
        request.visit_text(|_, _, _| text = true);
        if text {
            inputs.push("text");
        }
        if request.image_count() > 0 {
            inputs.push("image");
        }
        Self {
            questions: request
                .questions
                .iter()
                .map(|q| match q {
                    bitrouter_ai::decisions::DecisionQuestion::Predicate { .. } => "predicate",
                    bitrouter_ai::decisions::DecisionQuestion::Choice { .. } => "choice",
                    bitrouter_ai::decisions::DecisionQuestion::Score { .. } => "score",
                })
                .collect(),
            inputs,
            outputs: vec!["structured"],
            explicit_inputs: true,
            ..Default::default()
        }
    }

    fn matches(&self, interface: &Value) -> bool {
        let inputs = interface["input_modalities"].as_array();
        let outputs = interface["output_modalities"].as_array();
        self.inputs
            .iter()
            .all(|wanted| inputs.map_or(!self.explicit_inputs, |v| v.iter().any(|v| v == wanted)))
            && self
                .outputs
                .iter()
                .all(|wanted| outputs.is_none_or(|v| v.iter().any(|v| v == wanted)))
            && self.questions.iter().all(|wanted| {
                interface["question_types"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|v| v == wanted))
            })
            && if let Some(effort) = self.effort {
                serde_json::to_value(effort).is_ok_and(|name| {
                    interface["reasoning_effort"]["levels"]
                        .as_array()
                        .is_some_and(|v| v.contains(&name))
                })
            } else {
                interface
                    .pointer("/reasoning_effort/default")
                    .is_none_or(|default| {
                        serde_json::from_value::<bitrouter_ai::types::ReasoningEffort>(
                            default.clone(),
                        )
                        .is_ok()
                    })
            }
    }
}

pub(crate) fn supported(interface: &Value, policy: &ReaderPolicy) -> bool {
    policy.supports(interface)
        && compatibility_supported(interface)
        && matches!(
            interface["protocol_profile"].as_str(),
            None | Some("default")
        )
        && matches!(
            (
                interface["protocol"].as_str(),
                interface["operation"].as_str()
            ),
            (
                Some("chat_completions" | "messages" | "responses"),
                Some("generation")
            ) | (Some("decisions"), Some("decisions"))
        )
}
fn compatibility_supported(interface: &Value) -> bool {
    let Some(compat) = interface.get("compatibility") else {
        return true;
    };
    compat.as_object().is_some_and(|object| {
        object.keys().all(|key| {
            key == "extensions"
                || (interface["protocol"] == "chat_completions"
                    && matches!(
                        key.as_str(),
                        "token_limit_field"
                            | "supports_store"
                            | "supports_stream_options"
                            | "google_extensions"
                    ))
        })
    })
}
fn interface_capabilities(interface: &Value) -> Vec<bitrouter_ai::types::Capability> {
    use bitrouter_ai::types::Capability;
    let mut values = interface["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect::<Vec<_>>();
    for (field, name, capability) in [
        ("input_modalities", "image", Capability::ImageInput),
        ("input_modalities", "audio", Capability::AudioInput),
        ("input_modalities", "video", Capability::VideoInput),
        ("input_modalities", "file", Capability::FileInput),
        ("output_modalities", "image", Capability::ImageOutput),
        ("output_modalities", "audio", Capability::AudioOutput),
    ] {
        if interface[field]
            .as_array()
            .is_some_and(|v| !v.iter().any(|v| v == name))
        {
            values.retain(|v| *v != capability);
            continue;
        }
        if interface[field]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == name))
            && !values.contains(&capability)
        {
            values.push(capability);
        }
    }
    values
}
fn active(record: &Value) -> bool {
    record["status"] == "active"
        && ReaderPolicy::default().supports(record)
        && record
            .get("available_until")
            .and_then(Value::as_str)
            .and_then(timestamp)
            .is_none_or(|t| t > chrono::Utc::now())
}

/// Materialize the exact selected interface into existing routing primitives.
/// Unknown triples never fall back to a host-inferred protocol.
pub(crate) fn select(
    provider: &ProviderConfig,
    model: &str,
    native_only: bool,
    prefs: &crate::gateway::pipeline::routing::RoutingPrefs,
) -> Option<(ProviderConfig, Arc<SelectedInterface>)> {
    let raw = provider.registry.as_ref()?;
    if !active(raw) {
        return None;
    }
    let canonical_match = raw["models"]
        .as_array()?
        .iter()
        .any(|m| m["canonical_id"] == model);
    let mut offers = raw["models"]
        .as_array()?
        .iter()
        .filter(|m| {
            active(m)
                && if native_only {
                    m["provider_model_id"] == model
                } else if canonical_match {
                    m["canonical_id"] == model
                } else {
                    m["provider_model_id"] == model
                }
        })
        .collect::<Vec<_>>();
    offers.sort_by_key(|m| m["provider_model_id"].as_str());
    for offer in offers {
        let policy = ReaderPolicy::default();
        let mut interfaces = offer["interfaces"]
            .as_array()?
            .iter()
            .filter(|i| {
                supported(i, &policy)
                    && prefs
                        .require_capabilities
                        .iter()
                        .all(|c| interface_capabilities(i).contains(c))
                    && prefs
                        .registry_requirements
                        .as_ref()
                        .is_none_or(|r| r.matches(i))
                    && i["operation"].as_str()
                        == Some(if prefs.operation == ModelOperation::Decisions {
                            "decisions"
                        } else {
                            "generation"
                        })
            })
            .collect::<Vec<_>>();
        if let Some(inbound) = &prefs.inbound_protocol {
            interfaces.sort_by_key(|i| i["protocol"] != inbound.as_str());
        }
        for interface in interfaces {
            let Ok(protocol) = serde_json::from_value::<ApiProtocol>(interface["protocol"].clone())
            else {
                continue;
            };
            let base = provider
                .endpoint_for(&protocol)
                .or_else(|| {
                    (!provider.api_base.is_empty()
                        && raw["api_base"].as_str() != Some(provider.api_base.as_str()))
                    .then_some(provider.api_base.as_str())
                })
                .or_else(|| interface["base_url"].as_str())
                .unwrap_or(&provider.api_base);
            if base.is_empty() {
                continue;
            }
            let mut projected = provider.clone();
            projected.registry = None;
            projected.api_base = base.into();
            projected.protocol_endpoints.clear();
            let mut model_config=serde_json::from_value::<ProviderModel>(serde_json::json!({"id":offer["provider_model_id"],"provider_model_id":offer["provider_model_id"],"api_protocol":[protocol],"capabilities":[],"token_limits":{}})).ok()?;
            model_config.capabilities = interface_capabilities(interface);
            if let Some(limits) = interface.get("limits") {
                let known = ["max_input_tokens", "max_output_tokens", "context_window"]
                    .into_iter()
                    .filter_map(|key| {
                        bitrouter_registry::document::integer(&limits[key])
                            .map(|n| (key.to_owned(), serde_json::json!(n)))
                    })
                    .collect::<serde_json::Map<_, _>>();
                let Ok(limits) = serde_json::from_value(Value::Object(known)) else {
                    continue;
                };
                model_config.token_limits = limits;
            }
            if let Some(compat) = interface.get("compatibility") {
                let mut known = compat.clone();
                known.as_object_mut()?.remove("extensions");
                let Ok(compatibility) =
                    serde_json::from_value(serde_json::json!({"chat_completions":known}))
                else {
                    continue;
                };
                model_config.compatibility = compatibility;
            }
            if let Some(effort) = interface.get("reasoning_effort") {
                let levels = effort["levels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect::<Vec<bitrouter_ai::types::ReasoningEffort>>();
                let default = effort
                    .get("default")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                model_config.reasoning_effort = (!levels.is_empty())
                    .then_some(bitrouter_ai::types::ReasoningEffortConfig { levels, default });
            }
            projected.models = vec![model_config];
            return Some((
                projected,
                Arc::new(SelectedInterface {
                    provider_id: raw["name"].as_str()?.into(),
                    provider_model_id: offer["provider_model_id"].as_str()?.into(),
                    tariff_endpoint: interface["base_url"]
                        .as_str()
                        .or_else(|| raw["api_base"].as_str())
                        .map(str::to_owned),
                    verified_profile: provider.registry_endpoint_profile.clone(),
                    billing: raw["billing"].as_str().map(str::to_owned),
                    interface: interface.clone(),
                    auth: raw["auth"].clone(),
                    canonical_id: offer["canonical_id"].as_str().map(str::to_owned),
                }),
            ));
        }
    }
    None
}

/// The default automatic model pool is the current curated/available subset.
/// Explicit routes use the whole admitted provider inventory instead.
#[cfg(any(feature = "core", test))]
pub(crate) fn recommended(config: &crate::config::Config, id: &str) -> bool {
    config.registry_models.as_ref().is_none_or(|models| {
        models.iter().any(|m| {
            m["id"] == id
                && ReaderPolicy::default().supports(m)
                && m.pointer("/curation/expires_at")
                    .and_then(Value::as_str)
                    .and_then(timestamp)
                    .is_none_or(|t| t > chrono::Utc::now())
        })
    })
}

/// Explicit static authentication uses the frozen selected contract. It never
/// reuses a stale provider handler after a registry update changes auth kind.
pub(crate) fn authenticate_static(
    selected: &SelectedInterface,
    target: &crate::gateway::pipeline::types::RoutingTarget,
    mut request: reqwest::Request,
) -> crate::gateway::model::error::Result<crate::gateway::providers::auth::AppliedAuth> {
    use crate::gateway::model::error::ModelError;
    use crate::gateway::providers::auth::{AppliedAuth, CredentialAuthority};
    let invalid = || {
        ModelError::Configuration{message:"registry authentication contract cannot be applied; upgrade BitRouter or configure a supported provider".into()}
    };
    request.headers_mut().remove(http::header::AUTHORIZATION);
    request.headers_mut().remove("x-api-key");
    let key = target.effective_api_key();
    match selected.auth["kind"].as_str() {
        Some("none") => {}
        Some("bearer") => {
            if key.is_empty() {
                return Err(invalid());
            }
            let mut value =
                http::HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| invalid())?;
            value.set_sensitive(true);
            request
                .headers_mut()
                .insert(http::header::AUTHORIZATION, value);
        }
        Some("header") => {
            if key.is_empty() {
                return Err(invalid());
            }
            let name = http::HeaderName::from_bytes(
                selected.auth["header"]
                    .as_str()
                    .ok_or_else(invalid)?
                    .as_bytes(),
            )
            .map_err(|_| invalid())?;
            let mut value = http::HeaderValue::from_str(key).map_err(|_| invalid())?;
            value.set_sensitive(true);
            request.headers_mut().insert(name, value);
        }
        _ => return Err(invalid()),
    }
    for (name, value) in selected.auth["extra_headers"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        let value = http::HeaderValue::from_str(value.as_str().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        request.headers_mut().insert(name, value);
    }
    Ok(if selected.auth["kind"] == "none" {
        AppliedAuth::unproven(request)
    } else {
        AppliedAuth::proven(
            request,
            CredentialAuthority::derive("static-transport-credential", key),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, routing_table::resolve_route_chain};
    use crate::gateway::pipeline::routing::RoutingPrefs;
    use registry_json as serde_json;
    use serde_json::json;
    fn config(raw: Value) -> anyhow::Result<Config> {
        let data = super::super::decode::from_slice(&serde_json::to_vec(&raw)?)?;
        let mut config = Config::default();
        crate::providers::registry::apply::apply_registry(&mut config, &data);
        Ok(config)
    }

    #[test]
    fn requested_effort_selects_another_verified_interface_before_dispatch() -> anyhow::Result<()> {
        let mut raw = super::super::tests::document("native");
        raw["data"]["providers"][0]["models"][0]["interfaces"] = json!([
            {"protocol":"chat_completions","operation":"generation","capabilities":["reasoning"],"reasoning_effort":{"levels":["low"]}},
            {"protocol":"responses","operation":"generation","capabilities":["reasoning"],"reasoning_effort":{"levels":["high"]}}
        ]);
        let config = config(raw)?;
        let prompt: bitrouter_ai::types::Prompt = ::serde_json::from_value(
            ::serde_json::json!({"model":"org/model","messages":[],"tools":[],"params":{"reasoning_effort":"high"},"stream":false}),
        )?;
        let prefs = RoutingPrefs {
            inbound_protocol: Some(ApiProtocol::ChatCompletions),
            require_capabilities: crate::gateway::model::capabilities::required_capabilities(
                &prompt,
            ),
            registry_requirements: Some(RequestRequirements::generation(&prompt)),
            ..Default::default()
        };
        let chain = resolve_route_chain(&config, "org/model", &prefs)?;
        assert_eq!(chain[0].api_protocol, ApiProtocol::Responses);
        validate_generation(&chain[0], &prompt)?;
        Ok(())
    }

    #[test]
    fn decision_requirements_select_another_native_offer_with_the_same_canonical_id()
    -> anyhow::Result<()> {
        let mut raw = super::super::tests::document("a-predicate");
        let first = &mut raw["data"]["providers"][0]["models"][0];
        first["interfaces"] = json!([{"protocol":"decisions","operation":"decisions","question_types":["predicate"],"input_modalities":["text"]}]);
        let mut second = first.clone();
        second["provider_model_id"] = json!("b-score");
        second["interfaces"] = json!([{"protocol":"decisions","operation":"decisions","question_types":["score"],"input_modalities":["text","image"]}]);
        raw["data"]["providers"][0]["models"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("no offers"))?
            .push(second);
        let config = config(raw)?;
        let request: bitrouter_ai::decisions::DecisionRequest = ::serde_json::from_value(
            ::serde_json::json!({"model":"org/model","input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}],"questions":[{"type":"score","instructions":"rate evidence","levels":[{"label":"low"},{"label":"high"}]}]}),
        )?;
        let prefs = RoutingPrefs {
            operation: ModelOperation::Decisions,
            registry_requirements: Some(RequestRequirements::decisions(&request)),
            ..Default::default()
        };
        let chain = resolve_route_chain(&config, "org/model", &prefs)?;
        assert_eq!(chain[0].service_id, "b-score");
        validate_decisions(&chain[0], &request)?;
        Ok(())
    }
    #[test]
    fn native_wire_selection_never_unions_capabilities_or_limits() -> anyhow::Result<()> {
        let raw = super::super::tests::document("Native/A%雪");
        let config = config(raw)?;
        for (protocol, limit, tools) in [
            (ApiProtocol::Messages, 128, true),
            (ApiProtocol::ChatCompletions, 32, false),
        ] {
            let prefs = RoutingPrefs {
                inbound_protocol: Some(protocol.clone()),
                ..Default::default()
            };
            let targets = resolve_route_chain(&config, "org/model", &prefs)?;
            let target = targets.first().ok_or_else(|| anyhow::anyhow!("no route"))?;
            assert_eq!(target.api_protocol, protocol);
            assert_eq!(target.service_id, "Native/A%雪");
            assert_eq!(
                target.model_constraints.token_limits.max_output_tokens,
                Some(limit)
            );
            assert_eq!(
                target
                    .model_constraints
                    .capabilities
                    .contains(&bitrouter_ai::types::Capability::Tools),
                tools
            );
        }
        let encoded = bitrouter_registry::selector::encode("fixture", "Native/A%雪")?;
        assert_eq!(
            resolve_route_chain(&config, &encoded, &RoutingPrefs::default())?[0].service_id,
            "Native/A%雪"
        );
        assert!(
            resolve_route_chain(
                &config,
                "fixture:~native/Native%ZZ",
                &RoutingPrefs::default()
            )
            .is_err()
        );
        assert!(
            resolve_route_chain(&config, "fixture:~native/missing", &RoutingPrefs::default())?
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn unknown_and_expired_offers_skip_without_hiding_supported_siblings() -> anyhow::Result<()> {
        let mut raw = super::super::tests::document("z-supported");
        let mut unknown = raw["data"]["providers"][0]["models"][0].clone();
        unknown["provider_model_id"] = json!("a-future");
        unknown["interfaces"] = json!([{"protocol":"future","operation":"generation"}]);
        raw["data"]["providers"][0]["models"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("no inventory"))?
            .push(unknown);
        let config = config(raw.clone())?;
        assert_eq!(
            resolve_route_chain(&config, "org/model", &RoutingPrefs::default())?[0].service_id,
            "z-supported"
        );
        raw["data"]["providers"][0]["models"][0]["available_until"] = json!("2026-10-22T00:00:00Z");
        let expired = self::config(raw)?;
        assert!(resolve_route_chain(&expired, "org/model", &RoutingPrefs::default())?.is_empty());
        assert!(crate::config::routing_table::list_models_for(&expired).is_empty());
        Ok(())
    }
    #[test]
    fn native_selector_disambiguates_canonical_collisions_and_noncurated_models()
    -> anyhow::Result<()> {
        let mut raw = super::super::tests::document("z-bound");
        let mut collision = raw["data"]["providers"][0]["models"][0].clone();
        collision["provider_model_id"] = json!("org/model");
        collision["canonical_id"] = json!("org/other");
        raw["data"]["providers"][0]["models"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("no inventory"))?
            .push(collision);
        let config = config(raw)?;
        assert!(!recommended(&config, "org/model"));
        assert_eq!(
            resolve_route_chain(&config, "org/model", &RoutingPrefs::default())?[0].service_id,
            "z-bound"
        );
        let native = bitrouter_registry::selector::encode("fixture", "org/model")?;
        assert_eq!(
            resolve_route_chain(&config, &native, &RoutingPrefs::default())?[0]
                .registry
                .as_ref()
                .and_then(|m| m.canonical_id.as_deref()),
            Some("org/other")
        );
        Ok(())
    }

    #[test]
    fn request_requirements_choose_one_verified_interface_and_keep_effort_restrictions()
    -> anyhow::Result<()> {
        use bitrouter_ai::types::{Capability, ReasoningEffort};
        let mut raw = super::super::tests::document("native");
        let interfaces = &mut raw["data"]["providers"][0]["models"][0]["interfaces"];
        interfaces[0]["capabilities"] = json!(["tools", "reasoning"]);
        interfaces[0]["reasoning_effort"] =
            json!({"levels":["low","future"],"description":"extension"});
        interfaces[1]["compatibility"] =
            json!({"token_limit_field":"max_completion_tokens","extensions":{"future":true}});
        let config = config(raw.clone())?;
        let prefs = RoutingPrefs {
            inbound_protocol: Some(ApiProtocol::ChatCompletions),
            require_capabilities: vec![Capability::Tools],
            ..Default::default()
        };
        let target = resolve_route_chain(&config, "org/model", &prefs)?.remove(0);
        assert_eq!(target.api_protocol, ApiProtocol::Messages);
        let mut prompt: bitrouter_ai::types::Prompt = serde_json::from_value(
            json!({"model":"org/model","messages":[],"tools":[],"params":{},"stream":false}),
        )?;
        prompt.params.reasoning_effort = Some(ReasoningEffort::Low);
        validate_generation(&target, &prompt)?;
        prompt.params.reasoning_effort = Some(ReasoningEffort::High);
        assert!(validate_generation(&target, &prompt).is_err());
        let chat = resolve_route_chain(
            &config,
            "org/model",
            &RoutingPrefs {
                inbound_protocol: Some(ApiProtocol::ChatCompletions),
                ..Default::default()
            },
        )?
        .remove(0);
        assert_eq!(
            chat.chat_token_limit_field,
            Some(bitrouter_ai::types::ChatTokenLimitField::MaxCompletionTokens)
        );
        raw["data"]["providers"][0]["models"][0]["interfaces"][1]["compatibility"] =
            json!({"token_limit_field":"future"});
        assert_eq!(
            resolve_route_chain(
                &self::config(raw)?,
                "org/model",
                &RoutingPrefs {
                    inbound_protocol: Some(ApiProtocol::ChatCompletions),
                    ..Default::default()
                }
            )?[0]
                .api_protocol,
            ApiProtocol::Messages
        );
        Ok(())
    }

    #[test]
    fn decisions_require_verified_question_types_and_modalities() -> anyhow::Result<()> {
        let mut raw = super::super::tests::document("decision");
        raw["data"]["providers"][0]["models"][0]["interfaces"] = json!([{"protocol":"decisions","operation":"decisions","question_types":["predicate","choice","score"],"input_modalities":["text"]}]);
        let config = config(raw)?;
        let mut target = resolve_route_chain(
            &config,
            "org/model",
            &RoutingPrefs {
                operation: ModelOperation::Decisions,
                ..Default::default()
            },
        )?
        .remove(0);
        let mut request: bitrouter_ai::decisions::DecisionRequest = serde_json::from_value(
            json!({"model":"org/model","input":"evidence","questions":[{"type":"predicate","instructions":"is valid?"}]}),
        )?;
        validate_decisions(&target, &request)?;
        request.input = serde_json::from_value(
            json!([{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}]),
        )?;
        assert!(validate_decisions(&target, &request).is_err());
        let selected = Arc::make_mut(
            target
                .registry
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no metadata"))?,
        );
        selected.interface["input_modalities"] = json!(["text", "image"]);
        validate_decisions(&target, &request)?;
        Arc::make_mut(
            target
                .registry
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no metadata"))?,
        )
        .interface["question_types"] = json!([]);
        assert!(validate_decisions(&target, &request).is_err());
        Ok(())
    }

    #[test]
    fn selected_interface_cannot_authorize_a_different_model_provider_or_protocol()
    -> anyhow::Result<()> {
        let config = config(super::super::tests::document("native"))?;
        let original = resolve_route_chain(&config, "org/model", &RoutingPrefs::default())?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no route"))?;
        validate_target(&original)?;
        let mut changed = original.clone();
        changed.service_id = "another-model".into();
        assert!(validate_target(&changed).is_err());
        changed = original.clone();
        changed.provider_name = "another-provider".into();
        assert!(validate_target(&changed).is_err());
        changed = original;
        changed.api_protocol = ApiProtocol::Decisions;
        assert!(validate_target(&changed).is_err());
        Ok(())
    }

    #[test]
    fn exact_integral_json_spellings_project_without_losing_the_raw_document() -> anyhow::Result<()>
    {
        let mut raw = super::super::tests::document("native");
        raw["data"]["providers"][0]["models"][0]["interfaces"][0]["limits"]["max_output_tokens"] =
            bitrouter_registry::document::parse_json(b"1e3")?;
        raw["data"]["models"] = serde_json::json!([{"id":"org/model","max_input_tokens":bitrouter_registry::document::parse_json(b"1.0")?,"curation":{"reason":"fixture","source_url":"https://fixture.example/list","checked_at":"2026-10-22T00:00:00Z"}}]);
        let projected = super::super::decode::from_slice(&serde_json::to_vec(&raw)?)?;
        assert_eq!(projected.canonical[0].max_input_tokens, Some(1));
        assert_eq!(projected.document.as_ref(), Some(&raw));
        let mut config = Config::default();
        crate::providers::registry::apply::apply_registry(&mut config, &projected);
        assert_eq!(
            resolve_route_chain(&config, "org/model", &RoutingPrefs::default())?[0]
                .model_constraints
                .token_limits
                .max_output_tokens,
            Some(1000)
        );
        Ok(())
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use registry_json as serde_json;
    use serde_json::json;
    #[test]
    fn selected_auth_kind_never_inherits_a_previous_scheme_or_inference_header()
    -> anyhow::Result<()> {
        let raw = super::super::tests::document("native");
        let data = super::super::decode::from_slice(&serde_json::to_vec(&raw)?)?;
        let mut config = crate::config::Config::default();
        crate::providers::registry::apply::apply_registry(&mut config, &data);
        let mut target = crate::config::routing_table::resolve_route_chain(
            &config,
            "org/model",
            &crate::gateway::pipeline::routing::RoutingPrefs::default(),
        )?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("route absent"))?;
        target.api_key = "private-key".into();
        let mut selected = target
            .registry
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no selected interface"))?
            .clone();
        for auth in [
            json!({"kind":"none","extra_headers":{"api-version":"fixed"}}),
            json!({"kind":"bearer","env":"DECLARED_KEY","extra_headers":{"api-version":"fixed"}}),
            json!({"kind":"header","env":"DECLARED_KEY","header":"x-custom-key","extra_headers":{"api-version":"fixed"}}),
        ] {
            selected.auth = auth.clone();
            let mut request = reqwest::Request::new(
                http::Method::POST,
                reqwest::Url::parse("https://provider.example/v1/messages")?,
            );
            request.headers_mut().insert(
                "authorization",
                http::HeaderValue::from_static("Bearer inherited"),
            );
            request
                .headers_mut()
                .insert("x-api-key", http::HeaderValue::from_static("inherited"));
            let applied = authenticate_static(&selected, &target, request)?.into_request();
            assert!(!applied.headers().contains_key("x-api-key"));
            assert_eq!(
                applied.headers().get("api-version"),
                Some(&http::HeaderValue::from_static("fixed"))
            );
            assert_eq!(
                applied.headers().get("authorization").is_some(),
                auth["kind"] == "bearer"
            );
            assert_eq!(
                applied.headers().get("x-custom-key").is_some(),
                auth["kind"] == "header"
            );
        }
        Ok(())
    }
}

/// Final admission also covers direct/custom execution paths that did not use
/// a preference-aware config routing table. Unknown requirements fail normally.
pub(crate) fn validate_generation(
    target: &crate::gateway::pipeline::types::RoutingTarget,
    prompt: &bitrouter_ai::types::Prompt,
) -> crate::gateway::error::Result<()> {
    validate_target(target)?;
    let Some(selected) = &target.registry else {
        return Ok(());
    };
    let invalid = || {
        crate::gateway::error::BitrouterError::bad_request(
            "registry.unsupported_interface: selected interface cannot prove the requested capabilities, modalities, or reasoning effort; select a verified interface or upgrade BitRouter",
        )
    };
    let required = crate::gateway::model::capabilities::required_capabilities(prompt);
    let capabilities = interface_capabilities(&selected.interface);
    if target.api_protocol.operation() != ModelOperation::Generation
        || !required.iter().all(|c| capabilities.contains(c))
    {
        return Err(invalid());
    }
    if !RequestRequirements::generation(prompt).matches(&selected.interface) {
        return Err(invalid());
    }
    Ok(())
}
pub(crate) fn validate_decisions(
    target: &crate::gateway::pipeline::types::RoutingTarget,
    request: &bitrouter_ai::decisions::DecisionRequest,
) -> crate::gateway::error::Result<()> {
    validate_target(target)?;
    let Some(selected) = &target.registry else {
        return Ok(());
    };
    // Native Decisions supports text/image evidence and typed questions:
    // https://developers.openai.com/api/docs/guides/decisions
    if target.api_protocol.operation() != ModelOperation::Decisions
        || !RequestRequirements::decisions(request).matches(&selected.interface)
    {
        return Err(crate::gateway::error::BitrouterError::bad_request(
            "registry.unsupported_interface: selected interface does not verify the requested decision question types or input modalities; select a verified interface or upgrade BitRouter",
        ));
    }
    Ok(())
}

/// Route hooks may rebuild targets, but cannot carry another model or codec's
/// authority along with an earlier registry interface.
pub(crate) fn validate_target(
    target: &crate::gateway::pipeline::types::RoutingTarget,
) -> crate::gateway::error::Result<()> {
    let Some(selected) = &target.registry else {
        return Ok(());
    };
    let operation = if target.api_protocol.operation() == ModelOperation::Decisions {
        "decisions"
    } else {
        "generation"
    };
    if selected.provider_id != target.provider_name
        || selected.provider_model_id != target.service_id
        || selected.interface["protocol"] != target.api_protocol.as_str()
        || selected.interface["operation"] != operation
        || !supported(&selected.interface, &ReaderPolicy::default())
    {
        return Err(crate::gateway::error::BitrouterError::bad_request(
            "registry route identity or interface changed after admission; resolve a supported route again or upgrade BitRouter",
        ));
    }
    Ok(())
}

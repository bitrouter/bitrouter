//! Native authority exists only in the explicit current artifact/compiler.
use super::*;
use bitrouter_ai::tenancy::{admission::AdmissionProfileV1, ordered::GenerationProtocol};

fn native_route() -> Result<(manifest::Import, Artifact)> {
    let credential = reference("tenant", ResourceKind::Credential, "slot")?;
    let profile: ResourceRef = serde_json::from_value(json!({"owner_kind":"operator_pool",
        "owner_id":"pool","resource_kind":"policy","resource_id":"profile","revision_id":"v1"}))?;
    let profile_rules: AdmissionProfileV1 = serde_json::from_value(json!({"schema_version":1,
        "rate":null,"health":null,"fallback":"ordered_terminal_rejections_v1"}))?;
    let attachments = [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
        GenerationProtocol::GenerateContent,
    ]
    .into_iter()
    .map(|protocol| {
        json!({"inbound_protocol":protocol,"candidate_index":0,
            "credential":credential,"profile":profile})
    })
    .collect::<Vec<_>>();
    let import = serde_json::from_value(
        json!({"reference":reference("tenant",ResourceKind::Route,"native")?,
        "definition":{"kind":"native_profiled_route_v1","selector":"fixture:base","policy":null,
            "routing":{"providers":{"fixture":{"api_base":"https://paired.invalid/v1",
                "api_protocol":[{"*":["ordered_generate_content","chat_completions","messages","responses"]}],
                "models":[{"id":"base"}]}}},
            "credentials":[{"provider":"fixture","account":"default","credential":credential}],
            "attachments":attachments}}),
    )?;
    Ok((
        import,
        Artifact {
            reference: profile,
            definition: Definition::AdmissionProfile {
                profile: profile_rules,
            },
            envelope: None,
        },
    ))
}

#[test]
fn native_generate_content_compiler_retains_exact_four_roots_and_new_format() -> Result<()> {
    let (import, profile) = native_route()?;
    import.validate()?;
    let prepared = compiler::prepare_route(&import, None)?;
    let native = prepared
        .chains
        .iter()
        .find(|chain| {
            matches!(&chain.inbound,bitrouter_ai::types::ApiProtocol::Custom(name)
        if name==bitrouter_ai::tenancy::generate_content::PROTOCOL)
        })
        .context("native root")?;
    assert_eq!(native.targets.len(), 1);
    assert_eq!(
        native.targets[0].target["protocol"],
        "ordered_generate_content"
    );
    let artifact = Artifact {
        reference: import.reference.clone(),
        definition: import.definition,
        envelope: Some(prepared),
    };
    let resources = Resources::with_key("installation".try_into()?, [7; 32])?;
    let stored = storage::seal(&resources, &artifact)?;
    assert_eq!(stored.compiler_version, NATIVE_COMPILER_VERSION);
    let restored = storage::authenticate(&resources, stored)?;
    assert!(restored.definition.is_native_route());
    let artifacts = vec![profile, artifact];
    admission::validate_graph(&artifacts)?;
    let mut envelope = Envelope {
        format: 6,
        document: document(vec![import.reference], None)?,
        routes: Vec::new(),
        artifacts,
    };
    envelope.validate_format()?;
    for old in [1, 2, 3, 4, 5] {
        envelope.format = old;
        assert!(envelope.validate_format().is_err());
    }
    Ok(())
}

#[test]
fn native_generate_content_compiler_never_widens_legacy_or_arbitrary_custom_protocols() -> Result<()>
{
    let (import, _) = native_route()?;
    let Definition::NativeProfiledRouteV1 { routing, .. } = &import.definition else {
        anyhow::bail!("fixture kind")
    };
    assert!(compiler::routing_config(routing).is_err());
    let mut old = serde_json::to_value(&import)?;
    old["definition"]["kind"] = "route".into();
    old["definition"]
        .as_object_mut()
        .context("definition")?
        .remove("attachments");
    let old: manifest::Import = serde_json::from_value(old)?;
    old.validate()?;
    assert!(compiler::prepare_route(&old, None).is_err());
    for forbidden in ["generate_content", "unpaired_custom"] {
        let mut input = serde_json::to_value(&import)?;
        input["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
            json!([{"*":forbidden}]);
        let input: manifest::Import = serde_json::from_value(input)?;
        input.validate()?;
        assert!(compiler::prepare_route(&input, None).is_err());
    }
    Ok(())
}

#[test]
fn native_generate_content_compiler_selects_ordinary_fallback_before_bridge_filter() -> Result<()> {
    for model_override in [false, true] {
        let (mut import, _) = native_route()?;
        let Definition::NativeProfiledRouteV1 { routing, .. } = &mut import.definition else {
            anyhow::bail!("native fixture")
        };
        let provider = &mut routing["providers"]["fixture"];
        let protocols = json!(["ordered_generate_content", "chat_completions"]);
        if model_override {
            provider["models"][0]["api_protocol"] = protocols;
        } else {
            provider["api_protocol"] = json!([{"*":protocols}]);
        }
        provider["protocol_endpoints"] = json!({"chat_completions":"https://paired.invalid/chat", "ordered_generate_content":"https://paired.invalid/native"});
        let prepared = compiler::prepare_route(&import, None)?;
        assert_eq!(prepared.chains.len(), 4);
        for chain in prepared.chains {
            assert_eq!(chain.targets.len(), 1);
            let native = matches!(chain.inbound, bitrouter_ai::types::ApiProtocol::Custom(_));
            assert_eq!(
                chain.targets[0].target["protocol"],
                if native {
                    "ordered_generate_content"
                } else {
                    "chat_completions"
                }
            );
            assert_eq!(
                chain.targets[0].target["api_base"],
                if native {
                    "https://paired.invalid/native"
                } else {
                    "https://paired.invalid/chat"
                }
            );
        }
    }
    let (mut import, _) = native_route()?;
    let Definition::NativeProfiledRouteV1 {
        routing,
        attachments,
        ..
    } = &mut import.definition
    else {
        anyhow::bail!("native fixture")
    };
    routing["providers"]["fixture"]["api_protocol"] = json!([{"*":"ordered_generate_content"}]);
    attachments
        .retain(|attachment| attachment.inbound_protocol == GenerationProtocol::GenerateContent);
    let prepared = compiler::prepare_route(&import, None)?;
    assert_eq!(prepared.chains.len(), 1);
    assert!(
        matches!(&prepared.chains[0].inbound,bitrouter_ai::types::ApiProtocol::Custom(name) if name==bitrouter_ai::tenancy::generate_content::PROTOCOL)
    );
    Ok(())
}

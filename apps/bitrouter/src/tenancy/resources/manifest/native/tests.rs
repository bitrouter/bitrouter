//! Compatibility of the private historical codec after public writer removal.

use super::*;
use anyhow::Context;
use serde_json::{Value, json};

fn reference(kind: &str) -> Value {
    json!({"owner_kind":if kind=="policy"{"operator_pool"}else{"tenant"},"owner_id":if kind=="policy"{"pool"}else{"tenant"},"resource_kind":kind,"resource_id":kind,"revision_id":"revision"})
}

fn retained() -> Value {
    json!({"kind":"native_profiled_route_v1","routing":{"providers":{}},"selector":"provider/model","policy":null,
        "credentials":[{"provider":"provider","account":"default","credential":reference("credential")}],
        "attachments":[{"inbound_protocol":"generate_content","candidate_index":0,"credential":reference("credential"),"profile":reference("policy")}]})
}

#[test]
fn retained_native_artifact_round_trips_through_the_private_import_reader() -> Result<()> {
    let encoded = serde_json::to_string(&retained())?;
    let definition: NativeDefinition = serde_json::from_str(&encoded)?;
    definition.validate()?;
    ensure!(serde_json::to_string(&definition)? == encoded);
    let import: super::super::Definition = serde_json::from_str(&encoded)?;
    ensure!(serde_json::to_string(&import)? == encoded);
    Ok(())
}

#[test]
fn retained_native_artifacts_keep_exact_kinds_static_policy_and_required_null() -> Result<()> {
    for (pointer, value) in [
        ("/policy", reference("policy")),
        ("/attachments/0/inbound_protocol", json!("chat_completions")),
        ("/attachments/0/candidate_index", json!(256)),
        ("/attachments/0/credential/resource_kind", json!("route")),
        ("/attachments/0/profile/owner_kind", json!("tenant")),
    ] {
        let mut invalid = retained();
        *invalid.pointer_mut(pointer).context("fixture pointer")? = value;
        ensure!(
            serde_json::from_value::<NativeDefinition>(invalid)?
                .validate()
                .is_err(),
            "{pointer}"
        );
    }
    let mut missing = retained();
    missing
        .as_object_mut()
        .context("fixture object")?
        .remove("policy");
    ensure!(serde_json::from_value::<NativeDefinition>(missing).is_err());
    let duplicate = serde_json::to_string(&retained())?.replace(
        "\"candidate_index\":0",
        "\"candidate_index\":0,\"candidate_\\u0069ndex\":1",
    );
    ensure!(serde_json::from_str::<NativeDefinition>(&duplicate).is_err());
    Ok(())
}

#[test]
fn retained_native_routing_preserves_opaque_tokens_and_programmatic_depth_bound() -> Result<()> {
    let mut value = retained();
    value["routing"] = json!({"opaque":{"$serde_json::private::Number":"literal","sibling":[1.25,null]},"nested":[{"$serde_json::private::RawValue":"literal-raw"}]});
    let encoded = serde_json::to_string(&value)?;
    let mut definition: NativeDefinition = serde_json::from_str(&encoded)?;
    definition.validate()?;
    ensure!(serde_json::to_value(&definition)?["routing"] == value["routing"]);
    let NativeDefinition::NativeProfiledRouteV1 { routing, .. } = &mut definition;
    let mut deep = json!(0);
    for _ in 0..128 {
        deep = json!([deep]);
    }
    *routing = json!({"deep":deep});
    ensure!(definition.validate().is_err());
    Ok(())
}

#[test]
fn retained_native_routing_rejects_duplicate_keys_and_excessive_wire_depth() -> Result<()> {
    let encoded = serde_json::to_string(&retained())?;
    for routing in [
        r#"{"duplicate":1,"dupli\u0063ate":2}"#.to_owned(),
        format!("{{\"deep\":{}0{}}}", "[".repeat(128), "]".repeat(128)),
    ] {
        let invalid = encoded.replace("{\"providers\":{}}", &routing);
        ensure!(invalid != encoded, "routing substitution failed");
        ensure!(serde_json::from_str::<NativeDefinition>(&invalid).is_err());
    }
    Ok(())
}

#[test]
fn retained_native_ordinals_keep_exact_integral_json_spelling() -> Result<()> {
    let encoded = serde_json::to_string(&retained())?;
    for (token, accepted) in [
        ("0.0", true),
        ("0e0", true),
        ("2.55e2", true),
        ("2550e-1", true),
        ("-1", false),
        ("1e-100", false),
        ("1e100", false),
        ("65536", false),
        ("255.0000000000000000000001", false),
    ] {
        let input = encoded.replace(
            "\"candidate_index\":0",
            &format!("\"candidate_index\":{token}"),
        );
        ensure!(input != encoded, "ordinal substitution failed");
        let decoded = serde_json::from_str::<NativeDefinition>(&input);
        ensure!(
            decoded.is_ok() == accepted,
            "ordinal token acceptance changed"
        );
        if let Ok(definition) = decoded {
            definition.validate()?;
        }
    }
    Ok(())
}

#[test]
fn retained_native_selector_limit_counts_utf8_bytes() -> Result<()> {
    for (count, accepted) in [(256, true), (257, false)] {
        let mut value = retained();
        value["selector"] = json!("é".repeat(count));
        ensure!(
            serde_json::from_value::<NativeDefinition>(value)?
                .validate()
                .is_ok()
                == accepted
        );
    }
    Ok(())
}

use super::*;
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

fn selection(count: u16) -> Value {
    json!({
        "schema_version": 1,
        "route": {
            "owner_kind": "tenant", "owner_id": "tenant-a", "resource_kind": "route",
            "resource_id": "route-a", "revision_id": "route-rev-a"
        },
        "candidate_indices": (0..count).rev().collect::<Vec<_>>()
    })
}

fn ordered_headers(json: &[u8]) -> Result<HeaderMap> {
    let mut headers = headers()?;
    headers.insert("bitrouter-tenancy-version", "2".parse()?);
    headers.insert(SELECTION_HEADER, URL_SAFE_NO_PAD.encode(json).parse()?);
    Ok(headers)
}

#[test]
fn ordered_wire_binding_preserves_order_and_rejects_malformed_or_downgraded_selection() -> Result<()>
{
    for count in [1, 8, 9, 64] {
        let body = serde_json::to_vec(&selection(count))?;
        let valid = ordered_headers(&body)?;
        let binding = dispatch_binding(&valid).map_err(|e| anyhow::anyhow!("{:?}", e.0.code))?;
        ensure!(
            binding.selection.context("selection")?.candidate_indices
                == (0..count).rev().collect::<Vec<_>>()
        );
        for name in valid.keys() {
            let mut removed = valid.clone();
            removed.remove(name);
            ensure!(dispatch_binding(&removed).is_err());
            let mut duplicate = valid.clone();
            duplicate.append(name.clone(), valid[name].clone());
            ensure!(dispatch_binding(&duplicate).is_err());
        }
        let mut downgraded = valid;
        downgraded.insert("bitrouter-tenancy-version", "1".parse()?);
        ensure!(dispatch_binding(&downgraded).is_err());
    }
    let mut malformed = vec![selection(0), selection(65)];
    for indices in [
        json!([1, 1]),
        json!([-1]),
        json!([256]),
        json!([1.5]),
        json!(["1"]),
    ] {
        let mut changed = selection(1);
        changed["candidate_indices"] = indices;
        malformed.push(changed);
    }
    let mut unknown = selection(1);
    unknown["api_base"] = "https://injected.invalid".into();
    malformed.push(unknown);
    for changed in malformed {
        ensure!(
            dispatch_binding(&ordered_headers(&serde_json::to_vec(&changed)?)?).is_err(),
            "accepted invalid selection: {changed}"
        );
    }
    let encoded_json = serde_json::to_string(&selection(1))?;
    let duplicate_key = encoded_json.replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    ensure!(dispatch_binding(&ordered_headers(duplicate_key.as_bytes())?).is_err());
    for value in [
        "".to_owned(),
        "e30=".to_owned(),
        "e30,e30".to_owned(),
        "A".repeat(4097),
        "!!!".to_owned(),
    ] {
        let mut changed = ordered_headers(encoded_json.as_bytes())?;
        changed.insert(SELECTION_HEADER, value.parse()?);
        ensure!(dispatch_binding(&changed).is_err());
    }
    Ok(())
}

#[test]
fn ordered_requests_cannot_use_provider_or_gateway_continuations() -> Result<()> {
    let mut prompt: bitrouter_ai::types::Prompt = serde_json::from_value(json!({
        "model":"route-a", "messages":[], "stream":false
    }))?;
    ensure!(ordered_prompt(Some(&prompt)).is_ok());
    ensure!(ordered_prompt(None).is_err());
    for name in [
        "previous_response_id",
        "conversation",
        "context_management",
        "cachedContent",
    ] {
        for value in [json!("resp-private"), json!({}), json!(false), json!([])] {
            prompt.params.extra.insert(name.into(), value);
            ensure!(ordered_prompt(Some(&prompt)).is_err());
        }
        prompt.params.extra.insert(name.into(), Value::Null);
        ensure!(ordered_prompt(Some(&prompt)).is_ok());
        prompt.params.extra.remove(name);
        prompt
            .params
            .supplemental_extra
            .insert(name.into(), json!({}));
        ensure!(ordered_prompt(Some(&prompt)).is_err());
        prompt
            .params
            .supplemental_extra
            .insert(name.into(), Value::Null);
        ensure!(ordered_prompt(Some(&prompt)).is_ok());
        prompt.params.supplemental_extra.remove(name);
    }
    Ok(())
}

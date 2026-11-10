//! Explicit source proposals for human review, bound to the sync identity snapshot.

use super::*;

/// Remove only missing references from an otherwise valid source candidate.
/// Native inventory and the historical identity ledger remain available for review.
pub(super) fn stage(sources: &mut Sources, snapshot: &Value) -> Result<Vec<Value>> {
    let ids = identity_ids(snapshot)?;
    let document = sources.document()?;
    let missing_recommendations = document
        .models()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .filter(|id| !ids.contains(*id))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut repairs = Vec::new();
    for (path, value) in &mut sources.files {
        match path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
        {
            Some("providers") => {
                let provider = value["name"].clone();
                for offer in value["models"].as_array_mut().context("provider models")? {
                    let Some(id) = offer["canonical_id"].as_str() else {
                        continue;
                    };
                    if ids.contains(id) {
                        continue;
                    }
                    repairs.push(json!({
                        "kind": "unbind_offer",
                        "provider": provider,
                        "provider_model_id": offer["provider_model_id"],
                        "canonical_id": id,
                        "reason": "canonical_id_not_found"
                    }));
                    offer
                        .as_object_mut()
                        .context("provider offer")?
                        .remove("canonical_id");
                }
            }
            Some("models") => {
                value
                    .as_array_mut()
                    .context("curated models")?
                    .retain(|model| {
                        if model["id"]
                            .as_str()
                            .is_some_and(|id| missing_recommendations.contains(id))
                        {
                            repairs.push(json!({
                                "kind": "remove_recommendation",
                                "canonical_id": model["id"],
                                "reason": "canonical_id_not_found"
                            }));
                            false
                        } else {
                            true
                        }
                    });
            }
            _ => {}
        }
    }
    Ok(repairs)
}

//! Content-associated evidence, independent of synchronization heartbeat times.

use super::*;

fn relative<'a>(sources: &'a Sources, path: &'a Path) -> Result<&'a Path> {
    Ok(path.strip_prefix(sources.manifest.parent().context("manifest parent")?)?)
}

fn prior_file<'a>(prior: &'a Sources, current: &Sources, path: &Path) -> Result<Option<&'a Value>> {
    Ok(prior.files.get(
        &prior
            .manifest
            .parent()
            .context("manifest parent")?
            .join(relative(current, path)?),
    ))
}

fn row<'a>(records: Option<&'a Value>, key: &str, id: &Value) -> Option<&'a Value> {
    records?.as_array()?.iter().find(|r| r.get(key) == Some(id))
}

fn without_time(value: &Value, field: &str) -> Value {
    let mut value = value.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove(field);
    }
    value
}

fn equal(a: &Value, b: &Value) -> Result<bool> {
    Ok(bitrouter_registry::reconcile::equivalent(a, b)?)
}

fn model_content(value: &Value) -> Value {
    let mut value = value.clone();
    normalize_model(&mut value);
    value
}

fn normalize_model(value: &mut Value) {
    for key in [
        "input_modalities",
        "output_modalities",
        "model_types",
        "capabilities",
        "requires",
    ] {
        if let Some(items) = value.get_mut(key).and_then(Value::as_array_mut) {
            items.sort_by_key(Value::to_string);
        }
    }
    for interface in value
        .get_mut("interfaces")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for key in [
            "input_modalities",
            "output_modalities",
            "capabilities",
            "requires",
        ] {
            if let Some(items) = interface.get_mut(key).and_then(Value::as_array_mut) {
                items.sort_by_key(Value::to_string);
            }
        }
    }
}

fn recommendation(value: &Value) -> Value {
    let mut value = model_content(value);
    if let Some(map) = value["curation"].as_object_mut() {
        map.remove("checked_at");
    }
    value
}

fn public_content(document: &Document) -> Value {
    let mut value = document.value().clone();
    if let Some(map) = value.as_object_mut() {
        map.remove("as_of");
    }
    for model in value["data"]["models"].as_array_mut().into_iter().flatten() {
        if let Some(map) = model["curation"].as_object_mut() {
            map.remove("checked_at");
        }
    }
    for provider in value["data"]["providers"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        for model in provider["models"].as_array_mut().into_iter().flatten() {
            if let Some(map) = model["availability"].as_object_mut() {
                map.remove("checked_at");
            }
        }
    }
    value
}

pub(super) fn reuse_deadlines(
    candidate: &mut Sources,
    accepted: &Sources,
    pending: Option<&Sources>,
) -> Result<()> {
    let Some(pending) = pending else {
        return Ok(());
    };
    let policy = candidate
        .manifest
        .parent()
        .context("parent")?
        .join("model-discovery.yaml");
    if prior_file(pending, candidate, &policy)? != Some(&candidate.files[&policy]) {
        return Ok(());
    }
    let paths = candidate
        .records("models")
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>();
    for path in paths {
        let old = prior_file(pending, candidate, &path)?.cloned();
        let accepted_records = prior_file(accepted, candidate, &path)?.cloned();
        for model in candidate
            .files
            .get_mut(&path)
            .context("models")?
            .as_array_mut()
            .context("models array")?
        {
            // Never replace an accepted deadline; current discovery independently
            // removes it on re-entry or confirms the same retention transition.
            if model.pointer("/curation/expires_at").is_none()
                && row(accepted_records.as_ref(), "id", &model["id"]).is_some()
                && let Some(deadline) = row(old.as_ref(), "id", &model["id"])
                    .and_then(|r| r.pointer("/curation/expires_at"))
            {
                model["curation"]["expires_at"] = deadline.clone();
            }
        }
    }
    Ok(())
}

pub(super) fn finalize(
    candidate: &mut Sources,
    accepted: &Sources,
    pending: Option<&Sources>,
    now: chrono::DateTime<Utc>,
) -> Result<()> {
    let paths = candidate
        .records("providers")
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>();
    for path in paths {
        let base = prior_file(accepted, candidate, &path)?.cloned();
        let previous = pending
            .map(|p| prior_file(p, candidate, &path))
            .transpose()?
            .flatten()
            .cloned();
        let provider = candidate.files.get_mut(&path).context("provider")?;
        let config = provider["auto_sync"].clone();
        for model in provider["models"].as_array_mut().context("models")? {
            let Some(owner) = model
                .pointer("/sync/owner")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            for old_provider in [base.as_ref(), previous.as_ref()].into_iter().flatten() {
                let Some(old) = row(
                    old_provider.get("models"),
                    "provider_model_id",
                    &model["provider_model_id"],
                ) else {
                    continue;
                };
                if old_provider["auto_sync"] == config
                    && old["sync"] == model["sync"]
                    && equal(
                        &bitrouter_registry::reconcile::assertion(old, &owner)?,
                        &bitrouter_registry::reconcile::assertion(model, &owner)?,
                    )?
                {
                    if let Some(time) = old.pointer("/availability/checked_at") {
                        model["availability"]["checked_at"] = time.clone();
                    }
                    if equal(&model_content(old), &model_content(model))? {
                        *model = old.clone();
                    }
                    break;
                }
            }
        }
    }
    let policy_path = candidate
        .manifest
        .parent()
        .context("parent")?
        .join("model-discovery.yaml");
    let pending_curation = match pending {
        Some(prior)
            if prior_file(prior, candidate, &policy_path)? == candidate.files.get(&policy_path) =>
        {
            Some(prior)
        }
        _ => None,
    };
    let paths = candidate
        .records("models")
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>();
    for path in paths {
        let base = prior_file(accepted, candidate, &path)?.cloned();
        let previous = pending_curation
            .map(|p| prior_file(p, candidate, &path))
            .transpose()?
            .flatten()
            .cloned();
        for model in candidate
            .files
            .get_mut(&path)
            .context("models")?
            .as_array_mut()
            .context("models array")?
        {
            let mut reused = false;
            for old in [
                row(base.as_ref(), "id", &model["id"]),
                row(previous.as_ref(), "id", &model["id"]),
            ]
            .into_iter()
            .flatten()
            {
                if equal(&recommendation(old), &recommendation(model))? {
                    *model = old.clone();
                    reused = true;
                    break;
                }
            }
            if !reused {
                model["curation"]["checked_at"] = json!(now);
            }
        }
    }
    let old_ledgers = [
        Some(&accepted.files[&accepted.identities]),
        pending.map(|p| &p.files[&p.identities]),
    ];
    for entry in candidate
        .files
        .get_mut(&candidate.identities)
        .context("identities")?
        .as_array_mut()
        .context("identity array")?
    {
        let old_entries = old_ledgers
            .into_iter()
            .flatten()
            .filter_map(|ledger| row(Some(ledger), "canonical_id", &entry["canonical_id"]))
            .collect::<Vec<_>>();
        for old in &old_entries {
            let mut a = without_time(entry, "observed_at");
            let mut b = without_time(old, "observed_at");
            a.as_object_mut().context("identity")?.remove("bindings");
            b.as_object_mut().context("identity")?.remove("bindings");
            if equal(&a, &b)? {
                entry["observed_at"] = old["observed_at"].clone();
                break;
            }
        }
        for binding in entry["bindings"].as_array_mut().into_iter().flatten() {
            for old in &old_entries {
                if let Some(prior) = old["bindings"].as_array().into_iter().flatten().find(|b| {
                    b["provider"] == binding["provider"]
                        && b["provider_model_id"] == binding["provider_model_id"]
                }) && equal(
                    &without_time(binding, "verified_at"),
                    &without_time(prior, "verified_at"),
                )? {
                    binding["verified_at"] = prior["verified_at"].clone();
                    break;
                }
            }
        }
    }
    // Preserve original number spellings and object layout for semantic no-ops.
    for (path, value) in &mut candidate.files {
        for old in [Some(accepted), pending].into_iter().flatten() {
            let relative = path.strip_prefix(candidate.manifest.parent().context("parent")?)?;
            if let Some(prior) = old
                .files
                .get(&old.manifest.parent().context("parent")?.join(relative))
                && equal(value, prior)?
            {
                *value = prior.clone();
                break;
            }
        }
    }
    let document = candidate.document()?;
    let base = accepted.document()?;
    let baseline = if equal(&public_content(&document), &public_content(&base))? {
        Some(&base)
    } else {
        None
    };
    let previous_document = pending.map(Sources::document).transpose()?;
    let eligible_pending = previous_document
        .as_ref()
        .filter(|old| old.as_of() >= base.as_of());
    // Evidence has already been selected. Whole-document reuse must not restore
    // ineligible evidence after an authority/configuration change.
    let chosen = match baseline {
        Some(base) => Some(base),
        None => {
            if let Some(old) = eligible_pending {
                let a = without_time(document.value(), "as_of");
                let b = without_time(old.value(), "as_of");
                if equal(&a, &b)? { Some(old) } else { None }
            } else {
                None
            }
        }
    };
    if let Some(old) = chosen {
        candidate
            .files
            .get_mut(&candidate.manifest)
            .context("manifest")?["as_of"] = old.value()["as_of"].clone();
    }
    let final_document = candidate.document()?;
    let as_of = final_document.as_of();
    for model in final_document.models() {
        if model
            .pointer("/curation/checked_at")
            .and_then(Value::as_str)
            .and_then(timestamp)
            .is_some_and(|t| t > as_of)
        {
            bail!("curation evidence exceeds publication evaluation time");
        }
    }
    for provider in final_document.providers() {
        for model in provider["models"].as_array().into_iter().flatten() {
            if model
                .pointer("/availability/checked_at")
                .and_then(Value::as_str)
                .and_then(timestamp)
                .is_some_and(|t| t > as_of)
            {
                bail!("availability evidence exceeds publication evaluation time");
            }
        }
    }
    for entry in candidate.files[&candidate.identities]
        .as_array()
        .into_iter()
        .flatten()
    {
        if entry["observed_at"]
            .as_str()
            .and_then(timestamp)
            .is_some_and(|t| t > now)
            || entry["bindings"].as_array().into_iter().flatten().any(|b| {
                b["verified_at"]
                    .as_str()
                    .and_then(timestamp)
                    .is_some_and(|t| t > now)
            })
        {
            bail!("identity evidence is in the future");
        }
    }
    if as_of > now {
        bail!("clock rollback: candidate time exceeds current evaluation");
    }
    Ok(())
}

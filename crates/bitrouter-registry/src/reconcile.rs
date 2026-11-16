//! Atomic, idempotent snapshot reconciliation with scoped ownership.

use registry_json as serde_json;
use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::document::{RegistryError, interface_key};
use crate::mapping::{Observation, ObservedModel, domain, read, tokens, write};

fn signed_number(value: &str) -> Result<num_rational::BigRational, RegistryError> {
    let unsigned = crate::pricing::decimal(value.strip_prefix('-').unwrap_or(value))?;
    Ok(if value.starts_with('-') {
        -unsigned
    } else {
        unsigned
    })
}

/// Compare JSON with exact signed numeric equality, preserving opaque array order.
///
/// # Errors
/// Numbers outside the document resource profile are rejected.
// Exact typed equality: unknown objects cannot collide with a numeric surrogate.
pub fn equivalent(a: &Value, b: &Value) -> Result<bool, RegistryError> {
    Ok(match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            signed_number(&a.to_string())? == signed_number(&b.to_string())?
        }
        (Value::Object(a), Value::Object(b)) => {
            if a.len() != b.len() {
                false
            } else {
                let mut same = true;
                for (k, v) in a {
                    if let Some(w) = b.get(k) {
                        same &= equivalent(v, w)?;
                    } else {
                        same = false;
                    }
                }
                same
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                false
            } else {
                let mut same = true;
                for (v, w) in a.iter().zip(b) {
                    same &= equivalent(v, w)?;
                }
                same
            }
        }
        _ => a == b,
    })
}

/// Diagnostics contain paths and identities, never response bodies or secrets.
#[derive(Debug, Default, Serialize)]
pub struct Changes {
    /// Whether the normalized provider content is identical.
    pub unchanged: bool,
    /// Paths whose owned data changed.
    pub changed: Vec<String>,
    /// Preserved manual fields with a conflicting proposed value.
    pub manual_conflicts: Vec<String>,
    /// Native IDs transitioned to withdrawn.
    pub withdrawn: Vec<String>,
}

/// Reconcile into a private clone. The caller publishes it only after the global
/// identity gate. Failures never alter the supplied provider.
///
/// # Errors
/// Rejects obsolete state, illegal authority, ambiguous interfaces, and
/// attempts to withdraw a manually controlled status.
pub fn apply(
    provider: &Value,
    feed_id: &str,
    observation: &Observation,
    observed_at: &DateTime<Utc>,
) -> Result<(Value, Changes), RegistryError> {
    validate_provider(provider)?;
    let feeds = provider
        .pointer("/auto_sync/feeds")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Missing source feeds"))?;
    let index = feeds
        .iter()
        .position(|f| f["id"] == feed_id)
        .ok_or_else(|| invalid("Unknown feed ID"))?;
    let feed = &feeds[index];
    crate::mapping::validate(feed)?;
    let membership = feed["writes"]
        .as_array()
        .is_some_and(|w| w.iter().any(|v| v == "models"));
    validate_inventory(feed, membership)?;
    let mut candidate = provider.clone();
    let models = candidate
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid("Missing provider models"))?;
    let mut changes = Changes::default();
    let mut present = BTreeSet::new();
    for observed in &observation.models {
        let native = observed.value["provider_model_id"]
            .as_str()
            .ok_or_else(|| invalid("Missing observed native ID"))?;
        if !present.insert(native) {
            return Err(invalid("Duplicate normalized identity"));
        }
        let existing = models.iter().position(|m| m["provider_model_id"] == native);
        let model = match existing {
            Some(index) => &mut models[index],
            None if membership => {
                let mut value = json!({"provider_model_id":native,"interfaces":[],"sync":{"owner":feed_id,"field_owners":{}}});
                apply_fields(&mut value, observed, feed, feed_id, true, &mut changes)?;
                models.push(value);
                let index = models.len() - 1;
                changes.changed.push(format!("{native}:membership"));
                &mut models[index]
            }
            None => continue,
        };
        if existing.is_some() {
            apply_fields(model, observed, feed, feed_id, false, &mut changes)?;
        }
        if membership && model.pointer("/sync/owner").and_then(Value::as_str) == Some(feed_id) {
            let availability = object(model, "availability")?;
            availability.insert("source_url".into(), feed["request"]["url"].clone());
            availability.insert(
                "checked_at".into(),
                json!(observed_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
            );
            availability.insert("scope".into(), feed["inventory"]["scope"].clone());
        }
    }
    if membership && feed["inventory"]["mode"] == "snapshot" {
        for model in models.iter_mut() {
            let native = model["provider_model_id"]
                .as_str()
                .ok_or_else(|| invalid("Missing source native ID"))?
                .to_owned();
            if present.contains(native.as_str())
                || model.pointer("/sync/owner").and_then(Value::as_str) != Some(feed_id)
            {
                continue;
            }
            if model["status"] != "withdrawn" {
                if owner(model.pointer("/sync/field_owners"), "/status")? != Some(feed_id) {
                    return Err(invalid("Cannot withdraw a manually owned status"));
                }
                model["status"] = json!("withdrawn");
                changes.withdrawn.push(native);
            }
        }
    }
    models.sort_by(|a, b| {
        a["provider_model_id"]
            .as_str()
            .cmp(&b["provider_model_id"].as_str())
    });
    // Preserve evidence for the membership owner's unchanged assertion, including
    // foreign/manual fields which this feed cannot verify.
    for model in candidate["models"]
        .as_array_mut()
        .ok_or_else(|| invalid("models"))?
    {
        let prior = provider["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|old| old["provider_model_id"] == model["provider_model_id"]);
        if model.pointer("/sync/owner").and_then(Value::as_str) == Some(feed_id) {
            if let Some(prior) = prior
                && assertion(prior, feed_id)? == assertion(model, feed_id)?
            {
                if let Some(time) = prior.pointer("/availability/checked_at") {
                    model["availability"]["checked_at"] = time.clone();
                } else if let Some(availability) =
                    model.get_mut("availability").and_then(Value::as_object_mut)
                {
                    availability.remove("checked_at");
                }
            } else {
                object(model, "availability")?.insert("checked_at".into(), json!(observed_at));
            }
        }
    }
    changes.unchanged = candidate == *provider;
    changes.changed.sort();
    changes.changed.dedup();
    changes.manual_conflicts.sort();
    changes.manual_conflicts.dedup();
    Ok((candidate, changes))
}

/// Effective public assertion owned by a feed, excluding observation time.
///
/// # Errors
/// Invalid ownership pointers are rejected rather than guessed.
pub fn assertion(model: &Value, feed_id: &str) -> Result<Value, RegistryError> {
    let mut result = serde_json::Map::new();
    if model.pointer("/sync/owner").and_then(Value::as_str) == Some(feed_id) {
        result.insert(
            "provider_model_id".into(),
            model["provider_model_id"].clone(),
        );
        let mut availability = model.get("availability").cloned().unwrap_or(json!({}));
        if let Some(map) = availability.as_object_mut() {
            map.remove("checked_at");
        }
        result.insert("availability".into(), availability);
    }
    for (path, owner) in model
        .pointer("/sync/field_owners")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if owner == feed_id {
            result.insert(
                path.clone(),
                model.pointer(path).cloned().unwrap_or(Value::Null),
            );
        }
    }
    let mut interfaces = Vec::new();
    for owners in model
        .pointer("/sync/interface_owners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let interface = model["interfaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| interface_key(i) == interface_key(owners));
        let mut fields = serde_json::Map::new();
        for (path, owner) in owners["field_owners"].as_object().into_iter().flatten() {
            if owner == feed_id {
                fields.insert(
                    path.clone(),
                    interface
                        .and_then(|i| i.pointer(path))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
            }
        }
        if !fields.is_empty() {
            interfaces.push(json!({"identity":interface_key(owners),"fields":fields}));
        }
    }
    let preference = model["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| {
            interfaces
                .iter()
                .any(|v| v["identity"] == json!(interface_key(i)))
        })
        .map(interface_key)
        .collect::<Vec<_>>();
    for (path, value) in &mut result {
        normalize_owned(value, path, false);
    }
    for interface in &mut interfaces {
        if let Some(fields) = interface["fields"].as_object_mut() {
            for (path, value) in fields {
                normalize_owned(value, path, true);
            }
        }
    }
    interfaces.sort_by_key(|v| v["identity"].to_string());
    result.insert("preference".into(), json!(preference));
    result.insert("interfaces".into(), json!(interfaces));
    Ok(Value::Object(result))
}

fn validate_inventory(feed: &Value, membership: bool) -> Result<(), RegistryError> {
    if !membership {
        return Ok(());
    }
    let inventory = &feed["inventory"];
    if inventory
        .get("allow_empty")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err(invalid("allow_empty must be boolean"));
    }
    if !matches!(inventory["mode"].as_str(), Some("merge" | "snapshot"))
        || !matches!(inventory["scope"].as_str(), Some("public" | "account"))
        || !inventory["complete"].is_boolean()
    {
        return Err(invalid(
            "Inventory mode/scope/completeness must be explicit",
        ));
    }
    if inventory["mode"] == "snapshot"
        && (inventory["complete"] != true || inventory["scope"] != "public")
    {
        return Err(invalid(
            "Snapshot withdrawals require complete public authority",
        ));
    }
    Ok(())
}

pub(crate) fn validate_state(feed: &Value) -> Result<(), RegistryError> {
    if feed.get("checkpoint").is_some()
        || feed.get("inventory").is_some_and(|v| {
            v.get("missing_runs").is_some() || v.get("minimum_missing_hours").is_some()
        })
    {
        return Err(invalid(
            "Remove beta 6 checkpoints and absence controls before beta 7 sync",
        ));
    }
    if !matches!(
        feed["kind"].as_str(),
        Some("v1_models" | "openrouter_models")
    ) {
        return Ok(());
    }
    validate_inventory(
        feed,
        feed["writes"]
            .as_array()
            .is_some_and(|w| w.iter().any(|v| v == "models")),
    )
}

/// Validate source-only feed ownership and observation state without mutation.
///
/// # Errors
/// Malformed known fields and ambiguous ownership reject the source transaction.
pub fn validate_provider(provider: &Value) -> Result<(), RegistryError> {
    let feeds = provider
        .pointer("/auto_sync/feeds")
        .and_then(Value::as_array);
    let mut feed_ids = BTreeSet::new();
    for feed in feeds.into_iter().flatten() {
        let id = feed["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("Missing feed ID"))?;
        if !feed_ids.insert(id) {
            return Err(invalid("Duplicate feed ID"));
        }
        validate_state(feed)?;
    }
    for model in provider["models"]
        .as_array()
        .ok_or_else(|| invalid("Missing models"))?
    {
        let Some(sync) = model.get("sync") else {
            continue;
        };
        let sync = sync
            .as_object()
            .ok_or_else(|| invalid("Sync state must be an object"))?;
        if let Some(id) = sync.get("owner") {
            let id = id
                .as_str()
                .filter(|s| feed_ids.contains(s))
                .ok_or_else(|| invalid("Membership owner must name a configured feed"))?;
            if !feeds.into_iter().flatten().any(|f| {
                f["id"] == id
                    && f["writes"]
                        .as_array()
                        .is_some_and(|w| w.iter().any(|v| v == "models"))
            }) {
                return Err(invalid("Membership owner lacks models authority"));
            }
        }
        if ["last_seen_at", "missing_since", "missing_runs"]
            .iter()
            .any(|key| sync.contains_key(*key))
        {
            return Err(invalid(
                "Remove beta 6 observation state before beta 7 sync",
            ));
        }
        validate_owners(sync.get("field_owners"), &feed_ids)?;
        validate_owner_domains(sync.get("field_owners"), feeds, false)?;
        if let Some(owners) = sync.get("field_owners").and_then(Value::as_object) {
            for path in owners.keys() {
                if tokens(path)?.first().is_some_and(|p| p == "interfaces") {
                    return Err(invalid("Interface ownership must use stable triples"));
                }
            }
        }
        if let Some(records) = sync.get("interface_owners") {
            let mut keys = BTreeSet::new();
            for record in records
                .as_array()
                .ok_or_else(|| invalid("Interface owners must be an array"))?
            {
                for field in ["protocol", "operation"] {
                    if record[field].as_str().is_none_or(str::is_empty) {
                        return Err(invalid("Invalid interface ownership identity"));
                    }
                }
                if record
                    .get("protocol_profile")
                    .is_some_and(|v| v.as_str().is_none_or(str::is_empty))
                    || !keys.insert(interface_key(record))
                {
                    return Err(invalid("Ambiguous interface ownership identity"));
                }
                validate_owners(record.get("field_owners"), &feed_ids)?;
                validate_owner_domains(record.get("field_owners"), feeds, true)?;
            }
        }
    }
    Ok(())
}
fn validate_owners(owners: Option<&Value>, feeds: &BTreeSet<&str>) -> Result<(), RegistryError> {
    owner(owners, "/ownership_validation")?;
    if let Some(map) = owners.and_then(Value::as_object)
        && map
            .values()
            .any(|v| v.as_str().is_none_or(|id| !feeds.contains(id)))
    {
        return Err(invalid("Field owner must name a configured feed"));
    }
    Ok(())
}

fn validate_owner_domains(
    owners: Option<&Value>,
    feeds: Option<&Vec<Value>>,
    interface: bool,
) -> Result<(), RegistryError> {
    for (path, id) in owners.and_then(Value::as_object).into_iter().flatten() {
        let pointer = if interface {
            format!("/interfaces/0{path}")
        } else {
            path.clone()
        };
        if let Ok(domain) = domain(&pointer) {
            let feed = feeds
                .into_iter()
                .flatten()
                .find(|f| f["id"] == *id)
                .ok_or_else(|| invalid("Unknown field owner"))?;
            let allowed = feed["writes"].as_array().is_some_and(|w| {
                w.iter().any(|v| match domain {
                    "interface_identity" => v == "metadata",
                    "identity" => v == "metadata" || v == "models",
                    domain => v == domain,
                })
            });
            if !allowed || pointer == "/provider_model_id" {
                return Err(invalid("Field owner lacks its declared write authority"));
            }
        }
    }
    Ok(())
}

fn apply_fields(
    model: &mut Value,
    observed: &ObservedModel,
    feed: &Value,
    id: &str,
    new: bool,
    changes: &mut Changes,
) -> Result<(), RegistryError> {
    let fields: BTreeSet<String> = if new {
        observed
            .value
            .as_object()
            .ok_or_else(|| invalid("Observation must be object"))?
            .keys()
            .map(|k| format!("/{k}"))
            .collect()
    } else {
        observed.observed.clone()
    };
    for path in fields {
        if path == "/provider_model_id" {
            continue;
        }
        let Some(value) = read(&observed.value, &path)? else {
            continue;
        };
        if path == "/canonical_id" {
            if new {
                write(model, &path, value.clone())?;
            } else if model.get("canonical_id").is_some_and(|old| old != value) {
                return Err(invalid(
                    "Canonical binding conflicts with an existing offer",
                ));
            }
            // Binding assignment for an existing unbound offer requires ledger
            // evidence and is materialized by the publication coordinator.
            continue;
        }
        if path == "/interfaces" {
            merge_interfaces(model, value, id, new, changes)?;
        } else if let Some(rest) = path.strip_prefix("/interfaces/") {
            let (idx, relative) = rest
                .split_once('/')
                .ok_or_else(|| invalid("Whole indexed interface mapping is ambiguous"))?;
            let source = observed.value["interfaces"]
                .get(
                    idx.parse::<usize>()
                        .map_err(|_| invalid("Invalid interface index"))?,
                )
                .ok_or_else(|| invalid("Missing interface selector"))?;
            let create = feed["writes"]
                .as_array()
                .is_some_and(|w| w.iter().any(|v| v == "metadata"));
            let slot = interface_slot(model, source, create, id)?;
            let owners = interface_owners(model, source)?;
            let mut target = model["interfaces"][slot].clone();
            let owners = update(
                &mut target,
                &format!("/{relative}"),
                value,
                owners,
                id,
                changes,
            )?;
            model["interfaces"][slot] = target;
            store_interface_owners(model, source, owners)?;
        } else {
            let authority = domain(&path)?;
            if !feed["writes"]
                .as_array()
                .is_some_and(|w| w.iter().any(|v| v == authority))
            {
                return Err(invalid("Observation exceeds feed authority"));
            }
            let owners = model
                .pointer("/sync/field_owners")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let owners = update(model, &path, value, owners, id, changes)?;
            object(model, "sync")?.insert("field_owners".into(), owners);
        }
    }
    Ok(())
}

fn merge_interfaces(
    model: &mut Value,
    incoming: &Value,
    id: &str,
    new: bool,
    changes: &mut Changes,
) -> Result<(), RegistryError> {
    let values = incoming
        .as_array()
        .ok_or_else(|| invalid("Interfaces must be an array"))?;
    let mut keys = BTreeSet::new();
    for source in values {
        let key = interface_key(source);
        if !keys.insert(key.clone()) {
            return Err(invalid("Duplicate interface identity"));
        }
        let slot = interface_slot(model, source, true, id)?;
        let mut target = model["interfaces"][slot].clone();
        let mut owners = interface_owners(model, source)?;
        for (name, value) in source
            .as_object()
            .ok_or_else(|| invalid("Interface must be object"))?
        {
            if matches!(name.as_str(), "protocol" | "operation" | "protocol_profile") {
                continue;
            }
            owners = update(&mut target, &format!("/{name}"), value, owners, id, changes)?;
        }
        model["interfaces"][slot] = target;
        store_interface_owners(model, source, owners)?;
    }
    if !new {
        let existing = model["interfaces"]
            .as_array()
            .ok_or_else(|| invalid("Missing interface array"))?
            .clone();
        let mut removed = BTreeSet::new();
        for interface in &existing {
            if !keys.contains(&interface_key(interface)) {
                if crate::mapping::known_value("/interfaces/0", interface, true).is_err() {
                    return Err(invalid("Removing this interface would erase unknown data"));
                }
                let owners = interface_owners(model, interface)?;
                let all_owned = interface
                    .as_object()
                    .ok_or_else(|| invalid("Invalid interface"))?
                    .keys()
                    .all(|k| {
                        owner(Some(&owners), &format!("/{}", escape(k)))
                            .is_ok_and(|o| o == Some(id))
                    })
                    && owners
                        .as_object()
                        .is_some_and(|o| o.values().all(|v| v == id));
                let record = model
                    .pointer("/sync/interface_owners")
                    .and_then(Value::as_array)
                    .and_then(|a| {
                        a.iter()
                            .find(|v| interface_key(v) == interface_key(interface))
                    });
                let opaque = record.and_then(Value::as_object).is_some_and(|o| {
                    o.keys().any(|k| {
                        !matches!(
                            k.as_str(),
                            "protocol" | "operation" | "protocol_profile" | "field_owners"
                        )
                    })
                });
                if !all_owned || opaque {
                    return Err(invalid(
                        "Removing an interface would erase manual, foreign, or unknown data",
                    ));
                }
                removed.insert(interface_key(interface));
            }
        }
        let reordered = values
            .iter()
            .map(|source| {
                existing
                    .iter()
                    .find(|v| interface_key(v) == interface_key(source))
                    .cloned()
                    .ok_or_else(|| invalid("Interface join failed"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        model["interfaces"] = json!(reordered);
        if let Some(records) = model
            .pointer_mut("/sync/interface_owners")
            .and_then(Value::as_array_mut)
        {
            records.retain(|v| !removed.contains(&interface_key(v)));
        }
    }
    Ok(())
}
fn interface_slot(
    model: &mut Value,
    source: &Value,
    create: bool,
    feed: &str,
) -> Result<usize, RegistryError> {
    let interfaces = model
        .get_mut("interfaces")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid("Missing interfaces"))?;
    if let Some(index) = interfaces
        .iter()
        .position(|v| interface_key(v) == interface_key(source))
    {
        return Ok(index);
    }
    if !create {
        return Err(invalid(
            "Pricing cannot create or enable an undeclared interface",
        ));
    }
    let mut value = json!({"protocol":source["protocol"],"operation":source["operation"]});
    if let Some(profile) = source.get("protocol_profile") {
        value["protocol_profile"] = profile.clone();
    }
    interfaces.push(value);
    let index = interfaces.len() - 1;
    let mut owners = interface_owners(model, source)?;
    for field in ["protocol", "operation", "protocol_profile"] {
        if source.get(field).is_some() {
            owners[format!("/{field}")] = json!(feed);
        }
    }
    store_interface_owners(model, source, owners)?;
    Ok(index)
}
fn interface_owners(model: &Value, source: &Value) -> Result<Value, RegistryError> {
    let Some(owners) = model.pointer("/sync/interface_owners") else {
        return Ok(json!({}));
    };
    let owners = owners
        .as_array()
        .ok_or_else(|| invalid("Invalid interface owners"))?;
    Ok(owners
        .iter()
        .find(|v| interface_key(v) == interface_key(source))
        .and_then(|v| v.get("field_owners"))
        .cloned()
        .unwrap_or_else(|| json!({})))
}
fn store_interface_owners(
    model: &mut Value,
    source: &Value,
    owners: Value,
) -> Result<(), RegistryError> {
    let records = object(model, "sync")?
        .entry("interface_owners")
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or_else(|| invalid("Invalid interface owners"))?;
    if let Some(record) = records
        .iter_mut()
        .find(|v| interface_key(v) == interface_key(source))
    {
        record["field_owners"] = owners;
    } else {
        let key = interface_key(source);
        let mut record = json!({"protocol":key.0,"operation":key.1,"field_owners":owners});
        if let Some(profile) = key.2 {
            record["protocol_profile"] = json!(profile);
        }
        records.push(record);
    }
    Ok(())
}
fn owner<'a>(owners: Option<&'a Value>, path: &str) -> Result<Option<&'a str>, RegistryError> {
    let Some(owners) = owners else {
        return Ok(None);
    };
    let mut found = None;
    let owners = owners
        .as_object()
        .ok_or_else(|| invalid("Ownership map must be an object"))?;
    let mut claims = Vec::new();
    for (claim, feed) in owners {
        let tokens = tokens(claim)?;
        if tokens.is_empty() || tokens.iter().any(|s| s.is_empty()) {
            return Err(invalid("Invalid ownership path"));
        }
        if claims
            .iter()
            .any(|other: &Vec<String>| tokens.starts_with(other) || other.starts_with(&tokens))
        {
            return Err(invalid("Overlapping ownership claims"));
        }
        claims.push(tokens);
        let feed = feed
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("Invalid field owner"))?;
        if claim == path || path.starts_with(&format!("{claim}/")) {
            found = Some(feed);
        }
    }
    Ok(found)
}
fn normalize_owned(value: &mut Value, path: &str, interface: bool) {
    let set = if interface {
        matches!(
            path,
            "/input_modalities"
                | "/output_modalities"
                | "/question_types"
                | "/capabilities"
                | "/requires"
                | "/reasoning_effort/levels"
        )
    } else {
        matches!(path, "/model_types" | "/requires")
    };
    if set && let Some(items) = value.as_array_mut() {
        items.sort_by_key(Value::to_string);
    }
    if interface
        && path == "/reasoning_effort"
        && let Some(items) = value.get_mut("levels").and_then(Value::as_array_mut)
    {
        items.sort_by_key(Value::to_string);
    }
}

fn update(
    target: &mut Value,
    path: &str,
    value: &Value,
    mut owners: Value,
    feed: &str,
    changes: &mut Changes,
) -> Result<Value, RegistryError> {
    let prior = read(target, path)?;
    if let Some(old) = prior {
        let mut a = old.clone();
        let mut b = value.clone();
        let interface = target.get("protocol").is_some();
        normalize_owned(&mut a, path, interface);
        normalize_owned(&mut b, path, interface);
        if equivalent(&a, &b)? {
            return Ok(owners);
        }
    }
    let owning = owner(Some(&owners), path)?;
    let descendants = owners
        .as_object()
        .ok_or_else(|| invalid("Invalid ownership"))?
        .keys()
        .any(|claim| claim.starts_with(&format!("{path}/")));
    if owning.is_some_and(|o| o != feed) || (prior.is_some() && owning.is_none() && !descendants) {
        if prior != Some(value) {
            changes.manual_conflicts.push(path.into());
        }
        return Ok(owners);
    }
    if prior.is_some_and(Value::is_object)
        && let Some(fields) = value.as_object()
    {
        for (key, child) in fields {
            owners = update(
                target,
                &format!("{path}/{}", escape(key)),
                child,
                owners,
                feed,
                changes,
            )?;
        }
        return Ok(owners);
    }
    if owners
        .as_object()
        .ok_or_else(|| invalid("Invalid ownership"))?
        .keys()
        .any(|claim| claim.starts_with(&format!("{path}/")))
    {
        if let Some(fields) = value.as_object() {
            for (key, child) in fields {
                owners = update(
                    target,
                    &format!("{path}/{}", escape(key)),
                    child,
                    owners,
                    feed,
                    changes,
                )?;
            }
            return Ok(owners);
        }
        return Err(invalid(
            "Replacing a field would erase descendant ownership",
        ));
    }
    if let Some(previous) = prior.filter(|old| old.is_object() || old.is_array())
        && previous != value
    {
        let first = tokens(path)?.into_iter().next().unwrap_or_default();
        let pointer = if matches!(
            first.as_str(),
            "pricing"
                | "limits"
                | "rate_limits"
                | "compatibility"
                | "reasoning_effort"
                | "capabilities"
                | "question_types"
                | "input_modalities"
                | "output_modalities"
        ) {
            format!("/interfaces/0{path}")
        } else {
            path.into()
        };
        if crate::mapping::known_value(&pointer, previous, true).is_err() {
            return Err(invalid(
                "Replacing this container would erase unknown data; review the source change",
            ));
        }
    }
    if prior != Some(value) {
        if value.is_null() {
            remove(target, path)?;
        } else {
            write(target, path, value.clone())?;
        }
        changes.changed.push(path.into());
    }
    if owning.is_none() && !value.is_null() {
        owners
            .as_object_mut()
            .ok_or_else(|| invalid("Invalid field ownership"))?
            .insert(path.into(), json!(feed));
    }
    Ok(owners)
}
fn remove(target: &mut Value, path: &str) -> Result<(), RegistryError> {
    let (parent, last) = path
        .rsplit_once('/')
        .ok_or_else(|| invalid("Invalid clear path"))?;
    let key = tokens(&format!("/{last}"))?
        .into_iter()
        .next()
        .ok_or_else(|| invalid("Invalid clear key"))?;
    if let Some(map) = target.pointer_mut(parent).and_then(Value::as_object_mut) {
        map.remove(&key);
    }
    Ok(())
}
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}
fn object<'a>(
    value: &'a mut Value,
    key: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, RegistryError> {
    value
        .as_object_mut()
        .ok_or_else(|| invalid("Expected object"))?
        .entry(key)
        .or_insert(json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("Expected nested object"))
}
fn invalid(message: &str) -> RegistryError {
    RegistryError::invalid("/auto_sync", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::normalize;
    fn fixture() -> Value {
        json!({"name":"test","status":"active","models":[],"auto_sync":{"feeds":[{"id":"list","kind":"v1_models","writes":["models","metadata"],"request":{"url":"https://example.com/models","auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"snapshot","scope":"public","complete":true,"allow_empty":true},"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id","/name":"/name"},"defaults":{"status":"active","interfaces":[]}}}]}})
    }
    fn at(_label: u64, hour: i64) -> Result<DateTime<Utc>, RegistryError> {
        DateTime::from_timestamp(hour * 3600, 0).ok_or_else(|| invalid("test time"))
    }
    #[test]
    fn assertion_sets_are_unordered_but_interface_preference_is_content()
    -> Result<(), RegistryError> {
        let mut model = json!({"provider_model_id":"A","interfaces":[{"protocol":"messages","operation":"generation"},{"protocol":"chat_completions","operation":"generation"}],"model_types":["language","decision"],"sync":{"owner":"list","field_owners":{"/model_types":"list"},"interface_owners":[{"protocol":"messages","operation":"generation","field_owners":{"/protocol":"list"}},{"protocol":"chat_completions","operation":"generation","field_owners":{"/protocol":"list"}}]}});
        let before = assertion(&model, "list")?;
        model["model_types"] = json!(["decision", "language"]);
        assert_eq!(before, assertion(&model, "list")?);
        model["interfaces"]
            .as_array_mut()
            .ok_or_else(|| invalid("interfaces"))?
            .reverse();
        assert_ne!(before, assertion(&model, "list")?);
        assert!(equivalent(
            &json!(-1),
            &crate::document::parse_json(b"-1.00")?
        )?);
        assert!(!equivalent(&json!(1), &json!({"number":"1"}))?);
        Ok(())
    }
    #[test]
    fn historical_absent_offer_does_not_acquire_null_availability() -> Result<(), RegistryError> {
        let mut initial = fixture();
        initial["models"] = json!([{"provider_model_id":"old","status":"withdrawn","interfaces":[],"sync":{"owner":"list","field_owners":{"/status":"list"}}}]);
        let absent = normalize(&initial["auto_sync"]["feeds"][0], &[json!({"data":[]})])?;
        let (next, report) = apply(&initial, "list", &absent, &at(1, 0)?)?;
        assert_eq!(next, initial);
        assert!(report.unchanged);
        assert!(next["models"][0].get("availability").is_none());
        Ok(())
    }
    #[test]
    fn snapshots_are_idempotent_and_withdraw_without_history() -> Result<(), RegistryError> {
        let initial = fixture();
        let feed = &initial["auto_sync"]["feeds"][0];
        let present = normalize(feed, &[json!({"data":[{"id":"A","name":"first"}]})])?;
        let absent = normalize(feed, &[json!({"data":[]})])?;
        let (one, _) = apply(&initial, "list", &present, &at(1, 0)?)?;
        let (same, report) = apply(&one, "list", &present, &at(2, 1)?)?;
        assert_eq!(one, same);
        assert!(report.unchanged);
        let (two, report) = apply(&one, "list", &absent, &at(3, 2)?)?;
        assert_eq!(report.withdrawn, vec!["A"]);
        assert_eq!(two["models"][0]["status"], "withdrawn");
        let (same, report) = apply(&two, "list", &absent, &at(4, 3)?)?;
        assert_eq!(same, two);
        assert!(report.unchanged);
        let (three, _) = apply(&two, "list", &present, &at(5, 4)?)?;
        assert_eq!(three["models"][0]["status"], "active");
        Ok(())
    }
    #[test]
    fn manual_overrides_and_opaque_generated_metadata_survive() -> Result<(), RegistryError> {
        let initial = fixture();
        let feed = &initial["auto_sync"]["feeds"][0];
        let present = normalize(feed, &[json!({"data":[{"id":"A","name":"feed"}]})])?;
        let absent = normalize(feed, &[json!({"data":[]})])?;
        let (mut one, _) = apply(&initial, "list", &present, &at(1, 0)?)?;
        one["models"][0]["name"] = json!("manual");
        one["models"][0]["sync"]["field_owners"]
            .as_object_mut()
            .ok_or_else(|| invalid("test owners"))?
            .remove("/name");
        one["models"][0]["availability"]["future"] = json!({"opaque":true});
        let (mut two, changes) = apply(&one, "list", &present, &at(2, 1)?)?;
        assert_eq!(two["models"][0]["name"], "manual");
        assert!(!changes.manual_conflicts.is_empty());
        assert_eq!(
            two["models"][0]["availability"]["future"],
            one["models"][0]["availability"]["future"]
        );
        two["models"][0]["sync"]["field_owners"]
            .as_object_mut()
            .ok_or_else(|| invalid("test owners"))?
            .remove("/status");
        assert!(apply(&two, "list", &absent, &at(3, 2)?).is_err());
        Ok(())
    }
    #[test]
    fn a_foreign_clear_does_not_lose_ownership() -> Result<(), RegistryError> {
        let mut target = json!({});
        let owners = json!({"/name":"other"});
        let mut report = Changes::default();
        let result = update(
            &mut target,
            "/name",
            &json!("new"),
            owners.clone(),
            "list",
            &mut report,
        )?;
        assert_eq!(target, json!({}));
        assert_eq!(result, owners);
        assert_eq!(report.manual_conflicts, vec!["/name"]);
        Ok(())
    }

    #[test]
    fn partial_object_ownership_updates_only_its_children_and_preserves_opaque_data()
    -> Result<(), RegistryError> {
        let mut target = json!({"pricing":{"kind":"token","currency":"USD","input_tokens":{"no_cache":1,"cache_read":7},"future":{"opaque":true}}});
        let mut report = Changes::default();
        let owners = json!({"/pricing/input_tokens/no_cache":"list"});
        update(
            &mut target,
            "/pricing",
            &json!({"kind":"token","currency":"USD","input_tokens":{"no_cache":2,"cache_read":8}}),
            owners,
            "list",
            &mut report,
        )?;
        assert_eq!(
            target["pricing"]["input_tokens"],
            json!({"no_cache":2,"cache_read":7})
        );
        assert_eq!(target["pricing"]["future"], json!({"opaque":true}));
        let before = target.clone();
        assert!(
            update(
                &mut target,
                "/pricing",
                &Value::Null,
                json!({"/pricing":"list"}),
                "list",
                &mut report
            )
            .is_err()
        );
        assert_eq!(target, before);
        Ok(())
    }

    #[test]
    fn malformed_persisted_source_state_is_never_reset_or_disambiguated() {
        let mut value = fixture();
        value["models"] = json!([{"provider_model_id":"A","status":"active","interfaces":[],"sync":{"owner":"list","missing_runs":-1,"missing_since":"1970-01-01T00:00:00Z"}}]);
        assert!(validate_provider(&value).is_err());
        value["models"][0]["sync"] = json!({"interface_owners":[{"protocol":"messages","operation":"generation","field_owners":{}},{"protocol":"messages","operation":"generation","protocol_profile":"default","field_owners":{}}]});
        assert!(validate_provider(&value).is_err());
        value["models"][0]["sync"] = json!({});
        value["auto_sync"]["feeds"][0]["checkpoint"] =
            json!({"sequence":1,"observed_at":"1970-01-01T00:00:00Z"});
        assert!(validate_provider(&value).is_err());
    }

    #[test]
    fn whole_interface_removal_requires_owned_known_contents_and_correct_domain()
    -> Result<(), RegistryError> {
        let mut initial = fixture();
        initial["auto_sync"]["feeds"][0]["writes"] = json!(["models", "metadata", "pricing"]);
        initial["auto_sync"]["feeds"][0]["response"]["mapping"] =
            json!({"/id":"/provider_model_id","/interfaces":"/interfaces"});
        let feed = &initial["auto_sync"]["feeds"][0];
        let present = normalize(
            feed,
            &[
                json!({"data":[{"id":"A","interfaces":[{"protocol":"messages","operation":"generation","pricing":{"kind":"token","currency":"USD","input_tokens":{"no_cache":1}}}]}]}),
            ],
        )?;
        let empty = normalize(feed, &[json!({"data":[{"id":"A","interfaces":[]}]})])?;
        let (one, _) = apply(&initial, "list", &present, &at(1, 0)?)?;
        let mut opaque = one.clone();
        opaque["models"][0]["interfaces"][0]["pricing"]["future"] = json!({"opaque":true});
        assert!(apply(&opaque, "list", &empty, &at(2, 1)?).is_err());
        let (two, _) = apply(&one, "list", &empty, &at(2, 1)?)?;
        assert_eq!(two["models"][0]["interfaces"], json!([]));
        let mut bad = one;
        bad["auto_sync"]["feeds"][0]["writes"] = json!(["metadata"]);
        assert!(validate_provider(&bad).is_err());
        Ok(())
    }

    #[test]
    fn a_pricing_feed_cannot_create_an_interface_from_its_selector_defaults()
    -> Result<(), RegistryError> {
        let price = json!({"id":"prices","kind":"v1_models","writes":["pricing"],"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id","/cost":"/interfaces/0/pricing/input_tokens/no_cache"},"defaults":{"interfaces":[{"protocol":"messages","operation":"generation","pricing":{"kind":"token","currency":"USD"}}]}}});
        let provider = json!({"name":"test","status":"active","models":[{"provider_model_id":"A","status":"active","interfaces":[]}],"auto_sync":{"feeds":[price]}});
        let observation = normalize(
            &provider["auto_sync"]["feeds"][0],
            &[json!({"data":[{"id":"A","cost":1}]})],
        )?;
        assert!(apply(&provider, "prices", &observation, &at(1, 0)?).is_err());
        Ok(())
    }
}

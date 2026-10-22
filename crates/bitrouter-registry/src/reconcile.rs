//! Atomic source reconciliation, scoped ownership, and monotonic replay control.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::document::{RegistryError, integer, interface_key, timestamp};
use crate::mapping::{Observation, ObservedModel, domain, read, tokens, write};
use crate::pricing::decimal;

/// A complete observation's stable identity. Replays must keep all three fields.
#[derive(Clone, Debug, Serialize)]
pub struct Stamp {
    /// Strictly increasing within one provider/feed; safe JSON integer.
    pub sequence: u64,
    /// Diagnostic run label, never a substitute for sequence ordering.
    pub id: String,
    /// Time the successful complete observation was collected.
    pub observed_at: DateTime<Utc>,
}

/// Diagnostics contain paths and identities, never response bodies or secrets.
#[derive(Debug, Default, Serialize)]
pub struct Changes {
    /// Whether replay/stale suppression prevented all mutations.
    pub unchanged: bool,
    /// Paths whose owned data changed.
    pub changed: Vec<String>,
    /// Preserved manual fields with a conflicting proposed value.
    pub manual_conflicts: Vec<String>,
    /// Native IDs transitioned to withdrawn.
    pub withdrawn: Vec<String>,
}

/// Assign the next sequence from persisted state without wrapping.
///
/// # Errors
/// Invalid/exhausted checkpoint data is not repaired by inventing a new epoch.
pub fn next_stamp(
    feed: &Value,
    id: String,
    observed_at: DateTime<Utc>,
) -> Result<Stamp, RegistryError> {
    let sequence = checkpoint_sequence(feed)?
        .checked_add(1)
        .filter(|n| *n <= 9_007_199_254_740_991)
        .ok_or_else(|| invalid("Checkpoint sequence exhausted"))?;
    Ok(Stamp {
        sequence,
        id,
        observed_at,
    })
}

fn checkpoint_sequence(feed: &Value) -> Result<u64, RegistryError> {
    match feed.get("checkpoint") {
        None => Ok(0),
        Some(value) => {
            if value["id"].as_str().is_none_or(str::is_empty)
                || value["observed_at"].as_str().and_then(timestamp).is_none()
            {
                return Err(invalid("Invalid persisted checkpoint identity or time"));
            }
            integer(&value["sequence"])
                .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
                .ok_or_else(|| invalid("Invalid checkpoint sequence"))
        }
    }
}

/// Reconcile into a private clone. The caller publishes it only after the global
/// identity gate. Failures never alter the supplied provider or checkpoint.
///
/// # Errors
/// Rejects ordering conflicts, illegal authority, ambiguous interfaces, and
/// attempts to withdraw a manually controlled status.
pub fn apply(
    provider: &Value,
    feed_id: &str,
    observation: &Observation,
    stamp: &Stamp,
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
    let accepted = checkpoint_sequence(feed)?;
    let checkpoint = &feed["checkpoint"];
    if stamp.sequence == 0 || stamp.sequence > 9_007_199_254_740_991 || stamp.id.is_empty() {
        return Err(invalid("Invalid observation stamp"));
    }
    if stamp.sequence < accepted
        || (stamp.sequence == accepted
            && checkpoint["id"] == stamp.id
            && checkpoint["observed_at"].as_str().and_then(timestamp) == Some(stamp.observed_at))
    {
        return Ok((
            provider.clone(),
            Changes {
                unchanged: true,
                ..Changes::default()
            },
        ));
    }
    if stamp.sequence == accepted {
        return Err(invalid("Conflicting checkpoint replay"));
    }
    if accepted > 0
        && checkpoint["observed_at"]
            .as_str()
            .and_then(timestamp)
            .is_none_or(|t| stamp.observed_at < t)
    {
        return Err(invalid("Observation time precedes persisted checkpoint"));
    }
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
            let sync = object(model, "sync")?;
            sync.insert(
                "last_seen_at".into(),
                json!(
                    stamp
                        .observed_at
                        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
                ),
            );
            sync.remove("missing_since");
            sync.remove("missing_runs");
            let availability = object(model, "availability")?;
            availability.insert("source_url".into(), feed["request"]["url"].clone());
            availability.insert(
                "checked_at".into(),
                json!(
                    stamp
                        .observed_at
                        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
                ),
            );
            availability.insert("scope".into(), feed["inventory"]["scope"].clone());
        }
    }
    if membership && feed["inventory"]["mode"] == "snapshot" {
        let required = feed["inventory"]
            .get("missing_runs")
            .and_then(integer)
            .unwrap_or(2);
        let hours = decimal(
            &feed["inventory"]
                .get("minimum_missing_hours")
                .unwrap_or(&json!(24))
                .to_string(),
        )?;
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
            let sync = object(model, "sync")?;
            let count = sync
                .get("missing_runs")
                .and_then(integer)
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| invalid("Absence counter exhausted"))?;
            let since = match sync.get("missing_since") {
                None => stamp.observed_at,
                Some(v) => v
                    .as_str()
                    .and_then(timestamp)
                    .ok_or_else(|| invalid("Invalid missing_since"))?,
            };
            if since > stamp.observed_at {
                return Err(invalid("Absence evidence is in the future"));
            }
            sync.insert(
                "missing_since".into(),
                json!(since.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
            );
            sync.insert("missing_runs".into(), json!(count));
            let nanos = num_bigint::BigInt::from(stamp.observed_at.timestamp() - since.timestamp())
                * 1_000_000_000u64
                + num_bigint::BigInt::from(
                    i64::from(stamp.observed_at.timestamp_subsec_nanos())
                        - i64::from(since.timestamp_subsec_nanos()),
                );
            let elapsed = num_rational::BigRational::new(nanos, 3_600_000_000_000u64.into());
            if count >= required && elapsed >= hours && model["status"] != "withdrawn" {
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
    let checkpoint = object(&mut candidate["auto_sync"]["feeds"][index], "checkpoint")?;
    checkpoint.insert("sequence".into(), json!(stamp.sequence));
    checkpoint.insert("id".into(), json!(stamp.id));
    checkpoint.insert(
        "observed_at".into(),
        json!(
            stamp
                .observed_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        ),
    );
    changes.changed.sort();
    changes.changed.dedup();
    changes.manual_conflicts.sort();
    changes.manual_conflicts.dedup();
    Ok((candidate, changes))
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
    if let Some(runs) = inventory.get("missing_runs")
        && integer(runs).is_none_or(|n| n == 0)
    {
        return Err(invalid("missing_runs must be positive"));
    }
    if let Some(hours) = inventory.get("minimum_missing_hours") {
        decimal(&hours.to_string())?;
    }
    Ok(())
}

pub(crate) fn validate_state(feed: &Value) -> Result<(), RegistryError> {
    checkpoint_sequence(feed)?;
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
        for key in ["last_seen_at", "missing_since"] {
            if sync
                .get(key)
                .is_some_and(|v| v.as_str().and_then(timestamp).is_none())
            {
                return Err(invalid("Invalid sync timestamp"));
            }
        }
        if sync
            .get("missing_runs")
            .is_some_and(|v| integer(v).is_none_or(|n| n == 0))
            || sync.contains_key("missing_runs") != sync.contains_key("missing_since")
        {
            return Err(invalid("Invalid absence evidence"));
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
fn update(
    target: &mut Value,
    path: &str,
    value: &Value,
    mut owners: Value,
    feed: &str,
    changes: &mut Changes,
) -> Result<Value, RegistryError> {
    let prior = read(target, path)?;
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
        json!({"name":"test","status":"active","models":[],"auto_sync":{"feeds":[{"id":"list","kind":"v1_models","writes":["models","metadata"],"request":{"url":"https://example.com/models","auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"snapshot","scope":"public","complete":true,"allow_empty":true,"missing_runs":2,"minimum_missing_hours":24},"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id","/name":"/name"},"defaults":{"status":"active","interfaces":[]}}}]}})
    }
    fn at(sequence: u64, hour: i64) -> Result<Stamp, RegistryError> {
        Ok(Stamp {
            sequence,
            id: format!("run-{sequence}"),
            observed_at: DateTime::from_timestamp(hour * 3600, 0)
                .ok_or_else(|| invalid("test time"))?,
        })
    }
    #[test]
    fn presence_absence_replay_and_reactivation_are_transactional() -> Result<(), RegistryError> {
        let initial = fixture();
        let feed = &initial["auto_sync"]["feeds"][0];
        let present = normalize(feed, &[json!({"data":[{"id":"Native/A","name":"first"}]})])?;
        let absent = normalize(feed, &[json!({"data":[]})])?;
        let (one, _) = apply(&initial, "list", &present, &at(1, 0)?)?;
        let (two, _) = apply(&one, "list", &absent, &at(2, 1)?)?;
        assert_eq!(two["models"][0]["status"], "active");
        let (three, changes) = apply(&two, "list", &absent, &at(3, 25)?)?;
        assert_eq!(changes.withdrawn, vec!["Native/A"]);
        let (replay, report) = apply(&three, "list", &present, &at(1, 0)?)?;
        assert!(report.unchanged);
        assert_eq!(replay, three);
        let (four, _) = apply(&three, "list", &present, &at(4, 26)?)?;
        assert_eq!(four["models"][0]["status"], "active");
        assert!(four["models"][0]["sync"].get("missing_since").is_none());
        let mut conflict = at(4, 26)?;
        conflict.id = "different".into();
        assert!(apply(&four, "list", &present, &conflict).is_err());
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
        one["auto_sync"]["feeds"][0]["checkpoint"]["future"] = json!(42);
        let (mut two, changes) = apply(&one, "list", &present, &at(2, 1)?)?;
        assert_eq!(two["models"][0]["name"], "manual");
        assert!(!changes.manual_conflicts.is_empty());
        assert_eq!(
            two["models"][0]["availability"]["future"],
            one["models"][0]["availability"]["future"]
        );
        assert_eq!(two["auto_sync"]["feeds"][0]["checkpoint"]["future"], 42);
        two["models"][0]["sync"]["field_owners"]
            .as_object_mut()
            .ok_or_else(|| invalid("test owners"))?
            .remove("/status");
        let (three, _) = apply(&two, "list", &absent, &at(3, 2)?)?;
        assert!(apply(&three, "list", &absent, &at(4, 26)?).is_err());
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

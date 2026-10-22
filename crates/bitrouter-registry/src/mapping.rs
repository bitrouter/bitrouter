//! Bounded JSON Pointer normalization, without implicit coercion or passthrough.

use std::collections::{BTreeMap, BTreeSet};

use num_rational::BigRational;
use num_traits::Zero;
use serde_json::{Value, json};

use crate::document::{
    Document, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_NODES, ReaderPolicy, RegistryError, SPEC_REVISION,
    native_id,
};
use crate::pricing::decimal;

/// One normalized row plus the fields actually observed (not absent defaults).
#[derive(Debug, Clone)]
pub struct ObservedModel {
    /// Reviewed defaults plus mapped values, retaining explicit optional clears.
    pub value: Value,
    /// Source-present destination paths; membership status is always observed.
    pub observed: BTreeSet<String>,
}

/// Complete interpreted response set; global inventory authority is separate.
#[derive(Debug)]
pub struct Observation {
    /// Normalized rows, unique by exact native identity.
    pub models: Vec<ObservedModel>,
    /// Number of source rows excluded by the declared recognized filter.
    pub excluded: usize,
}

/// JSON typed equality, with mathematically equal numbers independent of spelling.
pub fn typed_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            let exact = |n: &serde_json::Number| {
                let spelling = n.to_string();
                let (negative, body) = spelling
                    .strip_prefix('-')
                    .map_or((false, spelling.as_str()), |s| (true, s));
                decimal(body).map(|v| if negative { -v } else { v }).ok()
            };
            exact(a).zip(exact(b)).is_some_and(|(a, b)| a == b)
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| typed_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, a)| b.get(k).is_some_and(|b| typed_equal(a, b)))
        }
        _ => a == b,
    }
}

fn interface_authority(
    interfaces: &Value,
    authority: &BTreeSet<&str>,
) -> Result<(), RegistryError> {
    for interface in interfaces
        .as_array()
        .ok_or_else(|| invalid("Interfaces must be an array"))?
    {
        for key in interface
            .as_object()
            .ok_or_else(|| invalid("Interface must be an object"))?
            .keys()
        {
            let owner = domain(&format!("/interfaces/0/{key}"))?;
            if owner != "interface_identity" && !authority.contains(owner) {
                return Err(invalid("Nested interface field exceeds declared writes"));
            }
        }
    }
    Ok(())
}

/// Decode JSON Pointer, rejecting unsupported escapes and unsafe object keys.
///
/// # Errors
/// Invalid pointer syntax is a configuration error, not a missing observation.
pub fn tokens(pointer: &str) -> Result<Vec<String>, RegistryError> {
    if pointer.bytes().filter(|b| *b == b'/').count() > crate::document::MAX_DOCUMENT_DEPTH {
        return Err(invalid("JSON Pointer exceeds the supported nesting depth"));
    }
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let raw = pointer
        .strip_prefix('/')
        .ok_or_else(|| invalid("JSON Pointer must start with slash"))?;
    raw.split('/')
        .map(|part| {
            let mut chars = part.chars();
            let mut decoded = String::new();
            while let Some(ch) = chars.next() {
                if ch == '~' {
                    match chars.next() {
                        Some('0') => decoded.push('~'),
                        Some('1') => decoded.push('/'),
                        _ => return Err(invalid("Invalid JSON Pointer escape")),
                    }
                } else {
                    decoded.push(ch);
                }
            }
            if matches!(decoded.as_str(), "__proto__" | "prototype" | "constructor") {
                return Err(invalid("Unsafe object key in JSON Pointer"));
            }
            Ok(decoded)
        })
        .collect()
}

fn index(token: &str) -> Result<usize, RegistryError> {
    if token.is_empty()
        || (token.len() > 1 && token.starts_with('0'))
        || !token.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid("Expected a fixed zero-based array index"));
    }
    token
        .parse()
        .map_err(|_| invalid("Array index exceeds supported range"))
}

/// Read a pointer while distinguishing missing fields from scalar traversal.
///
/// # Errors
/// Invalid pointers, array indices and traversal through scalars are rejected.
pub fn read<'a>(root: &'a Value, pointer: &str) -> Result<Option<&'a Value>, RegistryError> {
    let mut value = root;
    for token in tokens(pointer)? {
        value = match value {
            Value::Object(map) => match map.get(&token) {
                Some(value) => value,
                None => return Ok(None),
            },
            Value::Array(values) => match values.get(index(&token)?) {
                Some(value) => value,
                None => return Ok(None),
            },
            _ => return Err(invalid("JSON Pointer traverses a scalar")),
        };
    }
    Ok(Some(value))
}

fn write_parts(root: &mut Value, path: &[String], value: Value) -> Result<(), RegistryError> {
    let Some((head, tail)) = path.split_first() else {
        return Err(invalid("Destination root is forbidden"));
    };
    match root {
        Value::Object(map) => {
            if tail.is_empty() {
                map.insert(head.clone(), value);
                return Ok(());
            }
            if !map.contains_key(head) {
                if head == "interfaces" {
                    return Err(invalid("Destination arrays must already exist in defaults"));
                }
                map.insert(head.clone(), json!({}));
            }
            let child = map
                .get_mut(head)
                .ok_or_else(|| invalid("Missing destination object"))?;
            write_parts(child, tail, value)
        }
        Value::Array(values) => {
            let child = values
                .get_mut(index(head)?)
                .ok_or_else(|| invalid("Destination array index is outside defaults"))?;
            if tail.is_empty() {
                *child = value;
                Ok(())
            } else {
                write_parts(child, tail, value)
            }
        }
        _ => Err(invalid("Destination pointer traverses a scalar")),
    }
}

/// Write one validated destination pointer. Arrays must already have their shape.
///
/// # Errors
/// Rejects root replacement, append, sparse arrays, and scalar traversal.
pub fn write(root: &mut Value, pointer: &str, value: Value) -> Result<(), RegistryError> {
    let path = tokens(pointer)?;
    if path.iter().any(|t| t == "-") {
        return Err(invalid("Array append is forbidden"));
    }
    write_parts(root, &path, value)
}

/// Domain of a known normalized model path. Identity paths are join-only.
///
/// # Errors
/// Unknown destinations require an importer update rather than execution guesses.
pub fn domain(pointer: &str) -> Result<&'static str, RegistryError> {
    let path = tokens(pointer)?;
    let root = path
        .first()
        .map(String::as_str)
        .ok_or_else(|| invalid("Destination root is forbidden"))?;
    if path.iter().any(|p| {
        matches!(
            p.as_str(),
            "requires" | "sync" | "auth" | "curation" | "auto_sync"
        )
    }) {
        return Err(invalid("Mapping cannot write control or ownership fields"));
    }
    match root {
        "provider_model_id" | "canonical_id" if path.len() == 1 => Ok("identity"),
        "status" | "available_until" if path.len() == 1 => Ok("models"),
        "name" | "description" | "model_types" | "upstream_lifecycle" if path.len() == 1 => {
            Ok("metadata")
        }
        "interfaces" => {
            if path.len() == 1 {
                return Ok("metadata");
            }
            index(&path[1])?;
            if path.len() == 2 {
                return Ok("metadata");
            }
            match path[2].as_str() {
                "protocol" | "operation" | "protocol_profile" if path.len() == 3 => {
                    Ok("interface_identity")
                }
                "pricing" if known_price_path(&path[3..]) => Ok("pricing"),
                "question_types" | "base_url" | "input_modalities" | "output_modalities"
                | "capabilities" | "limits" | "rate_limits" | "compatibility"
                | "reasoning_effort"
                    if known_metadata_path(&path[2..]) =>
                {
                    Ok("metadata")
                }
                _ => Err(unsupported("Unknown interface mapping destination")),
            }
        }
        _ => Err(unsupported("Unknown normalized mapping destination")),
    }
}

fn known_metadata_path(path: &[String]) -> bool {
    if path.len() == 1 {
        return true;
    }
    match path[0].as_str() {
        "question_types" | "input_modalities" | "output_modalities" | "capabilities" => {
            path.len() == 2 && index(&path[1]).is_ok()
        }
        "limits" => {
            path.len() == 2
                && matches!(
                    path[1].as_str(),
                    "max_input_tokens" | "max_output_tokens" | "context_window"
                )
        }
        "rate_limits" => {
            path.len() == 2
                && matches!(
                    path[1].as_str(),
                    "requests_per_minute" | "tokens_per_minute"
                )
        }
        "compatibility" => {
            path.len() == 2
                && matches!(
                    path[1].as_str(),
                    "google_extensions"
                        | "token_limit_field"
                        | "supports_store"
                        | "supports_stream_options"
                )
        }
        "reasoning_effort" => {
            (path.len() == 2 && matches!(path[1].as_str(), "levels" | "default"))
                || (path.len() == 3 && path[1] == "levels" && index(&path[2]).is_ok())
        }
        _ => false,
    }
}
fn known_price_path(path: &[String]) -> bool {
    if path.is_empty() {
        return true;
    }
    match path[0].as_str() {
        "kind" | "currency" | "endpoint_profile" => path.len() == 1,
        "input_tokens" => {
            path.len() == 1
                || (path.len() == 2
                    && matches!(path[1].as_str(), "no_cache" | "cache_read" | "cache_write"))
        }
        "output_tokens" => {
            path.len() == 1 || (path.len() == 2 && matches!(path[1].as_str(), "text" | "reasoning"))
        }
        "context_tiers" => {
            path.len() == 1
                || (index(&path[1]).is_ok()
                    && (path.len() == 2
                        || (path.len() == 3 && path[2] == "above_input_tokens")
                        || (matches!(path[2].as_str(), "input_tokens" | "output_tokens")
                            && known_price_path(&path[2..]))))
        }
        "meters" => {
            path.len() == 1
                || (index(&path[1]).is_ok()
                    && (path.len() == 2
                        || (path.len() == 3
                            && matches!(path[2].as_str(), "id" | "unit" | "quantity" | "amount"))))
        }
        _ => false,
    }
}

pub(crate) fn known_value(path: &str, value: &Value, reviewed: bool) -> Result<(), RegistryError> {
    if reviewed && path.ends_with("/pricing/requires") {
        return Ok(());
    }
    domain(path)?;
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            known_value(
                &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                value,
                reviewed,
            )?;
        }
    } else if let Some(values) = value.as_array() {
        // Model type arrays have no nested object schema.
        if path == "/model_types" {
            return Ok(());
        }
        for (index, value) in values.iter().enumerate() {
            known_value(&format!("{path}/{index}"), value, reviewed)?;
        }
    }
    Ok(())
}

fn destinations(value: &Value) -> Result<Vec<&str>, RegistryError> {
    if let Some(path) = value.as_str() {
        return Ok(vec![path]);
    }
    let paths = value
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid("Mapping needs one destination or a nonempty destination array"))?;
    paths
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| invalid("Destination must be a JSON Pointer string"))
        })
        .collect()
}

fn writes(feed: &Value) -> Result<BTreeSet<&str>, RegistryError> {
    let values = feed["writes"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("Feed writes must be nonempty"))?;
    let mut domains = BTreeSet::new();
    for value in values {
        let name = value
            .as_str()
            .ok_or_else(|| invalid("Write domain must be a string"))?;
        if !matches!(name, "models" | "metadata" | "pricing") {
            return Err(unsupported("Unknown feed write domain"));
        }
        if !domains.insert(name) {
            return Err(invalid("Duplicate feed write domain"));
        }
    }
    Ok(domains)
}

/// Validate mapping/transforms before any credentials are read or HTTP is sent.
///
/// # Errors
/// Returns malformed/unsupported configuration with no feed side effects.
pub fn validate(feed: &Value) -> Result<(), RegistryError> {
    if feed["kind"] == crate::openrouter::KIND {
        return crate::openrouter::validate(feed);
    }
    if feed["kind"] != "v1_models" || !ReaderPolicy::default().supports(feed) {
        return Err(unsupported("Unknown feed kind or required feature"));
    }
    let authority = writes(feed)?;
    let response = feed
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("Missing response configuration"))?;
    tokens(
        response
            .get("items_path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Missing items_path"))?,
    )?;
    let mapping = response
        .get("mapping")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| invalid("Missing mapping object"))?;
    let mut paths = Vec::<(String, Vec<String>)>::new();
    for (source, target) in mapping {
        tokens(source)?;
        for pointer in destinations(target)? {
            let path = tokens(pointer)?;
            let owner = domain(pointer)?;
            if !matches!(owner, "identity" | "interface_identity") && !authority.contains(owner) {
                return Err(invalid("Destination is outside declared writes"));
            }
            if paths
                .iter()
                .any(|(_, other)| path.starts_with(other) || other.starts_with(&path))
            {
                return Err(invalid("Duplicate or overlapping mapping destinations"));
            }
            paths.push((pointer.into(), path));
        }
    }
    if !paths.iter().any(|(p, _)| p == "/provider_model_id") {
        return Err(invalid("Native identity must be explicitly mapped"));
    }
    if let Some(defaults) = response.get("defaults") {
        let defaults = defaults
            .as_object()
            .ok_or_else(|| invalid("Defaults must be an object"))?;
        if let Some(interfaces) = defaults.get("interfaces") {
            interface_authority(interfaces, &authority)?;
        }
        for (key, value) in defaults {
            known_value(&format!("/{key}"), value, true)?;
            let owner = domain(&format!("/{key}"))?;
            let empty_interfaces =
                key == "interfaces" && value.as_array().is_some_and(Vec::is_empty);
            if !empty_interfaces
                && !matches!(owner, "identity" | "interface_identity")
                && !authority.contains(owner)
                && !(key == "interfaces" && authority.contains("pricing"))
            {
                return Err(invalid("Default is outside declared writes"));
            }
        }
    }
    if let Some(transforms) = response.get("transforms") {
        for (target, steps) in transforms
            .as_object()
            .ok_or_else(|| invalid("Transforms must be an object"))?
        {
            if target == "/provider_model_id" || !paths.iter().any(|(path, _)| path == target) {
                return Err(invalid(
                    "Transform target must be mapped and cannot alter native identity",
                ));
            }
            let steps = steps
                .as_array()
                .filter(|v| v.len() <= 64)
                .ok_or_else(|| invalid("Invalid or excessive transform list"))?;
            for step in steps {
                match step["kind"].as_str() {
                    Some("prefix") if step["value"].is_string() => {}
                    Some("parse_decimal") => {}
                    Some("multiply") if step["factor"].is_number() => {
                        decimal(&step["factor"].to_string())?;
                    }
                    Some("value_map") => {
                        let values = step["values"]
                            .as_array()
                            .ok_or_else(|| invalid("value_map needs a values array"))?;
                        for (i, item) in values.iter().enumerate() {
                            if item.get("from").is_none()
                                || item.get("to").is_none()
                                || values[..i]
                                    .iter()
                                    .any(|v| typed_equal(&v["from"], &item["from"]))
                            {
                                return Err(invalid("Invalid or duplicate value_map source"));
                            }
                        }
                    }
                    Some("prefix" | "multiply") => {
                        return Err(invalid("Malformed transform parameters"));
                    }
                    _ => return Err(unsupported("Unknown transform kind")),
                }
            }
        }
    }
    if let Some(filter) = response.get("filter") {
        tokens(
            filter["path"]
                .as_str()
                .ok_or_else(|| invalid("Filter needs a path"))?,
        )?;
        let allowed = filter["in"]
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("Filter allowlist must be nonempty"))?;
        for (i, value) in allowed.iter().enumerate() {
            if allowed[..i].iter().any(|v| typed_equal(v, value)) {
                return Err(invalid("Duplicate filter allowlist value"));
            }
        }
        if let Some(known) = filter.get("known_values") {
            let known = known
                .as_array()
                .ok_or_else(|| invalid("known_values must be an array"))?;
            if allowed
                .iter()
                .any(|v| !known.iter().any(|k| typed_equal(k, v)))
                || known
                    .iter()
                    .enumerate()
                    .any(|(i, v)| known[..i].iter().any(|k| typed_equal(k, v)))
            {
                return Err(invalid("Invalid or duplicate known filter vocabulary"));
            }
        } else if feed["inventory"]["complete"] == true {
            return Err(invalid("Complete filtered inventory requires known_values"));
        }
    }
    Ok(())
}

fn transformed(value: &Value, steps: Option<&Value>) -> Result<Value, RegistryError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let mut value = value.clone();
    let Some(steps) = steps.and_then(Value::as_array) else {
        return Ok(value);
    };
    for step in steps {
        value = match step["kind"].as_str() {
            Some("prefix") => Value::String(format!(
                "{}{}",
                step["value"].as_str().unwrap_or_default(),
                value
                    .as_str()
                    .ok_or_else(|| invalid("prefix requires a string"))?
            )),
            Some("parse_decimal") => {
                let raw = value
                    .as_str()
                    .filter(|s| !s.contains(['e', 'E']))
                    .ok_or_else(|| {
                        invalid("parse_decimal requires a non-exponent decimal string")
                    })?;
                number(decimal(raw)?)?
            }
            Some("multiply") => {
                if !value.is_number() {
                    return Err(invalid("multiply requires a numeric value"));
                }
                number(decimal(&value.to_string())? * decimal(&step["factor"].to_string())?)?
            }
            Some("value_map") => step["values"]
                .as_array()
                .and_then(|values| values.iter().find(|row| typed_equal(&row["from"], &value)))
                .map(|row| row["to"].clone())
                .ok_or_else(|| invalid("value_map has no exact typed match"))?,
            _ => return Err(unsupported("Unknown transform")),
        };
    }
    Ok(value)
}

fn number(value: BigRational) -> Result<Value, RegistryError> {
    if value < BigRational::zero() {
        return Err(invalid("Negative normalized price"));
    }
    let mut numerator = value.numer().clone();
    let denominator = value.denom();
    let integer = &numerator / denominator;
    numerator %= denominator;
    let mut text = integer.to_string();
    if !numerator.is_zero() {
        text.push('.');
        for _ in 0..128 {
            numerator *= 10;
            text.push_str(&(&numerator / denominator).to_string());
            numerator %= denominator;
            if numerator.is_zero() {
                break;
            }
        }
        if !numerator.is_zero() {
            return Err(invalid("Normalized decimal exceeds precision budget"));
        }
    }
    if text.len() > 128 {
        return Err(invalid("Normalized decimal exceeds precision budget"));
    }
    text.parse::<serde_json::Number>()
        .map(Value::Number)
        .map_err(|_| invalid("Normalized value is not a JSON number"))
}

fn without_clears(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), without_clears(v)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_clears).collect()),
        _ => value.clone(),
    }
}

/// Normalize all already fetched pages before any inventory mutation.
///
/// # Errors
/// Any malformed row, unknown lifecycle state, conflicting duplicate identity,
/// mapping failure or unsupported transform rejects the entire observation.
pub fn normalize(feed: &Value, pages: &[Value]) -> Result<Observation, RegistryError> {
    validate(feed)?;
    let authority = writes(feed)?;
    let response = &feed["response"];
    let items_path = response["items_path"].as_str().unwrap_or_default();
    let mapping = response["mapping"]
        .as_object()
        .ok_or_else(|| invalid("Missing mapping"))?;
    let defaults = response
        .get("defaults")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut models = BTreeMap::<String, (Value, ObservedModel)>::new();
    let mut excluded = 0;
    let mut total_bytes = 0usize;
    let mut total_nodes = 0usize;
    for page in pages {
        let items = read(page, items_path)?
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("items_path must resolve to an array"))?;
        for item in items {
            if !item.is_object() {
                return Err(invalid("Model listing item must be an object"));
            }
            if let Some(filter) = response.get("filter") {
                let observed = read(item, filter["path"].as_str().unwrap_or_default())?
                    .filter(|v| !v.is_null())
                    .ok_or_else(|| incomplete("Missing filter value"))?;
                if filter
                    .get("known_values")
                    .and_then(Value::as_array)
                    .is_some_and(|known| !known.iter().any(|v| typed_equal(v, observed)))
                {
                    return Err(incomplete("Unrecognized lifecycle/filter value"));
                }
                if !filter["in"]
                    .as_array()
                    .is_some_and(|allowed| allowed.iter().any(|v| typed_equal(v, observed)))
                {
                    excluded += 1;
                    continue;
                }
            }
            let mut value = defaults.clone();
            let mut observed = BTreeSet::new();
            for (source, destinations_value) in mapping {
                if let Some(input) = read(item, source)? {
                    for target in destinations(destinations_value)? {
                        let output = transformed(
                            input,
                            response.get("transforms").and_then(|v| v.get(target)),
                        )?;
                        known_value(target, &output, false)?;
                        if output.is_null()
                            && matches!(
                                target,
                                "/provider_model_id" | "/canonical_id" | "/status" | "/interfaces"
                            )
                        {
                            return Err(invalid(
                                "Null identity/status/interfaces cannot activate a default",
                            ));
                        }
                        write(&mut value, target, output)?;
                        observed.insert(target.into());
                    }
                }
            }
            if let Some(interfaces) = value.get("interfaces") {
                interface_authority(interfaces, &authority)?;
            }
            let size = serde_json::to_vec(&value)
                .map_err(|_| invalid("Cannot encode normalized observation"))?
                .len();
            total_bytes = total_bytes
                .checked_add(size)
                .filter(|n| *n <= MAX_DOCUMENT_BYTES)
                .ok_or_else(|| invalid("Normalized observation exceeds byte budget"))?;
            total_nodes = total_nodes
                .checked_add(nodes(&value))
                .filter(|n| *n <= MAX_DOCUMENT_NODES)
                .ok_or_else(|| invalid("Normalized observation exceeds value budget"))?;
            let native = value["provider_model_id"]
                .as_str()
                .filter(|v| native_id(v))
                .ok_or_else(|| {
                    invalid("Mapped provider_model_id must be a nonempty native string")
                })?
                .to_owned();
            if !observed.contains("/provider_model_id") {
                return Err(invalid(
                    "Native identity was not observed; defaults cannot invent it",
                ));
            }
            for field in ["canonical_id", "status", "interfaces"] {
                if value.get(field).is_some_and(Value::is_null) {
                    return Err(invalid(
                        "Null identity/status/interfaces are invalid defaults",
                    ));
                }
            }
            if authority.contains("models") {
                if !value["status"].is_string() {
                    return Err(invalid(
                        "Membership rows require mapped status or defaults.status",
                    ));
                }
                if !matches!(
                    value["status"].as_str(),
                    Some("active" | "staging" | "suspended" | "withdrawn")
                ) {
                    return Err(incomplete("Uninterpretable membership lifecycle status"));
                }
                observed.insert("/status".into());
            }
            let mut validated = without_clears(&value);
            let object = validated
                .as_object_mut()
                .ok_or_else(|| invalid("Normalized row must be an object"))?;
            object.entry("status").or_insert(json!("active"));
            object.entry("interfaces").or_insert(json!([]));
            Document::from_value(
                json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":"1970-01-01T00:00:00Z","data":{"models":[],"providers":[{"name":"validation","status":"active","billing":"usage","models":[validated]}]}}),
                &ReaderPolicy::default(),
            )?;
            if let Some((previous, _)) = models.get(&native) {
                if !typed_equal(previous, item) {
                    return Err(invalid(
                        "Conflicting duplicate native identity across listing pages",
                    ));
                }
            } else {
                models.insert(native, (item.clone(), ObservedModel { value, observed }));
            }
        }
    }
    if authority.contains("models") && models.is_empty() && feed["inventory"]["allow_empty"] != true
    {
        return Err(incomplete("Empty inventory is not allowed by this feed"));
    }
    Ok(Observation {
        models: models.into_values().map(|(_, row)| row).collect(),
        excluded,
    })
}

fn invalid(message: &str) -> RegistryError {
    RegistryError::invalid("/auto_sync/feeds", message)
}
fn unsupported(message: &str) -> RegistryError {
    RegistryError {
        code: "registry.sync_unsupported",
        path: "/auto_sync/feeds".into(),
        message: message.into(),
        recovery:
            "Upgrade to an importer supporting this feed contract or repair the feed configuration"
                .into(),
    }
}
fn incomplete(message: &str) -> RegistryError {
    RegistryError {
        code: "registry.sync_incomplete",
        path: "/auto_sync/feeds".into(),
        message: message.into(),
        recovery: "Verify the provider's complete listing and lifecycle contract before retrying"
            .into(),
    }
}

pub(crate) fn nodes(value: &Value) -> usize {
    1 + match value {
        Value::Array(v) => v.iter().map(nodes).sum::<usize>(),
        Value::Object(v) => v.values().map(nodes).sum::<usize>(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_containers_lifecycle_and_expanded_output_are_fail_closed()
    -> Result<(), RegistryError> {
        let mut feed = json!({"kind":"v1_models","writes":["models","metadata"],"inventory":{"complete":true},"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id","/lifecycle":"/status","/interfaces":"/interfaces"},"defaults":{"status":"active","interfaces":[]}}});
        let unknown = json!({"data":[{"id":"A","lifecycle":"future_state"}]});
        assert!(normalize(&feed, &[unknown]).is_err());
        let price = json!({"data":[{"id":"A","interfaces":[{"protocol":"messages","operation":"generation","pricing":{"kind":"token","currency":"USD"}}]}]});
        assert!(normalize(&feed, &[price]).is_err());
        feed["response"]["mapping"] = json!({"/id":"/provider_model_id"});
        feed["response"]["defaults"]["description"] = json!("x".repeat(1024 * 1024));
        let rows = (0..35)
            .map(|i| json!({"id":format!("A{i}")}))
            .collect::<Vec<_>>();
        assert!(normalize(&feed, &[json!({"data":rows})]).is_err());
        let one = crate::document::parse_json(b"1.0")?;
        assert!(typed_equal(&one, &json!(1)));
        assert!(!typed_equal(&one, &json!("1")));
        Ok(())
    }

    fn feed() -> Value {
        json!({"kind":"v1_models","writes":["models","metadata","pricing"],"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id","/input":"/interfaces/0/pricing/input_tokens/no_cache"},"defaults":{"status":"active","interfaces":[{"protocol":"chat_completions","operation":"generation","pricing":{"kind":"token","currency":"USD"}}]},"transforms":{"/interfaces/0/pricing/input_tokens/no_cache":[{"kind":"parse_decimal"},{"kind":"multiply","factor":1000000}]}},"inventory":{"allow_empty":false}})
    }

    #[test]
    fn nested_mapping_is_exact_missing_is_unobserved_and_null_never_defaults()
    -> Result<(), RegistryError> {
        let config = feed();
        let result = normalize(
            &config,
            &[json!({"data":[{"id":"Case/Native","input":"0.00000125"},{"id":"Other"}]})],
        )?;
        assert_eq!(result.models[0].value["provider_model_id"], "Case/Native");
        assert_eq!(
            result.models[0].value["interfaces"][0]["pricing"]["input_tokens"]["no_cache"]
                .to_string(),
            "1.25"
        );
        assert!(
            !result.models[1]
                .observed
                .contains("/interfaces/0/pricing/input_tokens/no_cache")
        );
        assert!(normalize(&config, &[json!({"data":[{"id":null}]})]).is_err());
        assert!(normalize(&config, &[json!({"data":null})]).is_err());
        Ok(())
    }

    #[test]
    fn pointers_do_not_guess_or_evaluate_expressions() -> Result<(), RegistryError> {
        let value = json!({"a/b":{"~":{"":7}},"array":[1]});
        assert_eq!(read(&value, "/a~1b/~0/")?, Some(&json!(7)));
        assert!(read(&value, "/array/01").is_err());
        assert!(read(&value, "/array/0/child").is_err());
        assert!(read(&value, "/missing")?.is_none());
        assert!(tokens("/bad~2escape").is_err());
        assert!(tokens("/__proto__/value").is_err());
        Ok(())
    }

    #[test]
    fn overlapping_maps_unknown_transforms_and_lifecycle_ambiguity_fail() {
        let mut config = feed();
        config["response"]["mapping"]["/whole"] = json!("/interfaces");
        assert!(validate(&config).is_err());
        config = feed();
        config["response"]["transforms"]["/interfaces/0/pricing/input_tokens/no_cache"] =
            json!([{"kind":"script"}]);
        assert!(validate(&config).is_err());
        config = feed();
        config["response"]["filter"] =
            json!({"path":"/state","in":["active"],"known_values":["active","retired"]});
        assert!(normalize(&config, &[json!({"data":[{"id":"a","state":"future"}]})]).is_err());
        assert!(normalize(&config, &[json!({"data":[{"id":"a"}]})]).is_err());
    }
}

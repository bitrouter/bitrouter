//! Bounded, duplicate-key rejecting admission with open extension vocabularies.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use num_traits::ToPrimitive;
use serde::Serialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// Current normative prerelease. Stable support is deliberately opt-in.
pub const SPEC_REVISION: &str = "1.0.0-beta.6";
/// Promoted same-revision stream. Ordinary source PRs do not publish this branch.
pub const DEFAULT_SOURCE_URL: &str = "https://raw.githubusercontent.com/bitrouter/bitrouter/registry-v1/dist/registry/v1/1.0.0-beta.6/registry.json";
/// Bound applies to decoded JSON bytes, including caches and packaged data.
pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
/// Nested JSON depth allowed before constructing a full value tree.
pub const MAX_DOCUMENT_DEPTH: usize = 64;
/// Maximum number of scalar/container values, bounding decoded allocation.
pub const MAX_DOCUMENT_NODES: usize = 1_000_000;
/// Structural schema, also exported with the protocol specification.
pub const SCHEMA: &str = include_str!("../schema/registry-v1.json");

/// An actionable input error; never contains complete response bodies.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{code} at {path}: {message}. {recovery}")]
pub struct RegistryError {
    /// Stable machine-readable diagnostic code.
    pub code: &'static str,
    /// JSON Pointer locating the affected object or field.
    pub path: String,
    /// Sanitized explanation of the failed condition.
    pub message: String,
    /// Corrective action appropriate to the actual failure.
    pub recovery: String,
}

impl RegistryError {
    /// Construct a malformed-data diagnostic without embedding untrusted values.
    pub fn invalid(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: "registry.invalid_data",
            path: path.into(),
            message: message.into(),
            recovery: "Repair the registry source or select a valid registry snapshot".into(),
        }
    }

    fn unsupported(code: &'static str, path: &str, message: String) -> Self {
        Self {
            code,
            path: path.into(),
            message,
            recovery: "Upgrade BitRouter to a version supporting this contract or select a registry revision supported by the installed software".into(),
        }
    }
}

/// Capabilities declared by the installed reader, independently of the document.
#[derive(Debug, Clone)]
pub struct ReaderPolicy {
    /// Whether compatible stable 1.x.y revisions are accepted.
    pub stable_v1: bool,
    /// Exact accepted beta revision strings.
    pub beta_revisions: BTreeSet<String>,
    /// Implemented feature contracts; unknown required features fail closed.
    pub features: BTreeSet<String>,
}

impl Default for ReaderPolicy {
    fn default() -> Self {
        Self {
            stable_v1: false,
            beta_revisions: [SPEC_REVISION.into()].into(),
            features: ["pricing.metered.v1".into()].into(),
        }
    }
}

impl ReaderPolicy {
    /// Whether requirements on this object are implemented by this reader.
    /// Callers must additionally check the enclosing objects' requirements.
    pub fn supports(&self, object: &Value) -> bool {
        object.get("requires").is_none_or(|value| {
            value.as_array().is_some_and(|values| {
                values.iter().all(|feature| {
                    feature
                        .as_str()
                        .is_some_and(|name| self.features.contains(name))
                })
            })
        })
    }

    fn accepts_revision(&self, revision: &str) -> bool {
        if self.beta_revisions.contains(revision) {
            return valid_revision(revision);
        }
        self.stable_v1
            && semver::Version::parse(revision).is_ok_and(|v| v.major == 1 && v.pre.is_empty())
    }
}

fn valid_revision(revision: &str) -> bool {
    semver::Version::parse(revision).is_ok_and(|v| v.major == 1)
}

/// One validated document, retaining every unknown nested field and exact number.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Document(Value);

impl Document {
    /// Parse a bounded complete JSON document and apply the reader's policy.
    ///
    /// # Errors
    /// Returns structured data/compatibility errors; never partially admits data.
    pub fn parse(bytes: &[u8], policy: &ReaderPolicy) -> Result<Self, RegistryError> {
        let value = parse_json(bytes)?;
        Self::from_value(value, policy)
    }

    /// Validate an already parsed value, for staged source builds and fixtures.
    ///
    /// # Errors
    /// Returns structural, identity, or compatibility errors for the candidate.
    pub fn from_value(value: Value, policy: &ReaderPolicy) -> Result<Self, RegistryError> {
        validate_resources(&value)?;
        if value.get("version").and_then(integer) != Some(1) {
            return Err(RegistryError::unsupported(
                "registry.unsupported_version",
                "/version",
                "Expected integer wire version 1".into(),
            ));
        }
        let revision = value
            .get("spec_revision")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RegistryError::invalid("/spec_revision", "Expected a revision string")
            })?;
        if !policy.accepts_revision(revision) {
            return Err(RegistryError::unsupported(
                "registry.unsupported_revision",
                "/spec_revision",
                format!(
                    "Revision is not supported; accepted beta revisions: {:?}; stable v1: {}",
                    policy.beta_revisions, policy.stable_v1
                ),
            ));
        }
        let schema = validator()?;
        if let Err(error) = schema.validate(&value) {
            // A schema error's Display includes the rejected value. Keep only
            // its path here; source input can contain confidential metadata.
            return Err(RegistryError::invalid(
                error.instance_path().to_string(),
                "Field does not satisfy the registry v1 structural schema",
            ));
        }
        if !policy.supports(&value) {
            return Err(RegistryError::unsupported(
                "registry.unsupported_feature",
                "/requires",
                "Document requires an unimplemented feature".into(),
            ));
        }
        validate_semantics(&value)?;
        Ok(Self(value))
    }

    /// The complete admitted value; callers may project supported semantics.
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// Take the retained complete value for a source/cache rewrite.
    pub fn into_value(self) -> Value {
        self.0
    }

    /// Curated recommendation records, independent of provider membership.
    pub fn models(&self) -> &[Value] {
        array(&self.0["data"]["models"])
    }

    /// Complete provider records, including unsupported interfaces and prices.
    pub fn providers(&self) -> &[Value] {
        array(&self.0["data"]["providers"])
    }

    /// Publication evaluation time, never changed by runtime cache operations.
    pub fn as_of(&self) -> DateTime<Utc> {
        // Admission checked this field. Retain a total API even if an internal
        // invariant is accidentally broken by future code.
        timestamp(self.0["as_of"].as_str().unwrap_or_default())
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
    }
}

fn validator() -> Result<&'static jsonschema::Validator, RegistryError> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    VALIDATOR
        .get_or_init(|| {
            let schema: Value = serde_json::from_str(SCHEMA).map_err(|e| e.to_string())?;
            // Local-only schema; no remote references or network retrievers.
            // https://docs.rs/jsonschema/0.47.0/jsonschema/fn.validator_for.html
            jsonschema::validator_for(&schema).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|_| RegistryError::invalid("", "Bundled structural schema is invalid"))
}

/// Parse JSON while rejecting duplicate keys, excessive input size and depth.
///
/// # Errors
/// Returns an ordinary input error for any invalid or oversized document.
pub fn parse_json(bytes: &[u8]) -> Result<Value, RegistryError> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(RegistryError::invalid(
            "",
            "Document exceeds the decoded byte limit",
        ));
    }
    let raw: &serde_json::value::RawValue = serde_json::from_slice(bytes)
        .map_err(|_| RegistryError::invalid("", "Invalid or trailing JSON data"))?;
    let value = value_from_raw(raw, 0, &std::cell::Cell::new(0))?;
    validate_resources(&value)?;
    Ok(value)
}

fn value_from_raw(
    raw: &serde_json::value::RawValue,
    depth: usize,
    nodes: &std::cell::Cell<usize>,
) -> Result<Value, RegistryError> {
    if depth > MAX_DOCUMENT_DEPTH || nodes.get() >= MAX_DOCUMENT_NODES {
        return Err(RegistryError::invalid(
            "",
            "JSON nesting or node limit exceeded",
        ));
    }
    nodes.set(nodes.get() + 1);
    let invalid = || {
        RegistryError::invalid(
            "",
            "Invalid JSON, duplicate key, or exceeded resource budget",
        )
    };
    // Dispatch raw numeric tokens directly. Literal objects using serde_json's
    // private Number key remain ordinary objects, including sibling members.
    // https://docs.rs/serde_json/latest/serde_json/value/struct.RawValue.html
    match raw.get().as_bytes().first() {
        Some(b'{' | b'[') => {
            let mut decoder = serde_json::Deserializer::from_str(raw.get());
            serde::Deserializer::deserialize_any(&mut decoder, RawContainer { depth, nodes })
                .map_err(|_| invalid())
        }
        Some(b'"') => serde_json::from_str::<String>(raw.get())
            .map(Value::String)
            .map_err(|_| invalid()),
        Some(b't' | b'f') => serde_json::from_str::<bool>(raw.get())
            .map(Value::Bool)
            .map_err(|_| invalid()),
        Some(b'n') => Ok(Value::Null),
        Some(_) => raw
            .get()
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .map_err(|_| invalid()),
        None => Err(invalid()),
    }
}

struct RawContainer<'a> {
    depth: usize,
    nodes: &'a std::cell::Cell<usize>,
}

impl<'de> Visitor<'de> for RawContainer<'_> {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON object or array")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut object = serde_json::Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate object key"));
            }
            let raw: &'de serde_json::value::RawValue = access.next_value()?;
            let value = value_from_raw(raw, self.depth + 1, self.nodes)
                .map_err(serde::de::Error::custom)?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(raw) = access.next_element::<&'de serde_json::value::RawValue>()? {
            values.push(
                value_from_raw(raw, self.depth + 1, self.nodes)
                    .map_err(serde::de::Error::custom)?,
            );
        }
        Ok(Value::Array(values))
    }
}

struct ByteBudget(usize);

impl std::io::Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("document byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn validate_resources(value: &Value) -> Result<(), RegistryError> {
    check_depth(value, 0, &mut 0)?;
    serde_json::to_writer(ByteBudget(MAX_DOCUMENT_BYTES), value)
        .map_err(|_| RegistryError::invalid("", "Document exceeds the decoded byte limit"))
}

fn check_depth(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), RegistryError> {
    if depth > MAX_DOCUMENT_DEPTH || *nodes >= MAX_DOCUMENT_NODES {
        return Err(RegistryError::invalid("", "Nesting limit exceeded"));
    }
    *nodes += 1;
    match value {
        Value::Object(map) => {
            for value in map.values() {
                check_depth(value, depth + 1, nodes)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                check_depth(value, depth + 1, nodes)?;
            }
        }
        Value::Number(number) => {
            // Bound exponent allocation before schema evaluation. Both positive
            // and negative huge exponents can exhaust exact arithmetic backends.
            let text = number.to_string();
            let exponent = text.split_once(['e', 'E']).map(|(_, v)| v.parse::<i32>());
            let oversized = text.len() > 128
                || exponent.is_some_and(|n| n.map_or(true, |e| e.unsigned_abs() > 308));
            if oversized || !number.as_f64().is_some_and(f64::is_finite) {
                return Err(RegistryError::invalid(
                    "",
                    "JSON number exceeds the supported precision or exponent range",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// UTC RFC 3339 timestamps must explicitly end in `Z`.
pub fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    value
        .ends_with('Z')
        .then(|| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|v| v.with_timezone(&Utc))
        })
        .flatten()
}

/// Exact canonical grammar; neither case nor provider aliases are normalized.
pub fn canonical_id(value: &str) -> bool {
    value
        .split_once('/')
        .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
        && !value.starts_with("~native/")
        && !value.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Native IDs are opaque UTF-8 text and retain reserved punctuation exactly.
pub fn native_id(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

/// Known protocols have a default profile; unfamiliar protocols keep omission.
pub fn interface_key(value: &Value) -> (String, String, Option<String>) {
    let protocol = value["protocol"].as_str().unwrap_or_default();
    let profile = value
        .get("protocol_profile")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            matches!(
                protocol,
                "chat_completions" | "messages" | "responses" | "decisions"
            )
            .then(|| "default".into())
        });
    (
        protocol.into(),
        value["operation"].as_str().unwrap_or_default().into(),
        profile,
    )
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

/// Convert a mathematically integral nonnegative JSON number exactly to u64.
/// Decimal/exponent spellings are accepted without an intermediate float.
pub fn integer(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    number.as_u64().or_else(|| {
        let rational = crate::pricing::decimal(&number.to_string()).ok()?;
        rational
            .is_integer()
            .then(|| rational.to_integer().to_u64())
            .flatten()
    })
}

fn positive_fields(value: &Value, fields: &[&str], path: &str) -> Result<(), RegistryError> {
    for field in fields {
        if let Some(value) = value.get(*field)
            && !integer(value).is_some_and(|n| n > 0)
        {
            return Err(RegistryError::invalid(
                format!("{path}/{field}"),
                "Expected an exact positive u64 integer",
            ));
        }
    }
    Ok(())
}

fn validate_semantics(value: &Value) -> Result<(), RegistryError> {
    check_timestamp(value, "as_of", "")?;
    let mut ids = BTreeSet::new();
    for (i, model) in array(&value["data"]["models"]).iter().enumerate() {
        let id = model["id"].as_str().unwrap_or_default();
        if !canonical_id(id) || !ids.insert(id) {
            return Err(RegistryError::invalid(
                format!("/data/models/{i}/id"),
                "Invalid or duplicate canonical identity",
            ));
        }
        let base = format!("/data/models/{i}");
        positive_fields(
            model,
            &["max_input_tokens", "max_output_tokens", "context_window"],
            &base,
        )?;
        check_https(
            &model["curation"],
            "source_url",
            &format!("{base}/curation"),
        )?;
        for key in ["checked_at", "expires_at"] {
            check_timestamp(&model["curation"], key, &format!("{base}/curation"))?;
        }
        for key in ["release_date", "knowledge_cutoff"] {
            if let Some(date) = model.get(key).and_then(Value::as_str) {
                let date = if date.len() == 7 {
                    format!("{date}-01")
                } else {
                    date.into()
                };
                if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
                    return Err(RegistryError::invalid(
                        format!("{base}/{key}"),
                        "Invalid calendar date",
                    ));
                }
            }
        }
    }
    let mut providers = BTreeSet::new();
    for (i, provider) in array(&value["data"]["providers"]).iter().enumerate() {
        if !providers.insert(provider["name"].as_str().unwrap_or_default()) {
            return Err(RegistryError::invalid(
                format!("/data/providers/{i}/name"),
                "Duplicate provider identity",
            ));
        }
        check_timestamp(provider, "submitted_at", &format!("/data/providers/{i}"))?;
        for field in ["api_base", "doc_url"] {
            check_https(provider, field, &format!("/data/providers/{i}"))?;
        }
        if let Some(auth) = provider.get("auth") {
            check_auth(auth, &format!("/data/providers/{i}/auth"))?;
        }
        let mut native = BTreeSet::new();
        for (j, model) in array(&provider["models"]).iter().enumerate() {
            let base = format!("/data/providers/{i}/models/{j}");
            check_timestamp(model, "available_until", &base)?;
            if let Some(availability) = model.get("availability") {
                check_timestamp(availability, "checked_at", &format!("{base}/availability"))?;
                check_https(availability, "source_url", &format!("{base}/availability"))?;
            }
            let id = model["provider_model_id"].as_str().unwrap_or_default();
            if !native_id(id) || !native.insert(id) {
                return Err(RegistryError::invalid(
                    format!("{base}/provider_model_id"),
                    "Invalid or duplicate native identity",
                ));
            }
            if let Some(id) = model.get("canonical_id").and_then(Value::as_str)
                && !canonical_id(id)
            {
                return Err(RegistryError::invalid(
                    format!("{base}/canonical_id"),
                    "Invalid canonical identity",
                ));
            }
            let mut interfaces = BTreeSet::new();
            for (k, interface) in array(&model["interfaces"]).iter().enumerate() {
                let at = format!("{base}/interfaces/{k}");
                positive_fields(
                    &interface["limits"],
                    &["max_input_tokens", "max_output_tokens", "context_window"],
                    &format!("{at}/limits"),
                )?;
                positive_fields(
                    &interface["rate_limits"],
                    &["requests_per_minute", "tokens_per_minute"],
                    &format!("{at}/rate_limits"),
                )?;
                check_https(interface, "base_url", &at)?;
                if !interfaces.insert(interface_key(interface)) {
                    return Err(RegistryError::invalid(
                        &at,
                        "Duplicate protocol/operation/profile triple",
                    ));
                }
                if interface["protocol"] == "systemone"
                    && interface.get("protocol_profile").is_none()
                {
                    return Err(RegistryError::invalid(
                        &at,
                        "System One requires an explicit wire profile",
                    ));
                }
                if let Some(effort) = interface.get("reasoning_effort") {
                    if !array(&interface["capabilities"])
                        .iter()
                        .any(|v| v == "reasoning")
                    {
                        return Err(RegistryError::invalid(
                            &at,
                            "Effort levels require verified reasoning capability",
                        ));
                    }
                    if let Some(default) = effort.get("default")
                        && !array(&effort["levels"]).contains(default)
                    {
                        return Err(RegistryError::invalid(
                            &at,
                            "Default effort is not among the declared levels",
                        ));
                    }
                }
                if provider["billing"] == "subscription" && interface.get("pricing").is_some() {
                    return Err(RegistryError::invalid(
                        &at,
                        "Subscription has no core usage tariff",
                    ));
                }
                if let Some(price) = interface.get("pricing") {
                    validate_price(price, &at)?;
                }
            }
        }
    }
    Ok(())
}

fn check_timestamp(value: &Value, field: &str, path: &str) -> Result<(), RegistryError> {
    if let Some(v) = value.get(field)
        && timestamp(v.as_str().unwrap_or_default()).is_none()
    {
        return Err(RegistryError::invalid(
            format!("{path}/{field}"),
            "Expected UTC RFC 3339 timestamp ending in Z",
        ));
    }
    Ok(())
}

fn check_https(value: &Value, field: &str, path: &str) -> Result<(), RegistryError> {
    if let Some(raw) = value.get(field).and_then(Value::as_str) {
        let endpoint = matches!(field, "api_base" | "base_url");
        let valid = url::Url::parse(raw).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && (!endpoint
                    || (url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()))
        });
        if !valid {
            return Err(RegistryError::invalid(
                format!("{path}/{field}"),
                "Expected an absolute HTTPS URL; execution endpoints forbid userinfo and fragments",
            ));
        }
    }
    Ok(())
}

fn check_auth(auth: &Value, path: &str) -> Result<(), RegistryError> {
    let placement = match auth["kind"].as_str() {
        Some("bearer") => Some("authorization"),
        Some("header") => auth.get("header").and_then(Value::as_str),
        _ => None,
    };
    let mut names = BTreeSet::new();
    if let Some(header) = placement {
        let name = http::header::HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| RegistryError::invalid(path, "Invalid credential header name"))?;
        names.insert(name.as_str().to_owned());
    }
    if let Some(headers) = auth.get("extra_headers").and_then(Value::as_object) {
        // HTTP field-name equality is case insensitive.
        // https://www.rfc-editor.org/rfc/rfc9110.html#section-5.1
        for (key, value) in headers {
            let name = http::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| RegistryError::invalid(path, "Invalid constant header name"))?;
            http::header::HeaderValue::from_str(value.as_str().unwrap_or_default())
                .map_err(|_| RegistryError::invalid(path, "Invalid constant header value"))?;
            if !names.insert(name.as_str().to_owned()) {
                return Err(RegistryError::invalid(
                    path,
                    "Case-insensitive duplicate or credential/constant header collision",
                ));
            }
        }
    }
    Ok(())
}

fn validate_price(price: &Value, path: &str) -> Result<(), RegistryError> {
    match price["kind"].as_str() {
        Some("token") => {
            let mut previous = 0;
            for tier in array(&price["context_tiers"]) {
                let threshold = integer(&tier["above_input_tokens"]).unwrap_or_default();
                if threshold <= previous {
                    return Err(RegistryError::invalid(
                        path,
                        "Context thresholds must be positive and strictly ascending",
                    ));
                }
                previous = threshold;
            }
            for rates in std::iter::once(price).chain(array(&price["context_tiers"]).iter()) {
                for (field, known) in [
                    (
                        "input_tokens",
                        &["no_cache", "cache_read", "cache_write"][..],
                    ),
                    ("output_tokens", &["text", "reasoning"][..]),
                ] {
                    if let Some(object) = rates.get(field).and_then(Value::as_object) {
                        for (name, number) in object {
                            if known.contains(&name.as_str()) {
                                crate::pricing::decimal(&number.to_string()).map_err(|_| {
                                    RegistryError::invalid(
                                        path,
                                        "Invalid or excessive decimal rate",
                                    )
                                })?;
                            }
                        }
                    }
                }
            }
        }
        Some("metered") => {
            if !array(&price["requires"])
                .iter()
                .any(|v| v == "pricing.metered.v1")
            {
                return Err(RegistryError::invalid(
                    path,
                    "Metered tariff requires pricing.metered.v1",
                ));
            }
            let mut ids = BTreeSet::new();
            for meter in array(&price["meters"]) {
                positive_fields(meter, &["quantity"], path)?;
                if !ids.insert(meter["id"].as_str().unwrap_or_default()) {
                    return Err(RegistryError::invalid(path, "Duplicate meter identity"));
                }
                crate::pricing::decimal(meter["amount"].as_str().unwrap_or_default()).map_err(
                    |_| RegistryError::invalid(path, "Invalid or excessive meter amount"),
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

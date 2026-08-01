use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TRAJECTORY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryCompleteness {
    Complete,
    Incomplete,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrajectoryEventKind {
    RequestStarted,
    RouteIntentRecorded,
    RequestSettled,
    GuardActivated,
    EpisodeClosed,
}

/// Content-free, bounded evidence attached to one immutable ledger event.
///
/// The ledger accepts only structural counts, categorical state, and already
/// computed digests. It deliberately has no prompt, message, response, tool,
/// or arbitrary JSON field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrajectoryEvidence {
    #[serde(default)]
    pub structural: BTreeMap<String, u64>,
    #[serde(default)]
    pub categorical: BTreeMap<String, String>,
    #[serde(default)]
    pub digests: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrajectoryEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub owner_user_id: String,
    pub episode_id: String,
    pub request_id: Option<String>,
    pub sequence: u64,
    pub kind: TrajectoryEventKind,
    pub evidence: TrajectoryEvidence,
    pub captured_at: String,
    pub content_digest: String,
}

/// An already-keyed, HMAC-shaped correlation token.
///
/// This constructor validates a Task-2-compatible wire shape without ever
/// accepting or hashing message content in the Task 1 ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct KeyedDigest {
    value: String,
    key_id: String,
}

impl KeyedDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let Some((algorithm, remainder)) = value.split_once(':') else {
            anyhow::bail!("keyed digest must include its algorithm and key id")
        };
        if algorithm != "hmac-sha256" {
            anyhow::bail!("keyed digest must use hmac-sha256")
        }
        let Some((key_id, hex_digest)) = remainder.split_once(':') else {
            anyhow::bail!("keyed digest must include its key id and digest")
        };
        if remainder.matches(':').count() != 1 {
            anyhow::bail!("keyed digest must contain exactly one key id")
        }
        validate_keyed_component(key_id, "keyed digest key id")?;
        if hex_digest.len() != 64
            || !hex_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            anyhow::bail!("keyed digest must contain 64 lowercase hexadecimal digits")
        }
        let key_id = key_id.to_owned();
        Ok(Self { value, key_id })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

impl TryFrom<String> for KeyedDigest {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}

impl From<KeyedDigest> for String {
    fn from(value: KeyedDigest) -> Self {
        value.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpisodeStart {
    pub episode_id: String,
    /// Opaque, already-keyed correlation material. Task 1 never hashes it.
    pub correlation_digest: KeyedDigest,
    pub correlation_key_id: String,
    pub correlation_source: String,
    pub completeness: HistoryCompleteness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Started,
    Settled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginRequest {
    pub episode: EpisodeStart,
    pub event: TrajectoryEvent,
    /// Opaque, already-keyed full-input correlation value.
    pub full_input_digest: KeyedDigest,
    pub native_parent_id: Option<String>,
    pub protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboxPayload {
    #[serde(default)]
    pub structural: BTreeMap<String, u64>,
    #[serde(default)]
    pub digests: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboxWrite {
    pub outbox_id: String,
    pub topic: String,
    pub payload: OutboxPayload,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settlement {
    pub event: TrajectoryEvent,
    pub status: RequestStatus,
    pub outbox: Option<OutboxWrite>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRequest {
    pub request_id: String,
    pub episode_id: String,
    pub start_event_id: String,
    pub settlement_event_id: Option<String>,
    pub full_input_digest: KeyedDigest,
    pub native_parent_id: Option<String>,
    pub protocol: String,
    pub status: RequestStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOutbox {
    pub outbox_id: String,
    pub topic: String,
    pub payload_json: String,
    pub payload_digest: String,
    pub attempts: u64,
    pub created_at: String,
}

impl TrajectoryEvent {
    pub fn semantic_digest(&self) -> Result<String> {
        let input = EventDigestInput {
            schema_version: self.schema_version,
            event_id: &self.event_id,
            owner_user_id: &self.owner_user_id,
            episode_id: &self.episode_id,
            request_id: self.request_id.as_deref(),
            sequence: self.sequence,
            kind: self.kind,
            evidence: &self.evidence,
            captured_at: &self.captured_at,
        };
        canonical_digest(&input)
    }
}

#[derive(Serialize)]
struct EventDigestInput<'a> {
    schema_version: u32,
    event_id: &'a str,
    owner_user_id: &'a str,
    episode_id: &'a str,
    request_id: Option<&'a str>,
    sequence: u64,
    kind: TrajectoryEventKind,
    evidence: &'a TrajectoryEvidence,
    captured_at: &'a str,
}

pub fn validate_event(event: &TrajectoryEvent) -> Result<()> {
    if event.schema_version != TRAJECTORY_SCHEMA_VERSION {
        anyhow::bail!("unsupported trajectory event schema version")
    }
    validate_identifier(&event.event_id, "event_id")?;
    validate_identifier(&event.owner_user_id, "owner_user_id")?;
    validate_identifier(&event.episode_id, "episode_id")?;
    if let Some(request_id) = &event.request_id {
        validate_identifier(request_id, "request_id")?;
    }
    if event.sequence == 0 {
        anyhow::bail!("event sequence must be positive")
    }
    chrono::DateTime::parse_from_rfc3339(&event.captured_at)
        .context("captured_at must be RFC3339")?;
    validate_evidence(&event.evidence)?;
    validate_digest(&event.content_digest, "content_digest")?;
    if event.semantic_digest()? != event.content_digest {
        anyhow::bail!("trajectory event content_digest does not match its canonical content")
    }
    Ok(())
}

pub fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    let canonical = serde_json::to_vec(value).context("serializing canonical trajectory value")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

fn validate_evidence(evidence: &TrajectoryEvidence) -> Result<()> {
    validate_structured_attributes(
        &evidence.structural,
        &evidence.categorical,
        &evidence.digests,
        "trajectory evidence",
    )
}

pub fn validate_outbox_payload(payload: &OutboxPayload) -> Result<()> {
    if payload.structural.len() > 64 || payload.digests.len() > 64 {
        anyhow::bail!("trajectory outbox payload has too many attributes")
    }
    for key in payload.structural.keys().chain(payload.digests.keys()) {
        validate_attribute_key(key)?;
        if attribute_key_looks_sensitive(key) {
            anyhow::bail!("trajectory outbox payload contains credential-shaped attribute material")
        }
    }
    for (key, digest) in &payload.digests {
        validate_digest(digest, &format!("trajectory outbox payload digest '{key}'"))?;
    }
    Ok(())
}

fn validate_structured_attributes(
    structural: &BTreeMap<String, u64>,
    categorical: &BTreeMap<String, String>,
    digests: &BTreeMap<String, String>,
    field: &str,
) -> Result<()> {
    if structural.len() > 64 || categorical.len() > 64 || digests.len() > 64 {
        anyhow::bail!("{field} has too many attributes")
    }
    for key in structural
        .keys()
        .chain(categorical.keys())
        .chain(digests.keys())
    {
        validate_attribute_key(key)?;
        if attribute_key_looks_sensitive(key) {
            anyhow::bail!("{field} contains credential-shaped attribute material")
        }
    }
    for (key, value) in categorical {
        if value.trim().is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
            anyhow::bail!("{field} categorical attribute '{key}' must be bounded")
        }
        if attribute_looks_sensitive(key, value) {
            anyhow::bail!("{field} contains credential-shaped attribute material")
        }
    }
    for (key, digest) in digests {
        validate_evidence_digest(digest, &format!("{field} digest '{key}'"))?;
    }
    Ok(())
}

fn validate_evidence_digest(value: &str, field: &str) -> Result<()> {
    if value.starts_with("sha256:") {
        validate_digest(value, field)
    } else {
        KeyedDigest::parse(value.to_owned())
            .with_context(|| format!("{field} must be a digest"))?;
        Ok(())
    }
}

fn validate_identifier(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be a non-empty bounded identifier")
    }
    Ok(())
}

fn validate_attribute_key(value: &str) -> Result<()> {
    if value.len() > 128
        || !value.contains('.')
        || value.starts_with('.')
        || value.ends_with('.')
        || value.chars().any(|character| {
            !(character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-'))
        })
    {
        anyhow::bail!("trajectory evidence attribute '{value}' must be a lowercase namespaced id")
    }
    Ok(())
}

pub(crate) fn validate_keyed_component(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().any(|character| {
            !(character.is_ascii_lowercase()
                || character.is_ascii_uppercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-'))
        })
    {
        anyhow::bail!("{field} must be a bounded token")
    }
    Ok(())
}

pub(crate) fn validate_digest(value: &str, field: &str) -> Result<()> {
    let Some(hex_digest) = value.strip_prefix("sha256:") else {
        anyhow::bail!("{field} must be a sha256 digest")
    };
    if hex_digest.len() != 64
        || !hex_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("{field} must contain 64 lowercase hexadecimal digits")
    }
    Ok(())
}

fn attribute_looks_sensitive(key: &str, value: &str) -> bool {
    attribute_key_looks_sensitive(key)
        || value.to_ascii_lowercase().starts_with("bearer ")
        || value.to_ascii_lowercase().contains("brvk_")
        || value.to_ascii_lowercase().starts_with("sk-")
        || value
            .to_ascii_lowercase()
            .contains("-----begin private key-----")
}

fn attribute_key_looks_sensitive(key: &str) -> bool {
    let normalized_key = key.to_ascii_lowercase().replace(['-', '.'], "_");
    matches!(
        normalized_key.as_str(),
        "authorization"
            | "proxy_authorization"
            | "x_api_key"
            | "api_key"
            | "access_token"
            | "refresh_token"
            | "cookie"
            | "set_cookie"
            | "secret"
    ) || normalized_key.ends_with("_secret")
        || normalized_key.ends_with("_api_key")
        || normalized_key.ends_with("_access_token")
        || normalized_key.ends_with("_refresh_token")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn event_validation_rejects_invalid_wire_values_and_tampering() {
        let mut event = event_fixture();
        event.schema_version = TRAJECTORY_SCHEMA_VERSION + 1;
        assert!(validate_event(&event).is_err());

        let mut event = event_fixture();
        event.event_id = " ".into();
        assert!(validate_event(&event).is_err());

        let mut event = event_fixture();
        event.event_id = "x".repeat(513);
        assert!(validate_event(&event).is_err());

        let mut event = event_fixture();
        event.captured_at = "not-a-timestamp".into();
        assert!(validate_event(&event).is_err());

        let mut event = event_fixture();
        event.content_digest = "sha256:bad".into();
        assert!(validate_event(&event).is_err());
    }

    #[test]
    fn canonical_event_digest_is_stable_and_excludes_its_own_digest() -> anyhow::Result<()> {
        let event = event_fixture();
        let digest = event.semantic_digest()?;
        assert_eq!(digest, event.content_digest);
        assert!(digest.starts_with("sha256:"));
        Ok(())
    }

    #[test]
    fn evidence_rejects_credential_shaped_categorical_attributes() {
        let mut event = event_fixture();
        event
            .evidence
            .categorical
            .insert("request.api_key".into(), "Bearer brvk_super-secret".into());
        event.content_digest = event.semantic_digest().unwrap_or_default();

        assert!(validate_event(&event).is_err());
    }

    #[test]
    fn keyed_correlation_and_outbox_payload_reject_raw_or_sensitive_material() {
        assert!(KeyedDigest::parse("a raw user prompt").is_err());
        assert!(
            KeyedDigest::parse(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
        assert!(
            KeyedDigest::parse(
                "sha256:key-1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .is_err()
        );
        assert!(KeyedDigest::parse("hmac-sha256:key-1:short").is_err());
        assert!(
            KeyedDigest::parse(
                "hmac-sha256:key-1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
            .is_ok()
        );

        let payload = OutboxPayload {
            structural: BTreeMap::from([("trajectory.request_count".into(), 1)]),
            digests: BTreeMap::from([(
                "trajectory.event".into(),
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            )]),
        };
        assert!(validate_outbox_payload(&payload).is_ok());

        let mut keyed_outbox = payload.clone();
        keyed_outbox.digests.insert(
            "trajectory.event".into(),
            "hmac-sha256:key-1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
        );
        assert!(validate_outbox_payload(&keyed_outbox).is_err());

        let mut sensitive = payload;
        sensitive.structural.insert("trajectory.api_key".into(), 1);
        assert!(validate_outbox_payload(&sensitive).is_err());
    }

    #[test]
    fn event_evidence_accepts_valid_keyed_digests_but_rejects_mislabeled_values() {
        let mut event = event_fixture();
        event.evidence.digests.insert(
            "correlation.native_parent".into(),
            "hmac-sha256:key-1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
        );
        event.content_digest = event.semantic_digest().unwrap_or_default();
        assert!(validate_event(&event).is_ok());

        event.evidence.digests.insert(
            "correlation.native_parent".into(),
            "hmac-sha256:key-1:short".into(),
        );
        event.content_digest = event.semantic_digest().unwrap_or_default();
        assert!(validate_event(&event).is_err());
    }

    #[test]
    fn evidence_rejects_unbounded_attribute_keys() {
        let mut evidence = event_fixture().evidence;
        evidence
            .structural
            .insert(format!("{}.count", "x".repeat(128)), 1);

        assert!(validate_evidence(&evidence).is_err());
    }

    fn event_fixture() -> TrajectoryEvent {
        let evidence = TrajectoryEvidence {
            structural: BTreeMap::from([("request.input_count".into(), 1)]),
            categorical: BTreeMap::from([("request.protocol".into(), "responses".into())]),
            digests: BTreeMap::from([(
                "request.input".into(),
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            )]),
        };
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "event-1".into(),
            owner_user_id: "owner-1".into(),
            episode_id: "episode-1".into(),
            request_id: Some("request-1".into()),
            sequence: 1,
            kind: TrajectoryEventKind::RequestStarted,
            evidence,
            captured_at: "2026-08-01T00:00:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest().unwrap_or_default();
        event
    }
}

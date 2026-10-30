//! HTTP-only capability derived from an acknowledged 031 pin. No root-key API.
use super::super::{Crypto, PinnedCrypto, bounded_json};
use crate::{
    gateway::{
        model::{protocol::responses::CausalPrefixCommitment, target::AuthScheme},
        providers::auth::ContinuationAuthority,
    },
    runtime_crypto::SealedSecret,
};
use anyhow::{Result, ensure};
use bitrouter_ai::{
    tenancy::{
        authority::ResourceKind,
        configuration::ResourceRef,
        identity::{AttemptId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId},
    },
    types::ReasoningEffort,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

pub(crate) const PLAIN_LIMIT: usize = 16 * 1024;
pub(crate) const CELL_LIMIT: usize = 32 * 1024;
pub(crate) const VERSION: i32 = 2;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    pub(in crate::tenancy) installation: InstallationId,
    pub(in crate::tenancy) tenant: TenantId,
    pub(in crate::tenancy) principal: PrincipalId,
    pub(in crate::tenancy) route: ResourceRef,
}
impl Owner {
    pub(crate) fn route(&self) -> &ResourceRef {
        &self.route
    }
    pub(crate) fn tenant(&self) -> &TenantId {
        &self.tenant
    }
    pub(crate) fn principal(&self) -> &PrincipalId {
        &self.principal
    }
    pub(crate) fn installation(&self) -> &InstallationId {
        &self.installation
    }
}

/// Metadata is authenticated before the payload is interpreted. The public
/// route lives only in the sealed payload and keyed owner identity.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Metadata {
    pub installation_id: String,
    pub tenant_id: String,
    pub creator_principal_id: String,
    pub continuation_identity: String,
    pub owner_identity: String,
    pub target_fingerprint: String,
    pub key_id: String,
    pub cipher_version: i32,
    pub created_at: String,
    pub expires_at: String,
    pub purge_after: String,
    pub publication_state: String,
    pub publication_generation: String,
    pub publication_instance_id: String,
    pub publication_lease_until: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub candidate: String,
    pub credential: ResourceRef,
    pub credential_tag: String,
    pub authority: String,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Live {
    pub binding: Binding,
    pub revision: RevisionId,
    pub execution: ExecutionId,
    pub attempt: AttemptId,
    pub provider_response_id: String,
    pub effective_model: String,
    pub effective_effort: Option<ReasoningEffort>,
    pub causal_prefix: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Payload {
    pub format: u8,
    pub owner: Owner,
    pub public_id: String,
    // None is an authenticated tombstone. Target binding remains in metadata;
    // it is never usable without a live body and a recomputed matching tag.
    pub live: Option<Live>,
}

#[derive(Clone)]
pub(crate) struct HttpCrypto(PinnedCrypto);
impl PinnedCrypto {
    pub(crate) fn http(&self) -> HttpCrypto {
        HttpCrypto(self.clone())
    }
}
impl HttpCrypto {
    fn codec(&self) -> Codec<'_> {
        Codec(&self.0)
    }
    pub(crate) fn installation(&self) -> &InstallationId {
        &self.0.installation
    }
    pub(crate) fn id(&self) -> &str {
        self.0.id()
    }
    pub(crate) fn identity(&self, owner: &Owner, public_id: &str) -> Result<String> {
        self.codec().identity(owner, public_id)
    }
    pub(crate) fn owner_tag(&self, owner: &Owner) -> Result<String> {
        self.codec().tag("continuation.http.owner.v1", owner)
    }
    pub(crate) fn candidate_tag(&self, projection: &[u8]) -> Result<String> {
        ensure!(
            projection.len() <= 1024 * 1024,
            "HTTP candidate exceeds limit"
        );
        self.0.key.fingerprint(
            self.installation().as_str(),
            "continuation.http.candidate.v1",
            projection,
        )
    }
    pub(crate) fn credential_tag(&self, reference: &ResourceRef) -> Result<String> {
        self.codec()
            .tag("continuation.http.credential.v1", reference)
    }
    pub(crate) fn authority_tag(&self, authority: &ContinuationAuthority) -> Result<String> {
        let scheme = match authority.effective_scheme() {
            AuthScheme::Bearer => "bearer",
            AuthScheme::XApiKey => "x-api-key",
        };
        self.codec().tag(
            "continuation.http.authority.v1",
            &(scheme, authority.credential().proof_bytes()),
        )
    }
    pub(crate) fn target_tag(&self, binding: &Binding) -> Result<String> {
        self.codec().tag("continuation.http.target.v1", binding)
    }
    pub(crate) fn seal(&self, metadata: &Metadata, payload: &Payload) -> Result<String> {
        self.codec().seal(metadata, payload)
    }
    pub(crate) fn open(&self, metadata: &Metadata, ciphertext: &str) -> Result<Payload> {
        self.codec().open(metadata, ciphertext)
    }
}

struct Codec<'a>(&'a Crypto);
impl Codec<'_> {
    fn tag(&self, domain: &str, value: &impl Serialize) -> Result<String> {
        let bytes = bounded_json(value, PLAIN_LIMIT)?;
        self.0
            .key
            .fingerprint(self.0.installation.as_str(), domain, &bytes)
    }
    fn identity(&self, owner: &Owner, public_id: &str) -> Result<String> {
        validate_public(public_id)?;
        self.tag("continuation.http.identity.v1", &(owner, public_id))
    }
    fn validate(&self, m: &Metadata, p: &Payload) -> Result<()> {
        validate_metadata(m)?;
        ensure!(
            p.owner.route.resource_kind == ResourceKind::Route,
            "HTTP route kind invalid"
        );
        ensure!(
            p.format == 1 && p.owner.installation == self.0.installation && m.key_id == self.0.id(),
            "HTTP continuation identity unavailable"
        );
        ensure!(
            m.installation_id == p.owner.installation.as_str()
                && m.tenant_id == p.owner.tenant.as_str()
                && m.creator_principal_id == p.owner.principal.as_str(),
            "HTTP continuation owner mismatch"
        );
        ensure!(
            m.owner_identity == self.tag("continuation.http.owner.v1", &p.owner)?
                && m.continuation_identity == self.identity(&p.owner, &p.public_id)?,
            "HTTP continuation identity mismatch"
        );
        if let Some(live) = &p.live {
            ensure!(
                live.binding.credential.resource_kind == ResourceKind::Credential,
                "HTTP credential kind invalid"
            );
            ensure!(
                !live.provider_response_id.is_empty()
                    && live.provider_response_id.len() <= 512
                    && !live
                        .provider_response_id
                        .bytes()
                        .any(|b| b.is_ascii_control()),
                "HTTP provider identifier invalid"
            );
            ensure!(
                !live.effective_model.is_empty() && live.effective_model.len() <= 512,
                "HTTP effective model invalid"
            );
            for value in [
                &live.binding.candidate,
                &live.binding.credential_tag,
                &live.binding.authority,
            ] {
                ensure!(tag(value), "HTTP continuation binding invalid");
            }
            ensure!(
                live.binding.credential_tag
                    == self.tag("continuation.http.credential.v1", &live.binding.credential)?
                    && m.target_fingerprint
                        == self.tag("continuation.http.target.v1", &live.binding)?,
                "HTTP continuation binding mismatch"
            );
            if let Some(prefix) = &live.causal_prefix {
                ensure!(
                    CausalPrefixCommitment::parse(prefix).is_some(),
                    "HTTP prefix invalid"
                );
            }
        } else {
            ensure!(
                m.publication_state == "active",
                "HTTP tombstone must be active"
            );
        }
        Ok(())
    }
    fn seal(&self, m: &Metadata, p: &Payload) -> Result<String> {
        self.validate(m, p)?;
        let plain = bounded_json(p, PLAIN_LIMIT)?;
        let sealed = self.0.key.seal_bound("continuation.http.v1", m, &plain)?;
        Ok(String::from_utf8(bounded_json(&sealed, CELL_LIMIT)?)?)
    }
    fn open(&self, m: &Metadata, ciphertext: &str) -> Result<Payload> {
        validate_metadata(m)?;
        ensure!(
            m.installation_id == self.0.installation.as_str()
                && m.key_id == self.0.id()
                && ciphertext.len() <= CELL_LIMIT,
            "HTTP ciphertext unavailable"
        );
        let sealed: SealedSecret = serde_json::from_str(ciphertext)?;
        let plain = self.0.key.open_bound("continuation.http.v1", m, &sealed)?;
        ensure!(plain.len() <= PLAIN_LIMIT, "HTTP plaintext exceeds limit");
        let payload: Payload = serde_json::from_slice(&plain)?;
        self.validate(m, &payload)?;
        Ok(payload)
    }
}
fn tag(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}
pub(crate) fn time(value: &str) -> Result<DateTime<Utc>> {
    ensure!(
        value.len() == 27 && value.is_ascii(),
        "HTTP timestamp invalid"
    );
    let parsed = DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc);
    ensure!(timestamp(parsed) == value, "HTTP timestamp noncanonical");
    Ok(parsed)
}
pub(crate) fn validate_public(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 512
            && crate::gateway::model::protocol::responses::decode_gateway_continuation_id(value)?
                .is_some(),
        "HTTP public identifier invalid"
    );
    Ok(())
}
fn validate_metadata(m: &Metadata) -> Result<()> {
    let _: InstallationId = m.installation_id.clone().try_into()?;
    let _: TenantId = m.tenant_id.clone().try_into()?;
    let _: PrincipalId = m.creator_principal_id.clone().try_into()?;
    for value in [
        &m.continuation_identity,
        &m.owner_identity,
        &m.target_fingerprint,
        &m.key_id,
    ] {
        ensure!(tag(value), "HTTP metadata tag invalid");
    }
    ensure!(
        m.cipher_version == VERSION
            && matches!(
                m.publication_state.as_str(),
                "provisional" | "delivering" | "active"
            ),
        "HTTP metadata version/state invalid"
    );
    for value in [&m.publication_generation, &m.publication_instance_id] {
        ensure!(
            uuid::Uuid::parse_str(value).is_ok() && value.len() == 36,
            "HTTP publication identity invalid"
        );
    }
    let created = time(&m.created_at)?;
    let expires = time(&m.expires_at)?;
    let purge = time(&m.purge_after)?;
    time(&m.publication_lease_until)?;
    ensure!(
        created < expires && expires < purge,
        "HTTP retention invalid"
    );
    Ok(())
}

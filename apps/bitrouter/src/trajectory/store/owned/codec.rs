//! Strict physical/logical projection codecs. No metadata creates authority.
use super::super::{
    episode_entity as old_episode, outbox_entity as old_outbox, prefix_entity as old_prefix,
    request_entity as old_request,
};
use super::{OwnedStore, entities::*, io};
use crate::storage::scope::{KeyDomain, MetadataRef, Provenance};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u8,
    ids: BTreeMap<String, Option<String>>,
}
fn identity(ids: &[(&str, Option<String>)]) -> Result<String> {
    let value = Identity {
        version: 1,
        ids: ids.iter().map(|(k, v)| ((*k).into(), v.clone())).collect(),
    };
    io::json_size(&value, 8192)?;
    Ok(serde_json::to_string(&value)?)
}
fn decode_identity(json: Option<&str>, names: &[&str]) -> Result<Identity> {
    let json = json.context("missing trajectory logical identity")?;
    ensure!(json.len() <= 8192, "trajectory identity exceeds byte bound");
    let value: Identity = serde_json::from_str(json)?;
    ensure!(
        value.version == 1
            && value.ids.len() == names.len()
            && names.iter().all(|n| value.ids.contains_key(*n)),
        "invalid trajectory identity shape"
    );
    Ok(value)
}
fn logical(identity: &Identity, name: &str) -> Result<String> {
    identity
        .ids
        .get(name)
        .and_then(Option::as_ref)
        .cloned()
        .context("missing required logical trajectory key")
}
fn optional(identity: &Identity, name: &str) -> Result<Option<String>> {
    identity
        .ids
        .get(name)
        .cloned()
        .context("missing logical trajectory key")
}
impl OwnedStore {
    pub(super) fn encode_episode(
        &self,
        row: &old_episode::Model,
        provenance: &Provenance,
    ) -> Result<episode_entity::Model> {
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory owner mismatch"
        );
        let meta = self.scope.metadata(provenance)?;
        let logical_identity_json = Some(identity(&[
            ("episode_id", Some(row.episode_id.clone())),
            ("latest_request_id", row.latest_request_id.clone()),
            ("correlation_digest", Some(row.correlation_digest.clone())),
            ("correlation_key_id", Some(row.correlation_key_id.clone())),
            ("correlation_source", Some(row.correlation_source.clone())),
            (
                "history_completeness",
                Some(row.history_completeness.clone()),
            ),
            ("next_sequence", Some(row.next_sequence.to_string())),
            ("first_captured_at", Some(row.first_captured_at.clone())),
            ("last_captured_at", Some(row.last_captured_at.clone())),
            ("closed_at", row.closed_at.clone()),
        ])?);
        Ok(episode_entity::Model {
            episode_id: self.key(KeyDomain::TrajectoryEpisode, &row.episode_id)?,
            owner_user_id: row.owner_user_id.clone(),
            correlation_digest: row.correlation_digest.clone(),
            correlation_key_id: row.correlation_key_id.clone(),
            correlation_source: row.correlation_source.clone(),
            history_completeness: row.history_completeness.clone(),
            next_sequence: row.next_sequence,
            first_captured_at: row.first_captured_at.clone(),
            last_captured_at: row.last_captured_at.clone(),
            closed_at: row.closed_at.clone(),
            latest_request_id: row
                .latest_request_id
                .as_deref()
                .map(|v| self.key(KeyDomain::TrajectoryRequest, v))
                .transpose()?,
            installation_id: meta.installation,
            tenant_id: meta.tenant,
            creator_principal_id: meta.creator,
            logical_identity_json,
        })
    }
    pub(super) fn decode_episode(
        &self,
        row: episode_entity::Model,
    ) -> Result<(old_episode::Model, Provenance)> {
        let provenance = self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory compatibility owner mismatch"
        );
        let identity = decode_identity(
            row.logical_identity_json.as_deref(),
            &[
                "episode_id",
                "latest_request_id",
                "correlation_digest",
                "correlation_key_id",
                "correlation_source",
                "history_completeness",
                "next_sequence",
                "first_captured_at",
                "last_captured_at",
                "closed_at",
            ],
        )?;
        let episode_id = logical(&identity, "episode_id")?;
        ensure!(
            row.episode_id == self.key(KeyDomain::TrajectoryEpisode, &episode_id)?,
            "trajectory episode_id projection mismatch"
        );
        let latest_request_id = optional(&identity, "latest_request_id")?;
        ensure!(
            row.latest_request_id
                == latest_request_id
                    .as_deref()
                    .map(|v| self.key(KeyDomain::TrajectoryRequest, v))
                    .transpose()?,
            "trajectory latest_request_id projection mismatch"
        );
        ensure!(
            logical(&identity, "correlation_digest")? == row.correlation_digest,
            "trajectory correlation_digest projection mismatch"
        );
        ensure!(
            logical(&identity, "correlation_key_id")? == row.correlation_key_id,
            "trajectory correlation_key_id projection mismatch"
        );
        ensure!(
            logical(&identity, "correlation_source")? == row.correlation_source,
            "trajectory correlation_source projection mismatch"
        );
        ensure!(
            logical(&identity, "history_completeness")? == row.history_completeness,
            "trajectory history_completeness projection mismatch"
        );
        ensure!(
            logical(&identity, "next_sequence")? == row.next_sequence.to_string(),
            "trajectory next_sequence projection mismatch"
        );
        ensure!(
            logical(&identity, "first_captured_at")? == row.first_captured_at,
            "trajectory first_captured_at projection mismatch"
        );
        ensure!(
            logical(&identity, "last_captured_at")? == row.last_captured_at,
            "trajectory last_captured_at projection mismatch"
        );
        ensure!(
            optional(&identity, "closed_at")? == row.closed_at,
            "trajectory closed_at projection mismatch"
        );
        Ok((
            old_episode::Model {
                episode_id,
                owner_user_id: row.owner_user_id,
                correlation_digest: row.correlation_digest,
                correlation_key_id: row.correlation_key_id,
                correlation_source: row.correlation_source,
                history_completeness: row.history_completeness,
                next_sequence: row.next_sequence,
                first_captured_at: row.first_captured_at,
                last_captured_at: row.last_captured_at,
                closed_at: row.closed_at,
                latest_request_id,
            },
            provenance,
        ))
    }
    pub(super) fn encode_request(
        &self,
        row: &old_request::Model,
        provenance: &Provenance,
    ) -> Result<request_entity::Model> {
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory owner mismatch"
        );
        let meta = self.scope.metadata(provenance)?;
        let logical_identity_json = Some(identity(&[
            ("request_id", Some(row.request_id.clone())),
            ("episode_id", Some(row.episode_id.clone())),
            ("start_event_id", Some(row.start_event_id.clone())),
            ("settlement_event_id", row.settlement_event_id.clone()),
            ("settlement_outbox_id", row.settlement_outbox_id.clone()),
            ("native_parent_id", row.native_parent_id.clone()),
            ("full_input_digest", Some(row.full_input_digest.clone())),
            ("protocol", Some(row.protocol.clone())),
            ("status", Some(row.status.clone())),
        ])?);
        Ok(request_entity::Model {
            request_id: self.key(KeyDomain::TrajectoryRequest, &row.request_id)?,
            owner_user_id: row.owner_user_id.clone(),
            episode_id: self.key(KeyDomain::TrajectoryEpisode, &row.episode_id)?,
            start_event_id: self.key(KeyDomain::TrajectoryEvent, &row.start_event_id)?,
            settlement_event_id: row
                .settlement_event_id
                .as_deref()
                .map(|v| self.key(KeyDomain::TrajectoryEvent, v))
                .transpose()?,
            settlement_outbox_id: row
                .settlement_outbox_id
                .as_deref()
                .map(|v| self.key(KeyDomain::TrajectoryOutbox, v))
                .transpose()?,
            full_input_digest: row.full_input_digest.clone(),
            native_parent_id: row
                .native_parent_id
                .as_deref()
                .map(|v| self.key(KeyDomain::TrajectoryRequest, v))
                .transpose()?,
            protocol: row.protocol.clone(),
            status: row.status.clone(),
            installation_id: meta.installation,
            tenant_id: meta.tenant,
            creator_principal_id: meta.creator,
            logical_identity_json,
        })
    }
    pub(super) fn decode_request(
        &self,
        row: request_entity::Model,
    ) -> Result<(old_request::Model, Provenance)> {
        let provenance = self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory compatibility owner mismatch"
        );
        let identity = decode_identity(
            row.logical_identity_json.as_deref(),
            &[
                "request_id",
                "episode_id",
                "start_event_id",
                "settlement_event_id",
                "settlement_outbox_id",
                "native_parent_id",
                "full_input_digest",
                "protocol",
                "status",
            ],
        )?;
        let request_id = logical(&identity, "request_id")?;
        ensure!(
            row.request_id == self.key(KeyDomain::TrajectoryRequest, &request_id)?,
            "trajectory request_id projection mismatch"
        );
        let episode_id = logical(&identity, "episode_id")?;
        ensure!(
            row.episode_id == self.key(KeyDomain::TrajectoryEpisode, &episode_id)?,
            "trajectory episode_id projection mismatch"
        );
        let start_event_id = logical(&identity, "start_event_id")?;
        ensure!(
            row.start_event_id == self.key(KeyDomain::TrajectoryEvent, &start_event_id)?,
            "trajectory start_event_id projection mismatch"
        );
        let settlement_event_id = optional(&identity, "settlement_event_id")?;
        ensure!(
            row.settlement_event_id
                == settlement_event_id
                    .as_deref()
                    .map(|v| self.key(KeyDomain::TrajectoryEvent, v))
                    .transpose()?,
            "trajectory settlement_event_id projection mismatch"
        );
        let settlement_outbox_id = optional(&identity, "settlement_outbox_id")?;
        ensure!(
            row.settlement_outbox_id
                == settlement_outbox_id
                    .as_deref()
                    .map(|v| self.key(KeyDomain::TrajectoryOutbox, v))
                    .transpose()?,
            "trajectory settlement_outbox_id projection mismatch"
        );
        let native_parent_id = optional(&identity, "native_parent_id")?;
        ensure!(
            row.native_parent_id
                == native_parent_id
                    .as_deref()
                    .map(|v| self.key(KeyDomain::TrajectoryRequest, v))
                    .transpose()?,
            "trajectory native_parent_id projection mismatch"
        );
        ensure!(
            logical(&identity, "full_input_digest")? == row.full_input_digest,
            "trajectory full_input_digest projection mismatch"
        );
        ensure!(
            logical(&identity, "protocol")? == row.protocol,
            "trajectory protocol projection mismatch"
        );
        ensure!(
            logical(&identity, "status")? == row.status,
            "trajectory status projection mismatch"
        );
        Ok((
            old_request::Model {
                request_id,
                owner_user_id: row.owner_user_id,
                episode_id,
                start_event_id,
                settlement_event_id,
                settlement_outbox_id,
                full_input_digest: row.full_input_digest,
                native_parent_id,
                protocol: row.protocol,
                status: row.status,
            },
            provenance,
        ))
    }
    pub(super) fn encode_prefix(
        &self,
        row: &old_prefix::Model,
        provenance: &Provenance,
    ) -> Result<prefix_entity::Model> {
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory owner mismatch"
        );
        let meta = self.scope.metadata(provenance)?;
        let logical_identity_json = Some(identity(&[
            ("episode_id", Some(row.episode_id.clone())),
            ("full_input_digest", Some(row.full_input_digest.clone())),
            ("ambiguous", Some(row.ambiguous.to_string())),
        ])?);
        Ok(prefix_entity::Model {
            owner_user_id: row.owner_user_id.clone(),
            full_input_digest: row.full_input_digest.clone(),
            episode_id: self.key(KeyDomain::TrajectoryEpisode, &row.episode_id)?,
            ambiguous: row.ambiguous,
            installation_id: meta.installation,
            tenant_id: meta.tenant,
            creator_principal_id: meta.creator,
            logical_identity_json,
        })
    }
    pub(super) fn decode_prefix(
        &self,
        row: prefix_entity::Model,
    ) -> Result<(old_prefix::Model, Provenance)> {
        let provenance = self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory compatibility owner mismatch"
        );
        let identity = decode_identity(
            row.logical_identity_json.as_deref(),
            &["episode_id", "full_input_digest", "ambiguous"],
        )?;
        let episode_id = logical(&identity, "episode_id")?;
        ensure!(
            row.episode_id == self.key(KeyDomain::TrajectoryEpisode, &episode_id)?,
            "trajectory episode_id projection mismatch"
        );
        ensure!(
            logical(&identity, "full_input_digest")? == row.full_input_digest,
            "trajectory full_input_digest projection mismatch"
        );
        ensure!(
            logical(&identity, "ambiguous")? == row.ambiguous.to_string(),
            "trajectory ambiguous projection mismatch"
        );
        Ok((
            old_prefix::Model {
                owner_user_id: row.owner_user_id,
                full_input_digest: row.full_input_digest,
                episode_id,
                ambiguous: row.ambiguous,
            },
            provenance,
        ))
    }
    pub(super) fn encode_outbox(
        &self,
        row: &old_outbox::Model,
        provenance: &Provenance,
    ) -> Result<outbox_entity::Model> {
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory owner mismatch"
        );
        let meta = self.scope.metadata(provenance)?;
        let logical_identity_json = Some(identity(&[
            ("outbox_id", Some(row.outbox_id.clone())),
            ("topic", Some(row.topic.clone())),
            ("payload_digest", Some(row.payload_digest.clone())),
            ("attempts", Some(row.attempts.to_string())),
            ("created_at", Some(row.created_at.clone())),
            ("delivered_at", row.delivered_at.clone()),
        ])?);
        Ok(outbox_entity::Model {
            outbox_id: self.key(KeyDomain::TrajectoryOutbox, &row.outbox_id)?,
            owner_user_id: row.owner_user_id.clone(),
            topic: row.topic.clone(),
            payload_json: row.payload_json.clone(),
            payload_digest: row.payload_digest.clone(),
            attempts: row.attempts,
            created_at: row.created_at.clone(),
            delivered_at: row.delivered_at.clone(),
            installation_id: meta.installation,
            tenant_id: meta.tenant,
            creator_principal_id: meta.creator,
            logical_identity_json,
        })
    }
    pub(super) fn decode_outbox(
        &self,
        row: outbox_entity::Model,
    ) -> Result<(old_outbox::Model, Provenance)> {
        let provenance = self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            row.owner_user_id == self.owner()?,
            "trajectory compatibility owner mismatch"
        );
        let identity = decode_identity(
            row.logical_identity_json.as_deref(),
            &[
                "outbox_id",
                "topic",
                "payload_digest",
                "attempts",
                "created_at",
                "delivered_at",
            ],
        )?;
        let outbox_id = logical(&identity, "outbox_id")?;
        ensure!(
            row.outbox_id == self.key(KeyDomain::TrajectoryOutbox, &outbox_id)?,
            "trajectory outbox_id projection mismatch"
        );
        ensure!(
            logical(&identity, "topic")? == row.topic,
            "trajectory topic projection mismatch"
        );
        ensure!(
            logical(&identity, "payload_digest")? == row.payload_digest,
            "trajectory payload_digest projection mismatch"
        );
        ensure!(
            logical(&identity, "attempts")? == row.attempts.to_string(),
            "trajectory attempts projection mismatch"
        );
        ensure!(
            logical(&identity, "created_at")? == row.created_at,
            "trajectory created_at projection mismatch"
        );
        ensure!(
            optional(&identity, "delivered_at")? == row.delivered_at,
            "trajectory delivered_at projection mismatch"
        );
        Ok((
            old_outbox::Model {
                outbox_id,
                owner_user_id: row.owner_user_id,
                topic: row.topic,
                payload_json: row.payload_json,
                payload_digest: row.payload_digest,
                attempts: row.attempts,
                created_at: row.created_at,
                delivered_at: row.delivered_at,
            },
            provenance,
        ))
    }
}

impl OwnedStore {
    pub(super) fn encode_event(
        &self,
        event: &crate::trajectory::types::TrajectoryEvent,
    ) -> Result<event_entity::Model> {
        super::super::validate_event(event)?;
        ensure!(
            event.owner_user_id == self.owner()?,
            "trajectory event owner mismatch"
        );
        io::json_size(event, 60 * 1024)?;
        let meta = self.scope.metadata(&self.provenance)?;
        Ok(event_entity::Model {
            event_id: self.key(KeyDomain::TrajectoryEvent, &event.event_id)?,
            episode_id: self.key(KeyDomain::TrajectoryEpisode, &event.episode_id)?,
            request_id: event
                .request_id
                .as_deref()
                .map(|v| self.key(KeyDomain::TrajectoryRequest, v))
                .transpose()?,
            owner_user_id: self.owner()?,
            sequence: event.sequence.try_into()?,
            kind: super::super::event_kind_name(event.kind).into(),
            event_json: serde_json::to_string(event)?,
            content_digest: event.content_digest.clone(),
            captured_at: event.captured_at.clone(),
            installation_id: meta.installation,
            tenant_id: meta.tenant,
            creator_principal_id: meta.creator,
        })
    }
    pub(super) fn decode_event(
        &self,
        row: event_entity::Model,
    ) -> Result<crate::trajectory::types::TrajectoryEvent> {
        self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            row.event_json.len() <= 60 * 1024,
            "trajectory event exceeds byte bound"
        );
        let event: crate::trajectory::types::TrajectoryEvent =
            serde_json::from_str(&row.event_json)?;
        let expected = self.encode_event(&event)?;
        ensure!(
            row.event_id == expected.event_id
                && row.episode_id == expected.episode_id
                && row.request_id == expected.request_id
                && row.owner_user_id == expected.owner_user_id
                && row.sequence == expected.sequence
                && row.kind == expected.kind
                && row.content_digest == expected.content_digest
                && row.captured_at == expected.captured_at,
            "trajectory event projection mismatch"
        );
        Ok(event)
    }
}
impl io::Columns for episode_entity::Model {
    const NAMES: &'static [&'static str] = &[
        "episode_id",
        "owner_user_id",
        "correlation_digest",
        "correlation_key_id",
        "correlation_source",
        "history_completeness",
        "next_sequence",
        "first_captured_at",
        "last_captured_at",
        "closed_at",
        "latest_request_id",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
        "logical_identity_json",
    ];
    const CELLS: &'static [(&'static str, usize)] = &[
        ("episode_id", 1024),
        ("owner_user_id", 1024),
        ("correlation_digest", 1024),
        ("correlation_key_id", 1024),
        ("correlation_source", 1024),
        ("history_completeness", 1024),
        ("first_captured_at", 1024),
        ("last_captured_at", 1024),
        ("closed_at", 1024),
        ("latest_request_id", 1024),
        ("installation_id", 1024),
        ("tenant_id", 1024),
        ("creator_principal_id", 1024),
        ("logical_identity_json", 8192),
    ];
}
impl io::Columns for event_entity::Model {
    const NAMES: &'static [&'static str] = &[
        "event_id",
        "owner_user_id",
        "episode_id",
        "request_id",
        "sequence",
        "kind",
        "event_json",
        "content_digest",
        "captured_at",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
    ];
    const CELLS: &'static [(&'static str, usize)] = &[
        ("event_id", 1024),
        ("owner_user_id", 1024),
        ("episode_id", 1024),
        ("request_id", 1024),
        ("kind", 1024),
        ("event_json", 61440),
        ("content_digest", 1024),
        ("captured_at", 1024),
        ("installation_id", 1024),
        ("tenant_id", 1024),
        ("creator_principal_id", 1024),
    ];
}
impl io::Columns for request_entity::Model {
    const NAMES: &'static [&'static str] = &[
        "request_id",
        "owner_user_id",
        "episode_id",
        "start_event_id",
        "settlement_event_id",
        "settlement_outbox_id",
        "full_input_digest",
        "native_parent_id",
        "protocol",
        "status",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
        "logical_identity_json",
    ];
    const CELLS: &'static [(&'static str, usize)] = &[
        ("request_id", 1024),
        ("owner_user_id", 1024),
        ("episode_id", 1024),
        ("start_event_id", 1024),
        ("settlement_event_id", 1024),
        ("settlement_outbox_id", 1024),
        ("full_input_digest", 1024),
        ("native_parent_id", 1024),
        ("protocol", 1024),
        ("status", 1024),
        ("installation_id", 1024),
        ("tenant_id", 1024),
        ("creator_principal_id", 1024),
        ("logical_identity_json", 8192),
    ];
}
impl io::Columns for prefix_entity::Model {
    const NAMES: &'static [&'static str] = &[
        "owner_user_id",
        "full_input_digest",
        "episode_id",
        "ambiguous",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
        "logical_identity_json",
    ];
    const CELLS: &'static [(&'static str, usize)] = &[
        ("owner_user_id", 1024),
        ("full_input_digest", 1024),
        ("episode_id", 1024),
        ("installation_id", 1024),
        ("tenant_id", 1024),
        ("creator_principal_id", 1024),
        ("logical_identity_json", 8192),
    ];
}
impl io::Columns for outbox_entity::Model {
    const NAMES: &'static [&'static str] = &[
        "outbox_id",
        "owner_user_id",
        "topic",
        "payload_json",
        "payload_digest",
        "attempts",
        "created_at",
        "delivered_at",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
        "logical_identity_json",
    ];
    const CELLS: &'static [(&'static str, usize)] = &[
        ("outbox_id", 1024),
        ("owner_user_id", 1024),
        ("topic", 1024),
        ("payload_json", 61440),
        ("payload_digest", 1024),
        ("created_at", 1024),
        ("delivered_at", 1024),
        ("installation_id", 1024),
        ("tenant_id", 1024),
        ("creator_principal_id", 1024),
        ("logical_identity_json", 8192),
    ];
}

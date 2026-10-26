//! The harness supplies immutable references, not a transcript copy to Core.
use super::{
    Append, Association, Descriptor, MAX_CAPTURE_BYTES, MAX_EVENTS, entities::events, io,
    protocol::State,
};
use crate::{
    acp_trajectory::checkpoint::types::EventReference,
    storage::{capture::CaptureKind, operation_budget::OperationBudget},
};
use anyhow::{Context, Result, ensure};
use sea_orm::{DatabaseTransaction, QueryOrder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prefix {
    pub(crate) descriptor: Descriptor,
    pub(crate) connection: String,
    pub(crate) session_key: String,
    pub(crate) native: String,
    pub(crate) watermark: i64,
    pub(crate) references: Vec<EventReference>,
    pub(crate) setup: Vec<EventReference>,
    pub(crate) boundary_at: String,
}
impl Prefix {
    pub(crate) fn digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(
            io::json(self, 256 * 1024)?.as_bytes(),
        )))
    }
}

impl Association {
    /// Read a finite, stopped prefix of a session created on this exact
    /// connection. No owner-string scan, cost lookup, judge or active experiment.
    pub(crate) async fn stopped_prefix_in(
        &self,
        tx: &DatabaseTransaction,
        native: &str,
        budget: &OperationBudget<'_>,
    ) -> Result<Option<Prefix>> {
        budget.check()?;
        if self.descriptor().mode == bitrouter_ai::tenancy::configuration::LearningMode::Off {
            return Ok(None);
        }
        let (connection, current) = self.connection_in(tx, budget).await?;
        let session = self.session_in(tx, native, budget).await?;
        ensure!(
            current.sessions.get(native) == Some(&session.head),
            "capture session metadata mismatch"
        );
        let rows = io::rows(
            tx,
            self.events().order_by_asc(events::Column::Sequence),
            MAX_EVENTS,
            MAX_CAPTURE_BYTES + 4096 * MAX_EVENTS,
            budget,
        )
        .await?;
        ensure!(
            i64::try_from(rows.len())? == connection.head,
            "capture connection prefix has a gap"
        );
        let mut state = State::initial(self);
        let mut references = Vec::new();
        let mut setup = Vec::new();
        let mut new_requests = BTreeMap::new();
        let mut stopped = false;
        let mut boundary_at = String::new();
        for row in rows {
            budget.check()?;
            self.validate(
                row.installation_id.as_deref(),
                row.tenant_id.as_deref(),
                row.creator_principal_id.as_deref(),
            )?;
            ensure!(!row.replay, "fresh capture cannot contain imported replay");
            let event = serde_json::from_str(&row.event_json)?;
            let append = Append::new(row.sequence, row.captured_at.clone(), event)?;
            ensure!(
                append.bytes == row.event_json,
                "capture event encoding changed"
            );
            let (next, observed, created) = state.next(&append)?;
            let expected_key = observed
                .as_deref()
                .map(|id| self.session_key(id))
                .transpose()?;
            let expected_head = observed
                .as_ref()
                .and_then(|id| next.sessions.get(id).copied());
            ensure!(
                row.session_key == expected_key && row.session_sequence == expected_head,
                "capture event session closure mismatch"
            );
            let reference = EventReference {
                connection_id: row.connection_id,
                sequence: row.sequence,
                session_sequence: row.session_sequence,
                digest: hex::encode(Sha256::digest(
                    io::json(
                        &(
                            &row.event_json,
                            &row.captured_at,
                            &row.session_key,
                            row.session_sequence,
                        ),
                        MAX_CAPTURE_BYTES,
                    )?
                    .as_bytes(),
                )),
            };
            if append.event.kind == CaptureKind::Request && append.event.method == "session/new" {
                let call = append
                    .event
                    .call_id
                    .context("new session request call absent")?;
                new_requests.insert(call, reference.clone());
            }
            if append.event.kind == CaptureKind::Response && append.event.method == "session/new" {
                let call = append
                    .event
                    .call_id
                    .context("new session reply call absent")?;
                let request = new_requests
                    .remove(&call)
                    .context("new session setup absent")?;
                if created && observed.as_deref() == Some(native) {
                    setup.push(request);
                }
            }
            if observed.as_deref() == Some(native) {
                boundary_at = row.captured_at;
                references.push(reference);
                stopped = append.event.method == "session/prompt"
                    && append.event.kind == CaptureKind::Response
                    && append
                        .event
                        .payload
                        .get("result")
                        .and_then(|result| result.get("stopReason"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|reason| !reason.is_empty() && reason.len() <= 64);
            }
            state = next;
        }
        ensure!(
            state == current,
            "capture durable correlation closure changed"
        );
        ensure!(
            references.len() == usize::try_from(session.head)? && setup.len() == 1,
            "capture session prefix is incomplete"
        );
        if !stopped
            || state
                .calls
                .values()
                .any(|call| call.session.as_deref() == Some(native))
        {
            return Ok(None);
        }
        budget.check()?;
        Ok(Some(Prefix {
            descriptor: self.descriptor().clone(),
            connection: self.connection().to_owned(),
            session_key: session.session_key,
            native: native.to_owned(),
            watermark: session.head,
            references,
            setup,
            boundary_at,
        }))
    }
}

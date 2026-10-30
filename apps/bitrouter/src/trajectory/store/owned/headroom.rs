//! Required terminal bytes are reserved in the start before any provider work.
//! Reservations are immutable event metadata, not synthetic settlement evidence.
use super::{MAX_EVENTS, io};
use crate::{
    storage::operation_budget::Error,
    trajectory::{evaluation::headroom::sizing_payload, types::*},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

const CELL_BYTES: usize = 60 * 1024;
const EVIDENCE: &str = "owned.terminal_evidence_bytes";
const OBSERVE: &str = "owned.terminal_observe";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reservation {
    evidence_bytes: usize,
    observe: bool,
}
impl Reservation {
    pub(crate) fn new(evidence_bytes: usize, observe: bool) -> Result<Self> {
        ensure!(
            evidence_bytes > 0 && evidence_bytes <= CELL_BYTES,
            Error::Capacity
        );
        Ok(Self {
            evidence_bytes,
            observe,
        })
    }
    pub(crate) fn observes(self) -> bool {
        self.observe
    }
    pub(super) fn retain(self, start: &mut TrajectoryEvent) -> Result<()> {
        start
            .evidence
            .structural
            .insert(EVIDENCE.into(), u64::try_from(self.evidence_bytes)?);
        start
            .evidence
            .structural
            .insert(OBSERVE.into(), u64::from(self.observe));
        start.content_digest = start.semantic_digest()?;
        Ok(())
    }
    pub(super) fn read(start: &TrajectoryEvent) -> Result<Self> {
        let evidence = *start
            .evidence
            .structural
            .get(EVIDENCE)
            .context("missing terminal byte reservation")?;
        let observe = *start
            .evidence
            .structural
            .get(OBSERVE)
            .context("missing terminal observation reservation")?;
        ensure!(observe <= 1, "invalid terminal observation reservation");
        let evidence_bytes = usize::try_from(evidence)?;
        ensure!(
            evidence_bytes > 0 && evidence_bytes <= CELL_BYTES,
            "invalid retained terminal byte reservation"
        );
        Ok(Self {
            evidence_bytes,
            observe: observe == 1,
        })
    }
    pub(super) fn validate(self, settlement: &Settlement) -> Result<()> {
        ensure!(
            self.observe == settlement.outbox.is_some(),
            "terminal changed reserved observation mode"
        );
        io::json_size(&settlement.event.evidence, self.evidence_bytes)?;
        io::json_size(&settlement.event, CELL_BYTES)?;
        if let Some(outbox) = &settlement.outbox {
            io::json_size(&outbox.payload, CELL_BYTES)?;
        }
        Ok(())
    }
    fn terminal_bytes(self, start: &TrajectoryEvent) -> Result<usize> {
        // Identity lengths are preserved except the terminal event ID, which is
        // a framed SHA-256 stable ID. Reserve the largest permitted ID instead.
        let mut terminal = start.clone();
        terminal.kind = TrajectoryEventKind::RequestSettled;
        terminal.event_id = "0".repeat(512);
        terminal.sequence = u64::MAX;
        terminal.captured_at = "0".repeat(timestamp_bytes()?);
        terminal.evidence = TrajectoryEvidence {
            structural: BTreeMap::new(),
            categorical: BTreeMap::new(),
            digests: BTreeMap::new(),
            route_measurement: None,
            routing_evidence: Vec::new(),
        };
        let base = io::json_size(&terminal, CELL_BYTES)?;
        let bytes = base
            .checked_add(self.evidence_bytes)
            .context(Error::Capacity)?;
        ensure!(bytes <= CELL_BYTES, Error::Capacity);
        Ok(bytes)
    }
}

fn timestamp_bytes() -> Result<usize> {
    [DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC]
        .iter()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Nanos, true).len())
        .max()
        .context("timestamp sizing missing")
}

/// Check the complete candidate, including the optional guard, before start DML.
/// All pending starts reserve their eventual receipts regardless of terminal order.
pub(super) fn reserve(events: &[TrajectoryEvent]) -> Result<()> {
    let mut pending = BTreeMap::new();
    let mut observe = None;
    let mut history_bytes = 0usize;
    for event in events {
        history_bytes = history_bytes
            .checked_add(io::json_size(event, CELL_BYTES)?)
            .and_then(|n| n.checked_add(2048))
            .context(Error::Capacity)?;
        match event.kind {
            TrajectoryEventKind::RequestStarted => {
                let reservation = Reservation::read(event)?;
                ensure!(
                    observe.is_none_or(|v| v == reservation.observe),
                    "mixed observation modes in owned episode"
                );
                observe = Some(reservation.observe);
                let request = event
                    .request_id
                    .as_deref()
                    .context("start missing request")?;
                ensure!(
                    pending.insert(request, (event, reservation)).is_none(),
                    "duplicate pending start"
                );
            }
            TrajectoryEventKind::RequestSettled => {
                let request = event
                    .request_id
                    .as_deref()
                    .context("terminal missing request")?;
                ensure!(
                    pending.remove(request).is_some(),
                    "terminal missing reserved start"
                );
            }
            _ => {}
        }
    }
    ensure!(
        !pending.is_empty(),
        "admission candidate has no pending start"
    );
    ensure!(
        events
            .len()
            .checked_add(pending.len())
            .context(Error::Capacity)?
            <= MAX_EVENTS,
        Error::Capacity
    );
    let mut future_evidence = 0usize;
    for (start, reservation) in pending.values() {
        history_bytes = history_bytes
            .checked_add(reservation.terminal_bytes(start)?)
            .and_then(|n| n.checked_add(2048))
            .context(Error::Capacity)?;
        // At most two routing receipts, each with an ID <=512 bytes, also
        // appear in result.evidence_refs. JSON escaping costs <=6 per byte.
        // https://www.rfc-editor.org/rfc/rfc8259#section-7
        future_evidence = future_evidence
            .checked_add(reservation.evidence_bytes)
            .and_then(|n| n.checked_add(2 * (512 * 6 + 3)))
            .context(Error::Capacity)?;
    }
    // Terminal reads history twice (request_in and settle_current_in). Reserve
    // another 2 MiB for point reads, escaped write models and the <=60 KiB Eval
    // subject/result/outbox closure. Row counts remain below 2*512 + 32 <4096.
    ensure!(
        history_bytes
            .checked_mul(2)
            .and_then(|n| n.checked_add(2 * 1024 * 1024))
            .context(Error::Capacity)?
            <= io::MAX_BYTES,
        Error::Capacity
    );
    if observe == Some(true) {
        let fixed = io::json_size(&sizing_payload(events)?, CELL_BYTES)?;
        // The current prediction item's fields and all future receipts project
        // disjoint pieces of their terminal evidence. Converting u64 values to
        // string attributes adds <=2*64 quotes and two joining separators.
        let required = fixed
            .checked_add(future_evidence)
            .and_then(|n| n.checked_add(2 * 64 + 2))
            .context(Error::Capacity)?;
        ensure!(required <= CELL_BYTES, Error::Capacity);
    }
    Ok(())
}

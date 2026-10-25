//! Admission-only serialization bounds for the required operational outbox.
//! Sizing values never become facts: this module returns only a byte count.
//!
//! Keep this accounting aligned with trajectory/evaluation.rs. In particular,
//! every pending settlement contributes routing receipts, while only the
//! currently evaluated settlement contributes prediction attributes.

use crate::trajectory::{evaluation::headroom::sizing_payload, types::TrajectoryEvent};
use anyhow::{Context, Result, ensure};

use super::super::{MAX_GUARDED_SETTLEMENT_BYTES, MAX_OUTBOX_BYTES};

pub(super) fn reserve(events: &[TrajectoryEvent], unsettled: u64) -> Result<()> {
    // The complete outbox contains its evaluation as a JSON object, not an
    // escaped string. Its 1 MiB limit therefore also covers the current shared
    // evaluation sink's 1 MiB limit (storage/evaluation/shared.rs).
    ensure!(
        upper_bound(events, unsettled)? <= MAX_OUTBOX_BYTES,
        "shared trajectory lacks required evaluation outbox headroom"
    );
    Ok(())
}

pub(super) fn upper_bound(events: &[TrajectoryEvent], unsettled: u64) -> Result<usize> {
    ensure!(
        unsettled > 0,
        "shared output reserve requires unsettled work"
    );
    let payload = sizing_payload(events)?;
    let known_and_fixed = serde_json::to_vec(&payload)?.len();

    // A valid event contains <=2 routing receipts, each with an ID <=512
    // bytes. General JSON string escaping costs <=6 bytes per input byte,
    // plus quotes and the separator when appended to evidence_refs.
    // https://www.rfc-editor.org/rfc/rfc8259#section-7
    let receipt_references = 2usize * (512 * 6 + 2 + 1);
    let per_pending = MAX_GUARDED_SETTLEMENT_BYTES
        .checked_add(receipt_references)
        .context("shared output reserve overflow")?;
    let future = usize::try_from(unsettled)?
        .checked_mul(per_pending)
        .context("shared output reserve overflow")?;

    // Future receipts are copied once. The current prediction item projects
    // disjoint categorical/structural fields from that same event: its keys
    // lose the routing. prefix, strings retain their JSON encoding, and at
    // most 64 u64 values gain two quotes. Their source bytes plus receipt
    // bytes fit in the one full-event allowance; do not charge them twice.
    // The empty prediction item and reference are already in sizing_payload.
    // Two extra separators cover joining the copied map/array fragments.
    known_and_fixed
        .checked_add(future)
        .and_then(|bytes| bytes.checked_add(2 * 64 + 2))
        .context("shared output reserve overflow")
}

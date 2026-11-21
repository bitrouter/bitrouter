//! Service-owned admission policy over credential-free provider evidence.

use crate::config::model::ModelTokenLimits;

/// A provider that cannot accept a request-level output cap may still fit
/// a reservation covering its entire configured model output ceiling.
/// Missing or zero metadata never establishes that bound.
pub(crate) fn output_reservation_covers_model(limits: &ModelTokenLimits, reservation: u32) -> bool {
    limits
        .max_output_tokens
        .is_some_and(|ceiling| ceiling > 0 && ceiling <= u64::from(reservation))
}

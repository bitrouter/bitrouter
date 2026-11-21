//! Persisted session identity for gateway evaluation reservations.

use serde::{Deserialize, Serialize};

/// Reservation provenance; this observation does not confer execution authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub owner: String,
    pub source: String,
    pub native_session_id: String,
}

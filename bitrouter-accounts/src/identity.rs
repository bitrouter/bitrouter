//! Authenticated caller identity.
//!
//! These types form the contract between the auth layer (provided by the
//! caller) and the account/session routes. The auth filter extracts an
//! [`Identity`] from the request; route handlers receive it and use the
//! embedded [`AccountId`] and [`Scope`] to authorize operations.

use std::fmt;

use uuid::Uuid;

/// Opaque account identifier.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
pub struct AccountId(pub Uuid);

impl AccountId {
    /// Generate a new random account ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What level of access an authenticated caller has.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Scope {
    /// Can call LLM API endpoints and read own sessions.
    Api = 0,
    /// Can manage API keys, account settings, etc.
    Admin = 1,
}

/// The result of authentication.
///
/// Route builders receive this from the caller-supplied auth filter. The
/// accounts crate never needs to know *how* authentication was performed.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The authenticated account.
    pub account_id: AccountId,
    /// What this caller is permitted to do.
    pub scope: Scope,
}

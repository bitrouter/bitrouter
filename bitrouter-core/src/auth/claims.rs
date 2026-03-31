//! JWT claims types for the BitRouter authentication protocol.
//!
//! These types define the payload of a BitRouter JWT. The operator's OWS
//! wallet signs all tokens — both admin JWTs and agent access JWTs. The
//! `iss` claim carries the operator's CAIP-10 identity (e.g.
//! `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:<base58_pubkey>`), and the
//! server verifies that `iss` matches the configured operator wallet.
//!
//! Field keys are kept to ≤3 characters to minimize encoded JWT size.
//! The chain is derived from `iss` (CAIP-10 encodes CAIP-2), so no
//! separate chain field is needed.

use serde::{Deserialize, Serialize};

/// JWT claims for BitRouter authentication tokens.
///
/// Tokens are signed by the operator's OWS wallet. The CAIP-10 address
/// in `iss` identifies the operator; the server verifies the signature
/// against the operator's known public key from config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitrouterClaims {
    /// CAIP-10 account identifier of the operator wallet.
    ///
    /// The chain (CAIP-2) is derived from this field, so no separate
    /// chain claim is needed.
    ///
    /// Examples:
    /// - `"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb..."`
    /// - `"eip155:8453:0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B"`
    pub iss: String,

    /// Issued-at UNIX timestamp (seconds since epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<u64>,

    /// Expiration UNIX timestamp. Required for admin-scope tokens.
    /// Long-lived API tokens may omit this, relying on budget exhaustion
    /// or key rotation for invalidation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,

    /// Authorization scope: `"adm"` (admin) or `"api"` (agent access).
    /// Defaults to `Api` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scp: Option<TokenScope>,

    /// Optional allowlist of model name patterns this token may access.
    /// When `None`, all models configured on the server are accessible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdl: Option<Vec<String>>,

    /// Budget limit in micro USD (1 USD = 1,000,000 μUSD).
    /// Matches on-chain stablecoin precision (6 decimals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bgt: Option<u64>,

    /// Whether the budget applies per-session or per-account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bsc: Option<BudgetScope>,

    /// OWS agent key for payment authorization.
    /// When present, the server validates the key against the OWS vault
    /// and uses its associated policies for spending enforcement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl BitrouterClaims {
    /// Resolved scope — defaults to [`TokenScope::Api`] when `scp` is absent.
    pub fn scope(&self) -> TokenScope {
        self.scp.unwrap_or(TokenScope::Api)
    }
}

/// Token authorization scope.
///
/// - `Admin`: Management operations (route admin, key management).
/// - `Api`: LLM inference and tool access endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenScope {
    /// Admin management scope.
    #[serde(rename = "adm")]
    Admin,
    /// API / agent access scope (default when `scp` is absent).
    #[serde(rename = "api")]
    Api,
}

/// Budget scope — determines what the budget limit applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetScope {
    /// Budget applies independently to each chat session.
    #[serde(rename = "ses")]
    Session,
    /// Budget applies to the entire account across all sessions.
    #[serde(rename = "act")]
    Account,
}

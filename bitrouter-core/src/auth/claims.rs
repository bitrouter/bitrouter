//! JWT claims types for the BitRouter authentication protocol.
//!
//! These types define the payload of a BitRouter JWT. The `iss` claim carries
//! the signer's CAIP-10 account identifier (e.g.
//! `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:<base58_pubkey>`), which the
//! server uses for both signature verification and account resolution.

use serde::{Deserialize, Serialize};

/// JWT claims for BitRouter authentication tokens.
///
/// Tokens are self-signed by the account holder's web3 wallet key. The
/// CAIP-10 address in `iss` is the sole identity — the token has zero
/// knowledge of the underlying account ID or server-side state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitrouterClaims {
    /// CAIP-10 account identifier of the signer.
    ///
    /// Examples:
    /// - `"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb..."`
    /// - `"eip155:8453:0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B"`
    pub iss: String,

    /// CAIP-2 chain identifier indicating which chain was used to sign.
    ///
    /// Examples: `"solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"`, `"eip155:8453"`.
    pub chain: String,

    /// Issued-at UNIX timestamp (seconds since epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iat: Option<u64>,

    /// Expiration UNIX timestamp. Required for admin-scope tokens.
    /// Long-lived API tokens may omit this, relying on budget exhaustion
    /// or key rotation for invalidation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<u64>,

    /// Authorization scope granted by this token.
    pub scope: TokenScope,

    /// Optional allowlist of model name patterns this token may access.
    /// When `None`, all models configured on the server are accessible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,

    /// Optional allowlist of tool name patterns this token may access.
    /// Tool names are `{server}/{tool}`, so patterns like `"github/*"` work.
    /// When `None`, all tools are accessible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,

    /// Budget limit in micro USD (1 USD = 1,000,000 μUSD).
    /// Matches on-chain stablecoin precision (6 decimals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<u64>,

    /// Whether the budget applies per-session or per-account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_scope: Option<BudgetScope>,

    /// The range over which the budget is measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_range: Option<BudgetRange>,
}

/// Token authorization scope.
///
/// - `Admin`: Account management operations (rotate key, manage sessions).
///   Scoped to the caller's own account — NOT global server admin.
/// - `Api`: LLM inference endpoints only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    Admin,
    Api,
}

/// Budget scope — determines what the budget limit applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetScope {
    /// Budget applies independently to each chat session.
    Session,
    /// Budget applies to the entire account across all sessions.
    Account,
}

/// Budget range — the window over which the budget is measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BudgetRange {
    /// Budget covers the next N conversation rounds.
    Rounds { count: u32 },
    /// Budget covers a time period (in seconds).
    Duration { seconds: u64 },
}

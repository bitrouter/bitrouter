//! Issuer-kind dispatch for BitRouter JWTs.
//!
//! Every BitRouter JWT has an `iss` claim that identifies the signer.
//! Two structurally disjoint shapes are supported today:
//!
//! - **[`IssuerKind::WalletCaip10`]** — a CAIP-10 account identifier
//!   (`solana:<ref>:<addr>` or `eip155:<chainid>:<addr>`). Self-verifying:
//!   the public key is derivable from the `iss` string itself, which is
//!   then checked against the signature.
//! - **[`IssuerKind::HostThumbprint`]** — an RFC 7638 JWK thumbprint
//!   (43-char base64url-unpadded SHA-256 digest). The public key lives in
//!   the JWT header (`jwk`), and the token is accepted iff
//!   `sha256_thumbprint(jwk) == iss` and the signature verifies against
//!   that `jwk`.
//!
//! The two shapes are structurally disjoint: CAIP-10 contains colons in
//! a known namespace; a thumbprint is a fixed-length base64url string
//! (alphanumeric + `-_`, no colon). [`IssuerKind::parse`] tries CAIP-10
//! first, then falls through to thumbprint.
//!
//! A third variant (`MppSession`) is deliberately left unimplemented;
//! it will be added when MPP server-side support lands in a later phase.

use crate::auth::JwtError;
use crate::auth::chain::Caip10;

/// RFC 7638 SHA-256 JWK thumbprint length in base64url-no-pad characters.
/// SHA-256 produces 32 bytes; base64url-no-pad encodes 32 bytes as 43 chars.
const THUMBPRINT_B64URL_LEN: usize = 43;

/// Classification of a JWT's `iss` claim into one of the supported
/// issuer shapes.
///
/// Adding a new variant here is the only code change needed to extend the
/// auth layer to a new identity type, and it is deliberately the dispatch
/// point referenced from [`crate::auth::token::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuerKind {
    /// Wallet-derived CAIP-10 issuer (permissionless self-verifying path).
    WalletCaip10 { caip10: Caip10 },

    /// Host-custodied JWK thumbprint issuer (permissioned path — the
    /// custodian signs on behalf of an authenticated end user).
    HostThumbprint { thumbprint: String },
    // Reserved for a future MPP-session issuer variant:
    // MppSession { session_id: String, escrow_iss: Box<IssuerKind> },
}

impl IssuerKind {
    /// Classify an `iss` claim string into an [`IssuerKind`].
    ///
    /// Tries CAIP-10 first (structurally distinct — contains colons in a
    /// known namespace). Falls through to a JWK thumbprint shape check.
    /// Returns [`JwtError::InvalidIssuer`] for strings that match neither
    /// shape.
    ///
    /// This function performs format validation only — it does NOT check
    /// the header algorithm or verify signatures. The caller (e.g.
    /// [`crate::auth::token::verify`]) is responsible for binding the
    /// issuer kind to the correct algorithm and rejecting cross-kind
    /// algorithm forgeries.
    pub fn parse(iss: &str) -> Result<Self, JwtError> {
        if let Ok(caip10) = Caip10::parse(iss) {
            return Ok(Self::WalletCaip10 { caip10 });
        }

        if is_b64url_thumbprint(iss) {
            return Ok(Self::HostThumbprint {
                thumbprint: iss.to_string(),
            });
        }

        Err(JwtError::InvalidIssuer(iss.to_string()))
    }
}

/// Returns `true` if `s` matches the shape of a base64url-no-pad
/// SHA-256 thumbprint (43 characters from the base64url alphabet).
fn is_b64url_thumbprint(s: &str) -> bool {
    s.len() == THUMBPRINT_B64URL_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_THUMBPRINT: &str = "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs";
    const VALID_SOLANA_ISS: &str =
        "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy";
    const VALID_EVM_ISS: &str = "eip155:8453:0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";

    #[test]
    fn parses_solana_caip10() {
        let kind = IssuerKind::parse(VALID_SOLANA_ISS).expect("parse");
        assert!(matches!(kind, IssuerKind::WalletCaip10 { .. }));
    }

    #[test]
    fn parses_evm_caip10() {
        let kind = IssuerKind::parse(VALID_EVM_ISS).expect("parse");
        assert!(matches!(kind, IssuerKind::WalletCaip10 { .. }));
    }

    #[test]
    fn parses_jwk_thumbprint() {
        let kind = IssuerKind::parse(VALID_THUMBPRINT).expect("parse");
        match kind {
            IssuerKind::HostThumbprint { thumbprint } => {
                assert_eq!(thumbprint, VALID_THUMBPRINT);
            }
            _ => panic!("expected HostThumbprint"),
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(IssuerKind::parse("").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(IssuerKind::parse("not a valid iss").is_err());
    }

    #[test]
    fn rejects_thumbprint_wrong_length() {
        // 42 chars — off by one.
        assert!(IssuerKind::parse("NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9X").is_err());
        // 44 chars.
        assert!(IssuerKind::parse("NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9XsA").is_err());
    }

    #[test]
    fn rejects_thumbprint_bad_chars() {
        // `+` and `/` are standard base64 but invalid in base64url.
        let bad = "NzbLsXh8uDCcd+6MNwXF4W/7noWXFZAfHkxZsRGC9Xs";
        assert_eq!(bad.len(), THUMBPRINT_B64URL_LEN);
        assert!(IssuerKind::parse(bad).is_err());
    }

    #[test]
    fn caip10_takes_precedence_over_thumbprint_shape() {
        // CAIP-10 contains colons which are not in the base64url alphabet,
        // so the two shapes are structurally disjoint — this test merely
        // asserts the precedence order explicitly.
        let kind = IssuerKind::parse(VALID_SOLANA_ISS).expect("parse");
        assert!(matches!(kind, IssuerKind::WalletCaip10 { .. }));
    }
}

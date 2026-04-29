//! JWT authentication filters for the bitrouter gateway.
//!
//! Implements operator-signed JWT authentication:
//!
//! - **JWT path**: The operator's OWS wallet signs all JWTs. The `iss`
//!   claim carries the operator's CAIP-10 identity. The server verifies
//!   the signature and checks that `iss` matches the configured operator
//!   wallet — this is the single trust root.
//!
//! Credentials are extracted from the protocol-appropriate header:
//!
//! | Protocol   | Header                                          |
//! |------------|-------------------------------------------------|
//! | OpenAI     | `Authorization: Bearer <token>`                 |
//! | Anthropic  | `x-api-key: <token>` or `Authorization: Bearer` |
//! | Management | `Authorization: Bearer <token>`                 |
//!
//! Authentication is always enforced — the bitrouter binary requires a
//! database connection at startup.

use std::sync::Arc;

use sea_orm::DatabaseConnection;
use warp::Filter;

use bitrouter_accounts::identity::{AccountId, Identity, Scope};
use bitrouter_accounts::service::AccountService;
use bitrouter_accounts::service::VirtualKeyService;
use bitrouter_core::auth::chain::Caip10;
use bitrouter_core::auth::claims::{BitrouterClaims, TokenScope};
use bitrouter_core::auth::revocation::KeyRevocationSet;
use bitrouter_core::auth::token as jwt_token;

/// Shared auth state passed into filters.
#[derive(Clone)]
pub struct JwtAuthContext {
    /// Database connection for account lookups (auto-creation).
    db: DatabaseConnection,
    /// The operator's CAIP-10 identity resolved from wallet config at startup.
    /// When set, JWTs must have `iss` matching this identity.
    operator_caip10: Option<String>,
    /// Optional revocation set for per-key revocation. When present, JWTs
    /// whose `id` claim appears in this set are rejected.
    revocation_set: Option<Arc<dyn KeyRevocationSet>>,
}

impl JwtAuthContext {
    pub fn new(db: DatabaseConnection, operator_caip10: Option<String>) -> Self {
        Self {
            db,
            operator_caip10,
            revocation_set: None,
        }
    }

    /// Attach a revocation set for per-key JWT revocation.
    pub fn with_revocation_set(mut self, set: Arc<dyn KeyRevocationSet>) -> Self {
        self.revocation_set = Some(set);
        self
    }

    pub(crate) fn verify_jwt_claims(
        &self,
        credential: &str,
    ) -> Result<BitrouterClaims, Unauthorized> {
        let claims =
            jwt_token::verify(credential).map_err(|_| Unauthorized("invalid JWT signature"))?;

        jwt_token::check_expiration(&claims).map_err(|_| Unauthorized("JWT expired"))?;

        if let Some(ref expected) = self.operator_caip10
            && claims.iss != *expected
        {
            return Err(Unauthorized(
                "JWT issuer does not match configured operator wallet",
            ));
        }

        Ok(claims)
    }
}

// ── credential extraction ─────────────────────────────────────

/// Extract a bearer token from `Authorization: Bearer <token>`.
///
/// Handles comma-separated schemes so that
/// `"Bearer <jwt>, Payment <cred>"` correctly returns `"<jwt>"`.
fn extract_bearer(header: &str) -> Option<&str> {
    for segment in header.split(',') {
        let trimmed = segment.trim();
        if let Some(token) = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
        {
            return Some(token.trim());
        }
    }
    None
}

/// Warp filter: extract credential from `Authorization: Bearer` header.
pub fn bearer_credential() -> impl Filter<Extract = (String,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization").and_then(
        |header: Option<String>| async move {
            match header.and_then(|h| extract_bearer(&h).map(str::to_owned)) {
                Some(key) => Ok(key),
                None => Err(warp::reject::custom(Unauthorized("missing bearer token"))),
            }
        },
    )
}

/// Warp filter: extract credential from either `Authorization: Bearer` **or**
/// `x-api-key` (Anthropic-style). Bearer takes precedence.
pub fn any_credential() -> impl Filter<Extract = (String,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::header::optional::<String>("x-api-key"))
        .and_then(
            |auth_header: Option<String>, x_api_key: Option<String>| async move {
                if let Some(key) = auth_header.and_then(|h| extract_bearer(&h).map(str::to_owned)) {
                    return Ok(key);
                }
                if let Some(key) = x_api_key.filter(|k| !k.is_empty()) {
                    return Ok(key);
                }
                Err(warp::reject::custom(Unauthorized(
                    "missing authentication credentials",
                )))
            },
        )
}

// ── identity resolution ───────────────────────────────────────

/// Resolve a credential string to an [`Identity`].
///
/// Verifies the JWT signature, checks expiration, and resolves the account.
///
/// Security-critical ordering:
/// 1. Verify signature cryptographically.
/// 2. Check expiration.
/// 3. Look up / auto-create account in DB using CAIP-10 identity.
/// 4. Build Identity with account-relative scope.
async fn resolve_identity(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<Identity, warp::Rejection> {
    let jwt = resolve_credential_jwt(credential, ctx).await?;
    resolve_jwt_identity(&jwt, ctx).await
}

/// Resolve an inbound credential to the JWT that should be authenticated.
async fn resolve_credential_jwt(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<String, warp::Rejection> {
    if !bitrouter_accounts::service::is_virtual_key(credential) {
        return Ok(credential.to_owned());
    }

    let svc = VirtualKeyService::new(&ctx.db);
    svc.resolve(credential)
        .await
        .map_err(|_| warp::reject::custom(Unauthorized("virtual key lookup failed")))?
        .ok_or_else(|| warp::reject::custom(Unauthorized("invalid virtual key")))
}

/// Resolve a JWT credential to an [`Identity`].
async fn resolve_jwt_identity(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<Identity, warp::Rejection> {
    // 1-3. Verify signature, expiration, and configured operator issuer.
    let claims = ctx
        .verify_jwt_claims(credential)
        .map_err(warp::reject::custom)?;

    // 3b. Check per-key revocation (if a revocation set is configured).
    if let Some(ref key_id) = claims.id
        && let Some(ref revocation_set) = ctx.revocation_set
        && revocation_set.is_revoked(key_id).await
    {
        return Err(warp::reject::custom(Unauthorized(
            "API key has been revoked",
        )));
    }

    // 4. Derive chain from iss (CAIP-10 → CAIP-2).
    let chain = Caip10::parse(&claims.iss).ok().map(|c| c.chain.caip2());

    // 5. Resolve account from DB using CAIP-10 iss.
    let svc = AccountService::new(&ctx.db);
    let account = svc
        .find_or_create_by_pubkey(&claims.iss)
        .await
        .map_err(|_| warp::reject::custom(Unauthorized("account lookup failed")))?;

    let Some(account) = account else {
        return Err(warp::reject::custom(Unauthorized(
            "public key has been rotated — generate a new token with your current key",
        )));
    };

    // 6. Build identity with resolved scope and permissions.
    let scope = match claims.scope() {
        TokenScope::Admin => Scope::Admin,
        TokenScope::Api => Scope::Api,
    };

    Ok(Identity {
        account_id: AccountId(account.id),
        scope,
        key_id: claims.id,
        chain,
        models: claims.mdl,
        budget: claims.bgt,
        budget_scope: claims.bsc,
        issued_at: claims.iat,
        key: claims.key,
        policy_id: claims.pol,
    })
}

// ── composite auth filters ────────────────────────────────────

/// Build an auth filter for OpenAI-protocol routes (`Authorization: Bearer`).
pub fn openai_auth(
    ctx: Arc<JwtAuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    let ctx = ctx.clone();
    bearer_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<JwtAuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Build an auth filter for Anthropic-protocol routes.
///
/// Accepts credentials from either `x-api-key` (standard Anthropic) or
/// `Authorization: Bearer` (used by clients like Claude Code that set
/// `ANTHROPIC_AUTH_TOKEN`).
pub fn anthropic_auth(
    ctx: Arc<JwtAuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    let ctx = ctx.clone();
    any_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<JwtAuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Build an auth filter for management routes. Accepts both Bearer and x-api-key.
pub fn management_auth(
    ctx: Arc<JwtAuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    let ctx = ctx.clone();
    any_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<JwtAuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

// ── rejection types ───────────────────────────────────────────

#[derive(Debug)]
pub struct Unauthorized(pub &'static str);

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unauthorized: {}", self.0)
    }
}

impl warp::reject::Reject for Unauthorized {}

/// Convert an auth filter into a gate that rejects unauthorized requests
/// but does not add anything to the extract tuple.
pub(crate) fn auth_gate(
    auth: impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone,
) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    auth.map(|_| ()).untuple_one()
}

#[cfg(test)]
mod tests {
    use bitrouter_accounts::service::VirtualKeyService;
    use bitrouter_core::auth::chain::Chain;
    use bitrouter_core::auth::claims::{BitrouterClaims, TokenScope};
    use bitrouter_core::auth::keys::MasterKeypair;
    use bitrouter_core::auth::token;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    use super::*;

    async fn setup_test_db() -> Result<DatabaseConnection, Box<dyn std::error::Error>> {
        let db = Database::connect("sqlite::memory:").await?;

        struct TestMigrator;

        impl MigratorTrait for TestMigrator {
            fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
                bitrouter_accounts::migration::migrations()
            }
        }

        TestMigrator::up(&db, None).await?;
        Ok(db)
    }

    #[tokio::test]
    async fn virtual_key_resolves_to_stored_jwt_identity() -> Result<(), Box<dyn std::error::Error>>
    {
        let db = setup_test_db().await?;
        let keypair = MasterKeypair::generate();
        let caip10 = keypair.caip10(&Chain::solana_mainnet())?;
        let key_id = "virtual-key-test-id".to_owned();
        let claims = BitrouterClaims {
            iss: caip10.format(),
            iat: Some(1_700_000_000),
            exp: None,
            scp: Some(TokenScope::Api),
            mdl: Some(vec!["openai:gpt-4o".to_owned()]),
            bgt: Some(1_000_000),
            bsc: None,
            id: Some(key_id.clone()),
            key: None,
            pol: Some("default".to_owned()),
            jti: None,
            aud: None,
            sub: None,
            host: None,
        };
        let jwt = token::sign(&claims, &keypair)?;
        let virtual_key = VirtualKeyService::new(&db).create(&jwt).await?.key;
        let ctx = JwtAuthContext::new(db, None);

        let identity = resolve_identity(&virtual_key, &ctx)
            .await
            .map_err(|_| std::io::Error::other("virtual key auth failed"))?;

        assert_eq!(identity.key_id.as_deref(), Some(key_id.as_str()));
        assert_eq!(identity.models, Some(vec!["openai:gpt-4o".to_owned()]));
        assert_eq!(identity.budget, Some(1_000_000));
        assert_eq!(identity.policy_id.as_deref(), Some("default"));
        Ok(())
    }
}

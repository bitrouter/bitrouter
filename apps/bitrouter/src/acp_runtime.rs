//! Authenticated, ephemeral ACP controller runtime state.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// Prefix of credentials accepted only for authenticated ACP controllers.
pub const CONTROLLER_CREDENTIAL_PREFIX: &str = "brac_";

/// One short-lived controller credential returned over the owner-only daemon
/// control socket. Its `Debug` implementation never exposes the token.
#[derive(Clone)]
pub struct ControllerCredentialGrant {
    controller_instance_id: String,
    token: String,
    expires_at: DateTime<Utc>,
}

impl ControllerCredentialGrant {
    /// Plaintext bearer token. Callers must keep it inside harness endpoint
    /// configuration and must not log it.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Controller identity bound to this credential.
    pub fn controller_instance_id(&self) -> &str {
        &self.controller_instance_id
    }

    /// Expiry of this credential and every lease it owns.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

impl std::fmt::Debug for ControllerCredentialGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerCredentialGrant")
            .field("controller_instance_id", &self.controller_instance_id)
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Authenticated controller principal established from a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerPrincipal {
    controller_instance_id: String,
    expires_at: DateTime<Utc>,
}

impl ControllerPrincipal {
    /// Credential-bound controller identity.
    pub fn controller_instance_id(&self) -> &str {
        &self.controller_instance_id
    }

    /// Credential expiry.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

/// One effective session route lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteLease {
    lease_id: String,
    controller_instance_id: String,
    session_id: String,
    route: String,
}

impl RouteLease {
    /// Opaque runtime lease identity for diagnostics.
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    /// Owning controller.
    pub fn controller_instance_id(&self) -> &str {
        &self.controller_instance_id
    }

    /// ACP/native session key on which the lease was installed.
    pub fn matched_session_id(&self) -> &str {
        &self.session_id
    }

    /// BitRouter route selector installed by the manager.
    pub fn route(&self) -> &str {
        &self.route
    }
}

#[derive(Clone)]
struct CredentialEntry {
    controller_instance_id: String,
    expires_at: DateTime<Utc>,
}

#[derive(Default)]
struct RuntimeState {
    credentials: HashMap<String, CredentialEntry>,
    leases: HashMap<(String, String), RouteLease>,
}

/// Daemon-owned controller credentials and session route leases.
///
/// Everything is deliberately in memory: revocation, expiry, daemon restart,
/// or controller disconnect removes authority without touching harness-owned
/// session data.
#[derive(Default)]
pub struct AcpRuntime {
    state: RwLock<RuntimeState>,
}

impl AcpRuntime {
    /// Construct an empty runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a short-lived credential for one controller instance.
    pub fn issue_controller(
        &self,
        controller_instance_id: &str,
        ttl: Duration,
    ) -> Result<ControllerCredentialGrant, String> {
        let controller_instance_id = controller_instance_id.trim();
        if controller_instance_id.is_empty() {
            return Err("controller instance id must not be empty".to_string());
        }
        let ttl = chrono::Duration::from_std(ttl)
            .map_err(|error| format!("controller credential ttl is out of range: {error}"))?;
        let now = Utc::now();
        let expires_at = now + ttl;
        let mut state = self.write_state();
        cleanup_expired(&mut state, now);
        if state.credentials.values().any(|entry| {
            entry.controller_instance_id == controller_instance_id && entry.expires_at > now
        }) {
            return Err(format!(
                "controller instance '{controller_instance_id}' already has a live credential"
            ));
        }
        let token = format!(
            "{CONTROLLER_CREDENTIAL_PREFIX}{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        state.credentials.insert(
            credential_hash(&token),
            CredentialEntry {
                controller_instance_id: controller_instance_id.to_string(),
                expires_at,
            },
        );
        Ok(ControllerCredentialGrant {
            controller_instance_id: controller_instance_id.to_string(),
            token,
            expires_at,
        })
    }

    /// Authenticate a presented controller bearer. Expired credentials and
    /// their leases are removed before lookup.
    pub fn authenticate(&self, token: &str) -> Option<ControllerPrincipal> {
        if !token.starts_with(CONTROLLER_CREDENTIAL_PREFIX) {
            return None;
        }
        let mut state = self.write_state();
        cleanup_expired(&mut state, Utc::now());
        state
            .credentials
            .get(&credential_hash(token))
            .map(|entry| ControllerPrincipal {
                controller_instance_id: entry.controller_instance_id.clone(),
                expires_at: entry.expires_at,
            })
    }

    /// Revoke a controller credential and all of its route leases.
    pub fn revoke_controller(&self, controller_instance_id: &str) {
        let mut state = self.write_state();
        state
            .credentials
            .retain(|_, entry| entry.controller_instance_id != controller_instance_id);
        state
            .leases
            .retain(|(controller, _), _| controller != controller_instance_id);
    }

    /// Create or replace one controller/session route lease.
    pub fn set_route(
        &self,
        controller_instance_id: &str,
        session_id: &str,
        route: &str,
    ) -> Result<RouteLease, String> {
        let session_id = session_id.trim();
        let route = route.trim();
        if session_id.is_empty() || route.is_empty() {
            return Err("session id and route must not be empty".to_string());
        }
        let mut state = self.write_state();
        cleanup_expired(&mut state, Utc::now());
        if !controller_is_active(&state, controller_instance_id) {
            return Err("controller credential is not active".to_string());
        }
        let lease = RouteLease {
            lease_id: format!("brlease_{}", uuid::Uuid::new_v4().simple()),
            controller_instance_id: controller_instance_id.to_string(),
            session_id: session_id.to_string(),
            route: route.to_string(),
        };
        state.leases.insert(
            (controller_instance_id.to_string(), session_id.to_string()),
            lease.clone(),
        );
        Ok(lease)
    }

    /// Remove one route lease. Returns the removed lease when present.
    pub fn reset_route(
        &self,
        controller_instance_id: &str,
        session_id: &str,
    ) -> Option<RouteLease> {
        self.write_state()
            .leases
            .remove(&(controller_instance_id.to_string(), session_id.to_string()))
    }

    /// Resolve the first matching session candidate for an authenticated
    /// controller. Candidate order expresses exact-child before root fallback.
    pub fn resolve_route(
        &self,
        controller_instance_id: &str,
        session_candidates: &[&str],
    ) -> Option<RouteLease> {
        let mut state = self.write_state();
        cleanup_expired(&mut state, Utc::now());
        if !controller_is_active(&state, controller_instance_id) {
            return None;
        }
        session_candidates.iter().find_map(|session_id| {
            state
                .leases
                .get(&(
                    controller_instance_id.to_string(),
                    (*session_id).to_string(),
                ))
                .cloned()
        })
    }

    /// Read the exact lease for one controller/session pair.
    pub fn current_route(
        &self,
        controller_instance_id: &str,
        session_id: &str,
    ) -> Option<RouteLease> {
        self.resolve_route(controller_instance_id, &[session_id])
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, RuntimeState> {
        match self.state.write() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn credential_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn controller_is_active(state: &RuntimeState, controller_instance_id: &str) -> bool {
    state
        .credentials
        .values()
        .any(|entry| entry.controller_instance_id == controller_instance_id)
}

fn cleanup_expired(state: &mut RuntimeState, now: DateTime<Utc>) {
    let expired = state
        .credentials
        .values()
        .filter(|entry| entry.expires_at <= now)
        .map(|entry| entry.controller_instance_id.clone())
        .collect::<std::collections::HashSet<_>>();
    if expired.is_empty() {
        return;
    }
    state
        .credentials
        .retain(|_, entry| !expired.contains(&entry.controller_instance_id));
    state
        .leases
        .retain(|(controller, _), _| !expired.contains(controller));
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::AcpRuntime;

    #[test]
    fn controller_credentials_authenticate_and_revoke_without_debug_leakage() {
        let runtime = AcpRuntime::new();
        let grant = runtime
            .issue_controller("brc_alpha", Duration::from_secs(60))
            .expect("controller credential is issued");

        assert!(grant.token().starts_with("brac_"));
        assert!(!format!("{grant:?}").contains(grant.token()));
        assert_eq!(
            runtime
                .authenticate(grant.token())
                .expect("credential authenticates")
                .controller_instance_id(),
            "brc_alpha"
        );

        runtime.revoke_controller("brc_alpha");
        assert!(runtime.authenticate(grant.token()).is_none());
    }

    #[test]
    fn route_leases_are_isolated_and_follow_ordered_native_candidates() {
        let runtime = AcpRuntime::new();
        let alpha = runtime
            .issue_controller("brc_alpha", Duration::from_secs(60))
            .expect("alpha credential");
        let beta = runtime
            .issue_controller("brc_beta", Duration::from_secs(60))
            .expect("beta credential");
        runtime
            .set_route("brc_alpha", "root", "anthropic:claude-sonnet")
            .expect("alpha lease");
        runtime
            .set_route("brc_alpha", "child", "openai:gpt-5")
            .expect("child lease");
        runtime
            .set_route("brc_beta", "root", "google:gemini-3")
            .expect("beta lease");

        let exact = runtime
            .resolve_route("brc_alpha", &["child", "root"])
            .expect("exact child lease");
        assert_eq!(exact.matched_session_id(), "child");
        assert_eq!(exact.route(), "openai:gpt-5");

        runtime.reset_route("brc_alpha", "child");
        let inherited = runtime
            .resolve_route("brc_alpha", &["child", "root"])
            .expect("root fallback lease");
        assert_eq!(inherited.matched_session_id(), "root");
        assert_eq!(inherited.route(), "anthropic:claude-sonnet");
        assert_eq!(
            runtime
                .resolve_route("brc_beta", &["root"])
                .expect("beta route")
                .route(),
            "google:gemini-3"
        );

        runtime.revoke_controller("brc_alpha");
        assert!(runtime.resolve_route("brc_alpha", &["root"]).is_none());
        assert!(runtime.authenticate(alpha.token()).is_none());
        assert!(runtime.authenticate(beta.token()).is_some());
        assert!(runtime.resolve_route("brc_beta", &["root"]).is_some());
    }

    #[test]
    fn expired_controller_credentials_remove_owned_leases() {
        let runtime = AcpRuntime::new();
        let grant = runtime
            .issue_controller("brc_expired", Duration::ZERO)
            .expect("credential is issued before expiry cleanup");
        runtime
            .set_route("brc_expired", "session", "openai:gpt-5")
            .expect_err("an already-expired controller cannot create a lease");

        assert!(runtime.authenticate(grant.token()).is_none());
        assert!(runtime.resolve_route("brc_expired", &["session"]).is_none());
    }

    #[test]
    fn duplicate_live_controller_identity_is_rejected() {
        let runtime = AcpRuntime::new();
        let _grant = runtime
            .issue_controller("brc_same", Duration::from_secs(60))
            .expect("first controller");

        assert!(
            runtime
                .issue_controller("brc_same", Duration::from_secs(60))
                .is_err()
        );
    }
}

//! Ephemeral ACP session-route state.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Default lifetime of one route lease. Explicit close/delete/disconnect
/// cleanup normally removes leases first; the TTL bounds crash leftovers.
pub const DEFAULT_ROUTE_LEASE_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// One effective session route lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteLease {
    lease_id: String,
    api_principal: String,
    controller_instance_id: String,
    session_id: String,
    route: String,
    expires_at: DateTime<Utc>,
}

impl RouteLease {
    /// Opaque runtime lease identity for diagnostics.
    pub fn lease_id(&self) -> &str {
        &self.lease_id
    }

    /// Opaque API principal that owns the route namespace.
    pub fn api_principal(&self) -> &str {
        &self.api_principal
    }

    /// Declared controller namespace.
    pub fn controller_instance_id(&self) -> &str {
        &self.controller_instance_id
    }

    /// ACP/native session key on which the lease was installed.
    pub fn matched_session_id(&self) -> &str {
        &self.session_id
    }

    /// BitRouter route selector installed by the controller.
    pub fn route(&self) -> &str {
        &self.route
    }

    /// Independent expiry of this route lease.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

type RouteKey = (String, String, String);

#[derive(Default)]
struct RuntimeState {
    leases: HashMap<RouteKey, RouteLease>,
}

/// Daemon-owned, in-memory ACP route leases.
///
/// Authentication remains the normal model/API-key concern. Controller and
/// session values are declared namespace claims within that API principal;
/// this runtime does not issue credentials or attest those claims.
pub struct AcpRuntime {
    state: RwLock<RuntimeState>,
    lease_ttl: Duration,
}

impl Default for AcpRuntime {
    fn default() -> Self {
        Self::with_lease_ttl(DEFAULT_ROUTE_LEASE_TTL)
    }
}

impl AcpRuntime {
    /// Construct an empty runtime with the production lease TTL.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an empty runtime with an explicit lease TTL.
    ///
    /// This is public so embeddings can choose a shorter crash-recovery bound;
    /// it does not change request authentication semantics.
    pub fn with_lease_ttl(lease_ttl: Duration) -> Self {
        Self {
            state: RwLock::new(RuntimeState::default()),
            lease_ttl,
        }
    }

    /// Create or replace one API-principal/controller/session route lease.
    pub fn set_route(
        &self,
        api_principal: &str,
        controller_instance_id: &str,
        session_id: &str,
        route: &str,
    ) -> Result<RouteLease, String> {
        let api_principal = api_principal.trim();
        let controller_instance_id = controller_instance_id.trim();
        let session_id = session_id.trim();
        let route = route.trim();
        if api_principal.is_empty()
            || controller_instance_id.is_empty()
            || session_id.is_empty()
            || route.is_empty()
        {
            return Err(
                "API principal, controller id, session id, and route must not be empty".to_string(),
            );
        }
        let ttl = chrono::Duration::from_std(self.lease_ttl)
            .map_err(|error| format!("route lease ttl is out of range: {error}"))?;
        let now = Utc::now();
        let lease = RouteLease {
            lease_id: format!("brlease_{}", uuid::Uuid::new_v4().simple()),
            api_principal: api_principal.to_string(),
            controller_instance_id: controller_instance_id.to_string(),
            session_id: session_id.to_string(),
            route: route.to_string(),
            expires_at: now + ttl,
        };
        let mut state = self.write_state();
        cleanup_expired(&mut state, now);
        state.leases.insert(
            route_key(api_principal, controller_instance_id, session_id),
            lease.clone(),
        );
        Ok(lease)
    }

    /// Remove one route lease. Returns the removed lease when present.
    pub fn reset_route(
        &self,
        api_principal: &str,
        controller_instance_id: &str,
        session_id: &str,
    ) -> Option<RouteLease> {
        let mut state = self.write_state();
        cleanup_expired(&mut state, Utc::now());
        state.leases.remove(&route_key(
            api_principal,
            controller_instance_id,
            session_id,
        ))
    }

    /// Remove every lease in one API-principal/controller namespace.
    pub fn remove_controller(&self, api_principal: &str, controller_instance_id: &str) {
        let mut state = self.write_state();
        cleanup_expired(&mut state, Utc::now());
        state.leases.retain(|(principal, controller, _), _| {
            principal != api_principal || controller != controller_instance_id
        });
    }

    /// Resolve the first matching session candidate. Candidate order expresses
    /// exact-child before root fallback.
    pub fn resolve_route(
        &self,
        api_principal: &str,
        controller_instance_id: &str,
        session_candidates: &[&str],
    ) -> Option<RouteLease> {
        let mut state = self.write_state();
        cleanup_expired(&mut state, Utc::now());
        session_candidates.iter().find_map(|session_id| {
            state
                .leases
                .get(&route_key(
                    api_principal,
                    controller_instance_id,
                    session_id,
                ))
                .cloned()
        })
    }

    /// Read the exact lease for one principal/controller/session tuple.
    pub fn current_route(
        &self,
        api_principal: &str,
        controller_instance_id: &str,
        session_id: &str,
    ) -> Option<RouteLease> {
        self.resolve_route(api_principal, controller_instance_id, &[session_id])
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, RuntimeState> {
        match self.state.write() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn route_key(api_principal: &str, controller_instance_id: &str, session_id: &str) -> RouteKey {
    (
        api_principal.to_string(),
        controller_instance_id.to_string(),
        session_id.to_string(),
    )
}

fn cleanup_expired(state: &mut RuntimeState, now: DateTime<Utc>) {
    state.leases.retain(|_, lease| lease.expires_at > now);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::AcpRuntime;

    #[test]
    fn route_leases_are_isolated_by_api_principal_and_controller_claim() {
        let runtime = AcpRuntime::new();
        runtime
            .set_route(
                "principal-a",
                "controller-a",
                "root",
                "anthropic:claude-sonnet",
            )
            .expect("principal a lease");
        runtime
            .set_route("principal-a", "controller-b", "root", "openai:gpt-5")
            .expect("controller b lease");
        runtime
            .set_route("principal-b", "controller-a", "root", "google:gemini-3")
            .expect("principal b lease");

        assert_eq!(
            runtime
                .current_route("principal-a", "controller-a", "root")
                .expect("principal a controller a route")
                .route(),
            "anthropic:claude-sonnet"
        );
        assert_eq!(
            runtime
                .current_route("principal-a", "controller-b", "root")
                .expect("principal a controller b route")
                .route(),
            "openai:gpt-5"
        );
        assert_eq!(
            runtime
                .current_route("principal-b", "controller-a", "root")
                .expect("principal b controller a route")
                .route(),
            "google:gemini-3"
        );
        assert!(
            runtime
                .current_route("principal-b", "controller-b", "root")
                .is_none()
        );
    }

    #[test]
    fn routes_follow_ordered_native_candidates_and_cleanup_one_controller() {
        let runtime = AcpRuntime::new();
        runtime
            .set_route("principal", "controller", "root", "anthropic:claude-sonnet")
            .expect("root lease");
        runtime
            .set_route("principal", "controller", "child", "openai:gpt-5")
            .expect("child lease");
        runtime
            .set_route("principal", "other", "root", "google:gemini-3")
            .expect("other controller lease");

        let exact = runtime
            .resolve_route("principal", "controller", &["child", "root"])
            .expect("exact child lease");
        assert_eq!(exact.matched_session_id(), "child");
        assert_eq!(exact.route(), "openai:gpt-5");

        runtime.reset_route("principal", "controller", "child");
        assert_eq!(
            runtime
                .resolve_route("principal", "controller", &["child", "root"])
                .expect("root fallback")
                .route(),
            "anthropic:claude-sonnet"
        );

        runtime.remove_controller("principal", "controller");
        assert!(
            runtime
                .current_route("principal", "controller", "root")
                .is_none()
        );
        assert!(
            runtime
                .current_route("principal", "other", "root")
                .is_some()
        );
    }

    #[test]
    fn route_lease_expiry_is_independent_per_lease() {
        let runtime = AcpRuntime::with_lease_ttl(Duration::ZERO);
        let lease = runtime
            .set_route("principal", "controller", "session", "openai:gpt-5")
            .expect("lease can be installed");
        assert_eq!(lease.api_principal(), "principal");
        assert_eq!(lease.controller_instance_id(), "controller");
        assert!(
            runtime
                .current_route("principal", "controller", "session")
                .is_none(),
            "zero-TTL lease is removed on the next lookup"
        );
    }

    #[test]
    fn same_principal_can_deliberately_reuse_an_exact_claim_namespace() {
        let runtime = AcpRuntime::new();
        runtime
            .set_route("shared", "controller", "session", "first")
            .expect("first lease");
        runtime
            .set_route("shared", "controller", "session", "second")
            .expect("replacement lease");

        assert_eq!(
            runtime
                .current_route("shared", "controller", "session")
                .expect("shared namespace route")
                .route(),
            "second"
        );
    }
}

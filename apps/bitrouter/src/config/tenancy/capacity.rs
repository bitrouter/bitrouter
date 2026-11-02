//! Startup resource budgets, independent of persistent directory cardinality.

use anyhow::{Result, ensure};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// Finite process budgets. These values never limit stored tenant or key counts.
/// Effective values are traced at startup; the existing receipt-latency telemetry
/// opt-in also exports numeric cache/routing usage and cached volume observations.
/// These are accounting estimates, not measured RSS or an allocation guarantee.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityConfig {
    pub execution: Execution,
    pub ingress: Ingress,
    pub management: Management,
    pub caches: Caches,
    pub authority_views: AuthorityViews,
    pub routing: Routing,
    pub receipts: Receipts,
    pub transport: Transport,
}

/// Active owners retain their slot through required cleanup, including disconnects.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Execution {
    pub instance_active: usize,
    pub tenant_active: usize,
    /// Zero disables waiting; it never means an unbounded queue.
    pub instance_queue: usize,
    /// Request limits and the instance queue can further narrow this value.
    pub tenant_queue: usize,
    pub queue_wait_ms: u64,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            instance_active: 128,
            tenant_active: 16,
            instance_queue: 256,
            tenant_queue: 32,
            queue_wait_ms: 5_000,
        }
    }
}

/// Aggregate upload ownership, separate from the fixed protocol body ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Ingress {
    pub requests: usize,
    pub bytes: usize,
}

impl Default for Ingress {
    fn default() -> Self {
        Self {
            requests: 128,
            bytes: 64 * 1024 * 1024,
        }
    }
}

/// Bounded management work; safety has a distinct queue from ordinary requests.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Management {
    pub queue: usize,
    pub safety_queue: usize,
    pub reads: usize,
}

impl Default for Management {
    fn default() -> Self {
        Self {
            queue: 16,
            safety_queue: 8,
            reads: 8,
        }
    }
}

/// Immutable caches charge both metadata and pinned value allocations.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Caches {
    pub execution_entries: usize,
    pub execution_bytes: usize,
    pub credential_entries: usize,
    pub credential_bytes: usize,
}

impl Default for Caches {
    fn default() -> Self {
        Self {
            execution_entries: 64,
            execution_bytes: 128 * 1024 * 1024,
            credential_entries: 256,
            credential_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Authorization views loaded on demand; directory entries are not preallocated.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuthorityViews {
    pub entries: usize,
    /// One third holds leased and optionally retained facts; two thirds cover
    /// staged replacements and construction overlap. Allocation is lazy.
    pub bytes: usize,
}

impl Default for AuthorityViews {
    fn default() -> Self {
        Self {
            entries: 512,
            bytes: 64 * 1024 * 1024,
        }
    }
}

/// Retained routing windows and health state; active send ownership is derived.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub entries: usize,
    pub bytes: usize,
}

impl Default for Routing {
    fn default() -> Self {
        Self {
            entries: 2048,
            bytes: 64 * 1024 * 1024,
        }
    }
}

/// Admission policy does not invalidate retained evidence on restart.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Receipts {
    pub high_water_bytes: u64,
    pub admission_free_bytes: u64,
    pub maintenance_free_bytes: u64,
    pub unknown_instance: usize,
    pub unknown_tenant: usize,
}

impl Default for Receipts {
    fn default() -> Self {
        Self {
            high_water_bytes: 8 * 1024 * 1024 * 1024,
            admission_free_bytes: 768 * 1024 * 1024,
            maintenance_free_bytes: 256 * 1024 * 1024,
            unknown_instance: 1000,
            unknown_tenant: 100,
        }
    }
}

/// Connections are owned before parsing; request slots share the ingress budget.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Transport {
    pub connections: usize,
    pub header_timeout_ms: u64,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            connections: 256,
            header_timeout_ms: 30_000,
        }
    }
}

impl CapacityConfig {
    /// Validate representation and budget relationships without allocating owners.
    pub(crate) fn validate(&self) -> Result<()> {
        for (name, value, zero) in [
            (
                "execution.instance_active",
                self.execution.instance_active,
                false,
            ),
            (
                "execution.tenant_active",
                self.execution.tenant_active,
                false,
            ),
            (
                "execution.instance_queue",
                self.execution.instance_queue,
                true,
            ),
            ("execution.tenant_queue", self.execution.tenant_queue, true),
            ("ingress.requests", self.ingress.requests, false),
            ("management.queue", self.management.queue, false),
            (
                "management.safety_queue",
                self.management.safety_queue,
                false,
            ),
            ("management.reads", self.management.reads, false),
            ("transport.connections", self.transport.connections, false),
        ] {
            // Tokio constructors panic above this representational limit.
            // https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html#associatedconstant.MAX_PERMITS
            ensure!(
                (zero || value > 0) && value <= tokio::sync::Semaphore::MAX_PERMITS,
                "tenancy.capacity.{name} exceeds the runtime permit range"
            );
        }
        for (name, value) in [
            ("ingress.bytes", self.ingress.bytes),
            ("caches.execution_entries", self.caches.execution_entries),
            ("caches.execution_bytes", self.caches.execution_bytes),
            ("caches.credential_entries", self.caches.credential_entries),
            ("caches.credential_bytes", self.caches.credential_bytes),
            ("authority_views.entries", self.authority_views.entries),
            ("authority_views.bytes", self.authority_views.bytes),
            ("routing.entries", self.routing.entries),
            ("routing.bytes", self.routing.bytes),
            ("receipts.unknown_instance", self.receipts.unknown_instance),
            ("receipts.unknown_tenant", self.receipts.unknown_tenant),
        ] {
            ensure!(
                value > 0 && value <= isize::MAX as usize,
                "tenancy.capacity.{name} exceeds the addressable range"
            );
        }
        ensure!(
            self.execution.tenant_active <= self.execution.instance_active
                && self.execution.tenant_queue <= self.execution.instance_queue,
            "tenancy.capacity tenant execution budgets exceed instance budgets"
        );
        ensure!(
            self.receipts.unknown_tenant <= self.receipts.unknown_instance,
            "tenancy.capacity.receipts unknown tenant budget exceeds instance budget"
        );
        ensure!(
            self.receipts.high_water_bytes > 0
                && self.receipts.high_water_bytes <= i64::MAX as u64
                && self.receipts.maintenance_free_bytes > 0
                && self.receipts.admission_free_bytes > self.receipts.maintenance_free_bytes
                && self.receipts.admission_free_bytes <= i64::MAX as u64,
            "tenancy.capacity.receipts has invalid disk budgets"
        );
        for (name, millis) in [
            ("execution.queue_wait_ms", self.execution.queue_wait_ms),
            (
                "transport.header_timeout_ms",
                self.transport.header_timeout_ms,
            ),
        ] {
            ensure!(
                millis > 0
                    && Instant::now()
                        .checked_add(Duration::from_millis(millis))
                        .is_some(),
                "tenancy.capacity.{name} exceeds the host clock range"
            );
        }
        // Validate aggregate owner bookkeeping before any channel is constructed.
        self.execution
            .instance_active
            .checked_add(self.execution.instance_queue)
            .and_then(|n| n.checked_add(self.management.reads))
            .and_then(|n| n.checked_mul(std::mem::size_of::<usize>()))
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or_else(|| anyhow::anyhow!("tenancy.capacity aggregate owner budget overflows"))?;
        self.caches
            .execution_bytes
            .checked_add(self.caches.credential_bytes)
            .and_then(|n| n.checked_add(self.authority_views.bytes))
            .and_then(|n| n.checked_add(self.routing.bytes))
            .and_then(|n| n.checked_add(self.ingress.bytes))
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or_else(|| anyhow::anyhow!("tenancy.capacity aggregate memory budget overflows"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

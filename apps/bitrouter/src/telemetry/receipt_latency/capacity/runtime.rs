//! Fixed numeric copies of existing hot working-set accounting.
use super::ReadError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CacheUsage {
    pub entries: u64,
    pub bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoutingUsage {
    pub targets: u64,
    pub providers: u64,
    pub bytes: u64,
    pub active: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub executions: Result<CacheUsage, ReadError>,
    pub credentials: Result<CacheUsage, ReadError>,
    pub routing: Result<RoutingUsage, ReadError>,
}
impl Snapshot {
    pub(crate) fn bytes(self) -> [(&'static str, Result<u64, ReadError>); 3] {
        [
            ("execution_cache_bytes", self.executions.map(|v| v.bytes)),
            ("credential_cache_bytes", self.credentials.map(|v| v.bytes)),
            ("routing_bytes", self.routing.map(|v| v.bytes)),
        ]
    }
    pub(crate) fn state(self) -> [(&'static str, Result<u64, ReadError>); 8] {
        [
            (
                "execution_cache_entries",
                self.executions.map(|v| v.entries),
            ),
            (
                "credential_cache_entries",
                self.credentials.map(|v| v.entries),
            ),
            ("routing_target_entries", self.routing.map(|v| v.targets)),
            (
                "routing_provider_entries",
                self.routing.map(|v| v.providers),
            ),
            ("routing_active", self.routing.map(|v| v.active)),
            (
                "execution_cache_available",
                Ok(u64::from(self.executions.is_ok())),
            ),
            (
                "credential_cache_available",
                Ok(u64::from(self.credentials.is_ok())),
            ),
            ("routing_available", Ok(u64::from(self.routing.is_ok()))),
        ]
    }
}

//! Application-only agent, observation and retained harness records.

// Keep diagnostics bounded for records that can contain tool or model content.
macro_rules! redacted_contract_debug {
    ($($name:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    )+};
}

pub mod agent;
pub mod inspection;
pub mod item;
pub mod resources;
pub mod runtime;
pub mod skills;
pub mod thread;
pub mod turn;

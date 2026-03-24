//! Hot-reload support for routing tables.
//!
//! The [`ReloadableRoutingTable`] trait allows replacing the inner
//! (config-derived) routing table at runtime without restarting the process.
//! Dynamic routes added via [`AdminRoutingTable`](super::admin::AdminRoutingTable)
//! are preserved across reloads.

use crate::errors::Result;

/// A routing table whose underlying configuration can be replaced at runtime.
///
/// SDK users can implement this trait to support hot-reloading their own
/// routing table implementations.  The built-in
/// [`DynamicRoutingTable`](super::dynamic::DynamicRoutingTable) implements
/// this trait automatically for any inner `T`.
pub trait ReloadableRoutingTable {
    /// The inner routing table type that can be swapped in.
    type Inner;

    /// Replace the inner routing table with a freshly loaded one.
    ///
    /// Dynamically-added routes are **not** affected by this operation — only
    /// the config-derived base table is swapped.
    fn reload(&self, inner: Self::Inner) -> Result<()>;
}

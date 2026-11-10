//! Apply published policy blocks, preserving enrollment and request fences.

pub mod allocation;
#[cfg(feature = "semantic-routing")]
pub(crate) mod catalog;
pub mod control;
#[cfg(feature = "semantic-routing")]
pub mod runtime;
pub mod store;

#[cfg(all(test, feature = "semantic-routing"))]
pub(crate) mod fixtures;

#[cfg(all(test, feature = "semantic-routing"))]
mod tests;

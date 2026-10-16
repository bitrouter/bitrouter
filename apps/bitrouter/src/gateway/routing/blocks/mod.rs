//! Apply published policy blocks, preserving enrollment and request fences.

pub mod allocation;
pub(crate) mod catalog;
pub mod control;
pub mod runtime;
pub mod store;

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests;

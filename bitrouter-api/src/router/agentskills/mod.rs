//! Agent skills CRUD routes.
//!
//! Provides Warp filters that expose `/v1/skills` endpoints for
//! registering, listing, retrieving, and deleting agent skills.

mod filters;

#[cfg(test)]
mod tests;

pub use filters::skills_filter;

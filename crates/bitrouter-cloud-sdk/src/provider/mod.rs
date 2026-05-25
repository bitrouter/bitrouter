//! `bitrouter-cloud` LLM provider — `AuthApplier` implementation that
//! prefers the OAuth bearer from [`crate::auth::credentials`] (auto-refreshing
//! within [`crate::auth::credentials::REFRESH_WINDOW`] of expiry), falls back
//! to a `brk_…` API key carried on the routing target, and otherwise returns
//! a 401 with the onboarding guidance text.
//!
//! Registered by `apps/bitrouter` against `provider_name == "bitrouter-cloud"`.

mod applier;

pub use applier::{BitrouterCloudAuthApplier, PROVIDER_ID, onboarding_hint};

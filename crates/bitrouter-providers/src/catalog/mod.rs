//! The public model catalog at <https://models.dev/api.json>.
//!
//! A provider may declare a `models_dev` auto-sync feed: the channel the
//! registry itself curates from. The OSS reads the SAME channel at runtime to
//! pull that provider's FULL catalog (beyond the curated canonical subset) and
//! merge the non-curated models in — the curated canonical models keep the
//! highest route priority. (A `v1_models` feed is discovered from the
//! provider's own `/models` endpoint instead, via the SDK's `auto_discover`.)
//!
//! - [`types`] — the parsed JSON shape, pure data, no I/O.
//! - [`fetch`] — async `reqwest` download of the catalog document.
//!
//! The catalog application step lives in
//! [`registry::apply`](crate::registry::apply) alongside the registry merge it
//! enriches.

pub mod fetch;
pub mod types;

//! The built-in `web_search` server tool and its BYOK backends.
//!
//! When the deployment configures `server_tools.web_search` and a request
//! declares `bitrouter:web_search`, BitRouter advertises a single `web_search`
//! function tool, intercepts the model's calls, runs the query against a
//! configured search backend, and feeds normalized results back into the loop —
//! the same server-tool mechanism the advisor / sub-agent / fusion tools use.
//!
//! Backends come in two flavors behind one [`backend::WebSearchBackend`] seam:
//! BYOK HTTP search APIs ([`http::HttpSearchBackend`] — Parallel / Exa /
//! Firecrawl / Tavily) and a nested model completion
//! ([`nested::NestedSearchBackend`] — a provider's native search reused for
//! every model).
//!
//! Per crate guideline 2, this module does not `pub use` from its submodules;
//! downstream reaches types directly (e.g. `web_search::toolset::WebSearchToolset`).

pub mod backend;
pub mod config;
pub mod http;
pub mod nested;
pub mod toolset;

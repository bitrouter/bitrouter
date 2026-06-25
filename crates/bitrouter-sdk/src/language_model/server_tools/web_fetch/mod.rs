//! The built-in `web_fetch` server tool: fetches one URL's content through a
//! BYOK extraction backend (Exa / Firecrawl / Tavily) and returns normalized
//! page content. Structurally a sibling of `web_search` — an ordered
//! preference/failover backend list, declaration-gated advertisement, and a
//! stable result schema independent of which backend served the call.

pub mod backend;

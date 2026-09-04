//! BitRouter's action contract, plus its MCP binding — exposing BitRouter's
//! own tools (`complete` / `list_models` / `status`) over stdio and streamable
//! HTTP.
//!
//! Distinct from the MCP *gateway* in `bitrouter-sdk::mcp`, which proxies
//! *upstream* MCP servers. This crate is the *origin* server for BitRouter's
//! own capabilities.
//!
//! [`actions`] holds the shared report types and their port traits: one typed
//! answer per question, so the CLI leaf and the MCP tool cannot drift apart.
//! The implementations live app-side; this crate keeps no business logic for
//! an action it owns.

pub mod actions;
pub mod backend;
pub mod capabilities;
pub mod error;
pub mod install;
pub mod server;

use std::path::PathBuf;

/// Parameters for `install`.
pub struct InstallOptions {
    pub client: install::Client,
    /// When set, write+merge into this config path; otherwise print to stdout.
    pub config_path: Option<PathBuf>,
}

/// Render (and optionally merge+write) the MCP client config block.
pub fn install(opts: InstallOptions) -> anyhow::Result<()> {
    let block = install::render_block(opts.client);
    match opts.config_path {
        None => {
            println!("{}", serde_json::to_string_pretty(&block)?);
            Ok(())
        }
        Some(path) => {
            let mut doc: serde_json::Value = if path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&path)?)
                    .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?
            } else {
                serde_json::json!({})
            };
            install::merge_into(&mut doc, &block);
            std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
            println!("wrote bitrouter MCP server into {}", path.display());
            Ok(())
        }
    }
}

/// Which wire transport the server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (local clients launch this).
    Stdio,
    /// Streamable HTTP, mounted at `/mcp-control`.
    Http,
}

/// Which backend the tools route to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The local BYOK daemon at `127.0.0.1:4356`.
    Local,
    /// BitRouter Cloud at `api.bitrouter.ai`.
    Cloud,
}

/// Parameters for `serve`.
pub struct ServeOptions {
    pub transport: Transport,
    pub backend: BackendKind,
    /// Local daemon root. Default `http://127.0.0.1:4356`.
    pub local_url: String,
    /// Cloud root. Default `https://api.bitrouter.ai`.
    pub cloud_url: String,
    /// Bearer for the cloud backend (from `--token` / `BITROUTER_TOKEN`).
    pub cloud_token: Option<String>,
    /// HTTP bind address (only for `Transport::Http`). Default `127.0.0.1:4357`.
    pub bind: String,
    /// Optional spend annotator for tool results (stdio transport only —
    /// the HTTP transport is multi-tenant and per-caller spend isn't
    /// what the local metering database holds).
    pub cost_footer: Option<std::sync::Arc<dyn server::CostFooter>>,
    /// Optional `route` port backing `route_preview` (stdio transport only —
    /// it reads the serving machine's own routing table, which is not what a
    /// multi-tenant HTTP caller is asking about).
    pub routing: Option<std::sync::Arc<dyn actions::route::RouteQuery>>,
    /// Optional `status` port. When unset, the backend's own
    /// [`status_port`](backend::Backend::status_port) is used — which is how
    /// the cloud profile reports its remaining credit. The local profile has no
    /// such fallback: only the embedding binary can read the control socket and
    /// the metering database.
    pub status: Option<std::sync::Arc<dyn actions::status::StatusQuery>>,
    /// Optional `list_models` port. When unset, the backend's own
    /// [`models_port`](backend::Backend::models_port) is used — a
    /// `GET /v1/models` against the daemon or the cloud account. The local
    /// profile injects a better one: it reads the daemon's live routing table
    /// over the control socket and falls back to static config, so
    /// `list_models` answers with **no daemon running**.
    pub models: Option<std::sync::Arc<dyn actions::models::ModelsQuery>>,
    /// Optional `skills_search` / `skills_get` port (stdio transport only — it
    /// reads the serving machine's own installed-skills roots, which a
    /// multi-tenant HTTP caller has no claim on). Set on **every** stdio
    /// profile, not just `--backend skills`: an `mcp install`-ed client runs
    /// `bitrouter mcp serve`, and until this existed such a client never saw a
    /// skill.
    pub skills: Option<std::sync::Arc<dyn actions::skills::SkillsQuery>>,
    /// Optional SEP-2640 catalog (`skills/list` / `skills/get` plus
    /// `resources/*`), on the same stdio-only terms as [`Self::skills`]. Wire
    /// both or neither: they are two views of one installed-skills tree, and a
    /// client that got one without the other would be told a different set of
    /// skills depending on which surface it asked.
    pub skill_catalog: Option<std::sync::Arc<dyn capabilities::skill_catalog::SkillCatalog>>,
}

/// Run the MCP server to completion: the router profile (completion, plus
/// `route_preview`, `skills_search`/`skills_get` and the SEP-2640 catalog when
/// their ports are wired — which the embedding binary does for every stdio
/// profile). The narrow `--backend skills` gateway-subprocess profile is still
/// assembled directly through [`server::BitrouterMcp::builder`].
pub async fn serve(mut opts: ServeOptions) -> anyhow::Result<()> {
    let backend = server::build_backend(
        opts.backend,
        opts.transport,
        &opts.local_url,
        &opts.cloud_url,
        opts.cloud_token.as_deref(),
    )?;
    match opts.transport {
        Transport::Stdio => {
            let cost_footer = opts.cost_footer.take();
            server::serve_stdio(stdio_profile(backend, opts), cost_footer).await
        }
        Transport::Http => {
            let require_auth = matches!(opts.backend, BackendKind::Cloud);
            // Without the auth middleware (local backend), a non-loopback bind
            // would expose the BYOK daemon's provider keys to the network.
            if !require_auth {
                server::ensure_loopback_bind(&opts.bind)?;
            }
            server::serve_http(backend, &opts.bind, require_auth).await
        }
    }
}

/// The whole stdio tool surface, in one function so a test can assert it — the
/// mirror of `server::http_profile`, and the reason the two profiles can be
/// compared rather than reasoned about.
///
/// Everything host-bound lives here and nowhere else: `route_preview` resolves
/// against the serving machine's config and control socket, and the skills
/// surfaces read its installed-skills roots. That is legitimate over stdio,
/// where the server is a subprocess of the caller whose machine it is, and it
/// is exactly what must never reach the multi-tenant HTTP profile — which is
/// built from an `Arc<dyn Backend>` alone and therefore *cannot* reach these
/// ports even by accident.
fn stdio_profile(
    backend: std::sync::Arc<dyn backend::Backend>,
    opts: ServeOptions,
) -> server::BitrouterMcp {
    let mut builder = server::BitrouterMcp::builder().completion(backend.clone());
    // The injected port wins: on the local profile it is the control socket,
    // which knows the pid and the socket path the backend's `/v1/*` client
    // never could.
    if let Some(models) = opts.models.or_else(|| backend.clone().models_port()) {
        builder = builder.models(models);
    }
    if let Some(status) = opts.status.or_else(|| backend.status_port()) {
        builder = builder.status(status);
    }
    if let Some(routing) = opts.routing {
        builder = builder.routing(routing);
    }
    if let Some(skills) = opts.skills {
        builder = builder.skills(skills);
    }
    if let Some(catalog) = opts.skill_catalog {
        builder = builder.skill_catalog(catalog);
    }
    builder.build()
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::actions::skills::{SkillDetail, SkillsQuery, SkillsReport};
    use crate::backend::{Backend, BackendError, CallerAuth, CompleteRequest, CompleteResponse};
    use crate::error::ToolError;
    use bitrouter_sdk::mcp::skills::{GetSkillResult, ListSkillsResult};
    use std::sync::Arc;

    struct StubSkills;

    #[async_trait::async_trait]
    impl SkillsQuery for StubSkills {
        async fn list(&self) -> Result<SkillsReport, ToolError> {
            Ok(SkillsReport { skills: Vec::new() })
        }
        async fn get(&self, name: &str) -> Result<SkillDetail, ToolError> {
            Err(ToolError::new(format!("no installed skill named '{name}'")))
        }
    }

    #[async_trait::async_trait]
    impl capabilities::skill_catalog::SkillCatalog for StubSkills {
        async fn list(&self) -> Result<ListSkillsResult, ToolError> {
            Ok(ListSkillsResult { skills: Vec::new() })
        }
        async fn get(&self, uri: &str) -> Result<GetSkillResult, ToolError> {
            Err(ToolError::new(format!("no installed skill at '{uri}'")))
        }
        async fn read(
            &self,
            uri: &str,
        ) -> Result<capabilities::skill_catalog::SkillFile, ToolError> {
            Err(ToolError::new(format!("'{uri}' is not a file")))
        }
    }

    /// A backend that cannot complete anything — the tool *surface* is what
    /// these assertions are about, not what a call returns.
    struct StubBackend;

    #[async_trait::async_trait]
    impl Backend for StubBackend {
        async fn complete(
            &self,
            _: &CallerAuth,
            _: CompleteRequest,
        ) -> Result<CompleteResponse, BackendError> {
            Err(BackendError::Transport("stub".into()))
        }
        fn status_port(self: Arc<Self>) -> Option<Arc<dyn actions::status::StatusQuery>> {
            None
        }
        fn models_port(self: Arc<Self>) -> Option<Arc<dyn actions::models::ModelsQuery>> {
            None
        }
    }

    fn options(skills: bool) -> ServeOptions {
        ServeOptions {
            transport: Transport::Stdio,
            backend: BackendKind::Local,
            local_url: "http://127.0.0.1:4356".into(),
            cloud_url: "https://api.bitrouter.ai".into(),
            cloud_token: None,
            bind: "127.0.0.1:4357".into(),
            cost_footer: None,
            routing: None,
            status: None,
            models: None,
            skills: skills.then(|| Arc::new(StubSkills) as Arc<dyn SkillsQuery>),
            skill_catalog: skills.then(|| {
                Arc::new(StubSkills) as Arc<dyn capabilities::skill_catalog::SkillCatalog>
            }),
        }
    }

    fn tools(server: &server::BitrouterMcp) -> Vec<String> {
        let mut names: Vec<String> = server
            .tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// The disjoint-profiles fix, asserted where it bites: `mcp install` writes
    /// `["mcp", "serve"]`, which is [`serve`] on stdio — not `--backend skills`
    /// — so before this an installed client never saw a skill even though the
    /// binary could serve them.
    #[test]
    fn an_installed_client_sees_the_skills_surfaces() {
        let installed = install::render_block(install::Client::Claude);
        assert_eq!(
            installed["mcpServers"]["bitrouter"]["args"],
            serde_json::json!(["mcp", "serve"]),
            "this test is only meaningful while `mcp install` writes `mcp serve`",
        );

        let server = stdio_profile(Arc::new(StubBackend), options(true));
        let names = tools(&server);
        for expected in ["skills_get", "skills_search"] {
            assert!(
                names.contains(&expected.to_string()),
                "an installed client must see `{expected}`: {names:?}"
            );
        }
        // And the SEP-2640 surface, which is methods rather than tools, so it
        // shows up as the declared extension instead.
        let extensions = rmcp::ServerHandler::get_info(&server)
            .capabilities
            .extensions
            .expect("a wired catalog declares the skills extension");
        assert!(extensions.contains_key(bitrouter_sdk::mcp::skills::SKILLS_EXTENSION_ID));
    }

    /// Invariant 2, from the other side: wiring more tools into stdio must not
    /// widen the HTTP profile. The HTTP profile is assembled from an
    /// `Arc<dyn Backend>` and nothing else, so it has no way to reach a skills
    /// port — this pins that, tools *and* declared capabilities.
    #[test]
    fn wiring_skills_into_stdio_does_not_widen_the_http_profile() {
        let http = server::http_profile_for_test(Arc::new(StubBackend));
        let names = tools(&http);
        for hidden in ["skills_search", "skills_get", "route_preview"] {
            assert!(
                !names.contains(&hidden.to_string()),
                "the HTTP profile must not carry `{hidden}`: {names:?}"
            );
        }
        let capabilities = rmcp::ServerHandler::get_info(&http).capabilities;
        assert!(
            capabilities.extensions.is_none() && capabilities.resources.is_none(),
            "no skills extension and no resources over HTTP: {capabilities:?}"
        );

        // The stdio profile with no skills ports wired has no skills surface
        // either — so what changed is the wiring, not the transport doing
        // something implicit.
        let bare = stdio_profile(Arc::new(StubBackend), options(false));
        assert!(!tools(&bare).contains(&"skills_search".to_string()));
        assert!(
            rmcp::ServerHandler::get_info(&bare)
                .capabilities
                .extensions
                .is_none()
        );
    }
}

//! `BitrouterMcp` — the rmcp origin server handler. One handler assembles its
//! profiles from named `#[tool_router]` blocks: a **public** profile
//! (`list_models` and `status`, wherever those ports are wired — both
//! HTTP-safe), the stdio **router** profile (that plus `route_preview`), and
//! the **skills** origin profile. The
//! [`Builder`] merges only the routers whose capability is wired, so an
//! unwired capability's tools are never registered — a public HTTP client
//! can't so much as see `route_preview`.
//!
//! Every tool here is control or introspection. There is no inference tool:
//! running a completion goes through the daemon's HTTP API
//! (`/v1/messages`, `/v1/chat/completions`), which is the transport built for
//! it — streaming, the full parameter surface, and the metering path included.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    ClientJsonRpcMessage, ClientRequest, ErrorData, GetMeta, ProtocolVersion, RequestId,
    ServerCapabilities, ServerInfo, ServerJsonRpcMessage,
};
use rmcp::service::RequestContext;
use rmcp::transport::Transport;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};

use crate::actions::models::{ListModelsArgs, ModelsQuery, ModelsReport};
use crate::actions::route::{RouteInput, RouteQuery, RouteReport};
use crate::actions::skills::{
    SkillDetail, SkillsGetArgs, SkillsQuery, SkillsReport, SkillsSearchArgs,
};
use crate::actions::status::{StatusQuery, StatusReport};
use crate::backend::{Backend, CallerAuth};
use crate::capabilities::skill_catalog::{SkillCatalog, SkillFileBody};
use crate::error::ToolError;
use bitrouter_sdk::mcp::skills::{
    GetSkillParams, SKILLS_EXTENSION_ID, SKILLS_GET_METHOD, SKILLS_LIST_METHOD,
};

/// Extract the caller's bearer from MCP request extensions. The streamable-HTTP
/// transport injects `http::request::Parts`; returns an empty `CallerAuth` over
/// stdio (no parts) or when no/!Bearer `Authorization` is present.
fn caller_from_extensions(ext: &rmcp::model::Extensions) -> CallerAuth {
    let bearer = ext
        .get::<http::request::Parts>()
        .and_then(|p| p.headers.get(http::header::AUTHORIZATION))
        .and_then(|h| h.to_str().ok())
        .and_then(parse_bearer)
        .map(str::to_owned);
    CallerAuth { bearer }
}

/// Token from a `Bearer <token>` Authorization value. The scheme is matched
/// case-insensitively per RFC 7235 (`bearer`/`BEARER` are equally valid). A
/// whitespace-only token (`"Bearer "`) is `None`: an empty credential must
/// not count as "present" at the pre-auth gate.
fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// The capability ports plus their port-adjacent state, declared once and
/// shared between [`Builder`] (which fills it) and the built handler (which
/// reads it) — a capability is never a pair of parallel field declarations.
#[derive(Clone, Default)]
struct Caps {
    /// The `status` action's port. Which side fills it depends on the
    /// deployment: a local daemon's liveness comes off the control socket and
    /// its spend off the local metering database (both injected app-side), a
    /// metered account's remaining credit off the cloud backend itself.
    status_query: Option<Arc<dyn StatusQuery>>,
    /// The `list_models` action's port. Filled on the same terms as
    /// `status_query`: on stdio + local the catalog comes off the daemon's
    /// control socket (with a static-config fallback that answers with no
    /// daemon at all), while a `--local-url` or cloud client gets the backend's
    /// own `GET /v1/models`.
    models_query: Option<Arc<dyn ModelsQuery>>,
    routing: Option<Arc<dyn RouteQuery>>,
    skills: Option<Arc<dyn SkillsQuery>>,
    /// The SEP-2640 skills surface (`skills/list` / `skills/get` plus
    /// `resources/*` over skill files). Contributes no tools — it is served as
    /// JSON-RPC methods and resources — so it stays a plain field rather than
    /// a [`CapSpec`] entry.
    skill_catalog: Option<Arc<dyn SkillCatalog>>,
}

/// One tool-contributing capability: whether it's wired, the tool router it
/// contributes, and the server-instruction fragment describing its tools.
/// [`Builder::build`] and [`BitrouterMcp::instructions`] iterate the same
/// table, so a capability's registration and its guidance can't drift apart —
/// adding a capability is one entry here plus its [`Caps`] field and
/// [`Builder`] setter.
struct CapSpec {
    wired: fn(&Caps) -> bool,
    router: fn() -> ToolRouter<BitrouterMcp>,
    /// The instruction fragment describing this capability's tools.
    instructions: fn(&Caps) -> String,
}

/// What every profile says about itself, before any capability adds its own
/// guidance. Kept separate from [`CAPABILITIES`] because it describes the
/// server rather than a tool: a profile with nothing but the SEP-2640 skills
/// catalog wired registers no tools at all and still has to introduce itself.
const BASE_INSTRUCTIONS: &str = "BitRouter origin MCP server — control and \
     introspection. It runs no inference: send completions to the daemon's \
     HTTP API (`/v1/messages`, `/v1/chat/completions`) instead.";

/// The tool-contributing capabilities, in registration + instruction order.
/// State-only capabilities (the SEP-2640 skill catalog) contribute no router
/// and stay plain fields.
const CAPABILITIES: &[CapSpec] = &[
    CapSpec {
        wired: |caps| caps.models_query.is_some(),
        router: BitrouterMcp::models_router,
        instructions: |_| {
            "`list_models` lists every routable model with **all** the providers \
             that can serve it, in fallback order — pass `provider` to narrow it. \
             `resolved_via` says whether a running router answered (`live`) or the \
             list was projected from static config (`config`)."
                .to_string()
        },
    },
    CapSpec {
        wired: |caps| caps.status_query.is_some(),
        router: BitrouterMcp::status_router,
        instructions: |_| {
            "`status` reports whether BitRouter is running (pid, listen address, \
             routable models, providers, control socket) and the spend position: \
             what has been spent, and what is left where a cap exists."
                .to_string()
        },
    },
    CapSpec {
        wired: |caps| caps.routing.is_some(),
        router: BitrouterMcp::routing_router,
        instructions: |_| {
            "`route_preview` shows how a model/prompt would route — the effective model \
             the policy table selects, the provider fallback chain, and the first hop's \
             per-token rates — without sending anything."
                .to_string()
        },
    },
    CapSpec {
        wired: |caps| caps.skills.is_some(),
        router: BitrouterMcp::skills_router,
        instructions: |_| {
            "`skills_search` / `skills_get` browse the skills installed on this machine \
             and fetch one's full body. A skill listed with `valid: false` carries a \
             `problem` explaining why it cannot be loaded."
                .to_string()
        },
    },
];

#[derive(Clone)]
pub struct BitrouterMcp {
    caps: Caps,
    tool_router: ToolRouter<BitrouterMcp>,
}

// ── the `list_models` action (guarded on `self.caps.models_query`) ──
#[tool_router(router = models_router)]
impl BitrouterMcp {
    #[tool(
        description = "List the models BitRouter can route. Each entry carries every provider \
                       that can serve it, in fallback order — not just the first — so `providers` \
                       is the chain a request would walk. `resolved_via` is `live` when a running \
                       router answered and `config` when the list was projected from static \
                       configuration (what the file says now, resolved the way a daemon would \
                       at start-up — a running daemon's `reload`s are not reflected). Pass \
                       `provider` to list only what one provider declares.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_models(
        &self,
        Parameters(args): Parameters<ListModelsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::handler::server::wrapper::Json<ModelsReport>, McpError> {
        let caller = caller_from_extensions(&ctx.extensions);
        // `Json<ModelsReport>` rather than a `CallToolResult`: that is what
        // makes rmcp derive the tool's `output_schema` from the shared report
        // type, which is the agreement `actions::ACTIONS` asserts.
        self.models_query()?
            .list_models(&caller)
            .await
            .map(|report| report.filtered(args.provider.as_deref()))
            .map(rmcp::handler::server::wrapper::Json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

// ── the `status` action (guarded on `self.caps.status_query`) ──
#[tool_router(router = status_router)]
impl BitrouterMcp {
    #[tool(
        description = "Report BitRouter status: whether it is running (pid, listen address, \
                       routable models, providers, control socket) and the spend position \
                       — `spend.spent` is what has already gone (a locally metered estimate; \
                       read `spend.spent.unpriced` for the requests it could not price), and \
                       `spend.limit` is what is left where a cap exists. A stopped daemon is \
                       `running: false`, not an error.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn status(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::handler::server::wrapper::Json<StatusReport>, McpError> {
        let caller = caller_from_extensions(&ctx.extensions);
        // Deliberately *not* a `CallToolResult`: returning `Json<StatusReport>`
        // is what makes rmcp derive the tool's `output_schema` from the shared
        // report type, which is the agreement `actions::ACTIONS` asserts.
        //
        // Spend rides along as typed data under `StatusReport::spend` rather
        // than as a free-text footer, which is strictly richer: `unpriced` and
        // the remaining cap have no line in a one-sentence footer.
        self.status_query()?
            .status(&caller)
            .await
            .map(rmcp::handler::server::wrapper::Json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

// ── the router profile's routing slice (guarded on `self.routing`) ──
#[tool_router(router = routing_router)]
impl BitrouterMcp {
    #[tool(
        description = "Preview how BitRouter would route a model/prompt: the effective model the \
                       policy table selects (which can differ from the one requested), the \
                       resolved provider fallback chain, the policy decision behind it, and the \
                       first hop's per-token rates. Read-only — nothing is sent upstream, and \
                       no credential is surfaced.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn route_preview(
        &self,
        Parameters(input): Parameters<RouteInput>,
    ) -> Result<rmcp::handler::server::wrapper::Json<RouteReport>, McpError> {
        // `Json<RouteReport>` rather than a hand-built `CallToolResult`: that
        // is what makes rmcp derive the tool's `output_schema` from the shared
        // report type, which is the agreement `actions::ACTIONS` asserts
        // against `bitrouter route --json`.
        self.routing()?
            .route(input)
            .await
            .map(rmcp::handler::server::wrapper::Json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

// ── the skills profile's slice (guarded on `self.skills`) ──
#[tool_router(router = skills_router)]
impl BitrouterMcp {
    #[tool(
        description = "List the skills installed on this machine, optionally narrowed by a \
                       `query` matched against name and description. Every skill found on disk \
                       is returned, including ones that cannot be used: those carry \
                       `valid: false` and a `problem` saying why (bad frontmatter, a directory \
                       name that does not match `name`, an out-of-bounds name or description). \
                       Only `valid` skills are servable — `skills/list` publishes those alone — \
                       so a skill you can see here but not load is a bug in the skill, not a \
                       missing one. `dir` is the skill's directory; `skill_md` is its SKILL.md.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn skills_search(
        &self,
        Parameters(args): Parameters<SkillsSearchArgs>,
    ) -> Result<rmcp::handler::server::wrapper::Json<SkillsReport>, McpError> {
        // `Json<SkillsReport>` rather than a hand-built `CallToolResult`: that
        // is what makes rmcp derive the tool's `output_schema` from the shared
        // report type, which is the agreement `actions::ACTIONS` asserts
        // against `bitrouter skills list --json`.
        self.skills()?
            .list()
            .await
            .map(|report| report.matching(args.query.as_deref()))
            .map(rmcp::handler::server::wrapper::Json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "Fetch one skill's frontmatter metadata and SKILL.md body so you can hand \
                       it to a subagent.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn skills_get(
        &self,
        Parameters(args): Parameters<SkillsGetArgs>,
    ) -> Result<rmcp::handler::server::wrapper::Json<SkillDetail>, McpError> {
        self.skills()?
            .get(&args.name)
            .await
            .map(rmcp::handler::server::wrapper::Json)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

/// The typed accessor for a capability port: the port, or a wired-capability
/// error (unreachable in practice — each capability's router is merged only
/// when its port is `Some`, so a routed tool call implies a wired port).
macro_rules! port_accessor {
    ($field:ident, $port:ty, $what:literal) => {
        fn $field(&self) -> Result<&Arc<$port>, McpError> {
            self.caps
                .$field
                .as_ref()
                .ok_or_else(|| McpError::internal_error(concat!($what, " not wired"), None))
        }
    };
}

impl BitrouterMcp {
    /// Start assembling a handler. `build()` merges only the routers whose
    /// capability was wired.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Every tool this handler registers, with its schemas and annotations.
    ///
    /// The same list `tools/list` answers with. Public so the embedding binary
    /// can assert the actions table against the real tool surface — that guard
    /// has to name both `ACTIONS` and the CLI's clap definition, so it can only
    /// live app-side.
    pub fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    port_accessor!(status_query, dyn StatusQuery, "status capability");
    port_accessor!(models_query, dyn ModelsQuery, "list_models capability");
    port_accessor!(routing, dyn RouteQuery, "routing capability");
    port_accessor!(skills, dyn SkillsQuery, "skills capability");
    port_accessor!(skill_catalog, dyn SkillCatalog, "skills catalog");

    /// Server instructions: [`BASE_INSTRUCTIONS`], then a fragment per wired
    /// capability, walking [`CAPABILITIES`] — the same table `build()` merges
    /// routers from, so a client is told about exactly the tools it can call
    /// (the public HTTP profile gets the introspection pair; the stdio router
    /// profile adds the `route_preview` guidance, and the skills origin server
    /// its own).
    fn instructions(&self) -> String {
        let mut s = BASE_INSTRUCTIONS.to_string();
        for spec in CAPABILITIES {
            if (spec.wired)(&self.caps) {
                s.push(' ');
                s.push_str(&(spec.instructions)(&self.caps));
            }
        }
        s
    }
}

/// Assembles a [`BitrouterMcp`] from the capabilities the caller wires. Each
/// wired capability contributes its named router; the composed router is the
/// server's whole tool surface, so unwired tools are never registered.
#[derive(Default)]
pub struct Builder {
    caps: Caps,
}

impl Builder {
    /// Wire the `status` action's port (the `status` tool).
    pub fn status(mut self, status: Arc<dyn StatusQuery>) -> Self {
        self.caps.status_query = Some(status);
        self
    }

    /// Wire the `list_models` action's port (the `list_models` tool).
    pub fn models(mut self, models: Arc<dyn ModelsQuery>) -> Self {
        self.caps.models_query = Some(models);
        self
    }

    /// Wire the routing-introspection capability (the `route_preview` tool).
    pub fn routing(mut self, routing: Arc<dyn RouteQuery>) -> Self {
        self.caps.routing = Some(routing);
        self
    }

    /// Wire the skills-introspection capability (`skills_search`/`skills_get`).
    pub fn skills(mut self, skills: Arc<dyn SkillsQuery>) -> Self {
        self.caps.skills = Some(skills);
        self
    }

    /// Wire the SEP-2640 skills surface: `skills/list`, `skills/get`, and
    /// `resources/list` + `resources/read` over skill files.
    ///
    /// Independent of [`Self::skills`]. That one is a pair of *tools*, which
    /// every MCP client can call today; this one is the *method* form SEP-aware
    /// hosts will consume. Wiring both is the intended configuration.
    pub fn skill_catalog(mut self, catalog: Arc<dyn SkillCatalog>) -> Self {
        self.caps.skill_catalog = Some(catalog);
        self
    }

    /// Compose the handler, merging each wired capability's router from
    /// `CAPABILITIES`.
    pub fn build(self) -> BitrouterMcp {
        let mut tool_router = ToolRouter::new();
        for spec in CAPABILITIES {
            if (spec.wired)(&self.caps) {
                tool_router += (spec.router)();
            }
        }
        BitrouterMcp {
            caps: self.caps,
            tool_router,
        }
    }
}

/// How long a client may treat our `tools/list` as fresh (SEP-2549).
///
/// The router is frozen at [`Builder::build`] from the wired capabilities and
/// never varies per caller or over a connection's life, so this is bounded only
/// by how quickly a client should notice a *restarted* server with a different
/// profile. Five minutes keeps a re-dial cheap without pinning a stale list.
const TOOLS_LIST_TTL_MS: u64 = 5 * 60 * 1000;

/// Map a [`ToolError`] from the skills surface onto the JSON-RPC code SEP-2640
/// mandates.
///
/// `-32602` (Invalid params) is "the same code `resources/read` uses for
/// unknown resources" per the SEP, and covers every failure this surface has:
/// the URI does not name a skill, or does not name a file of one.
fn skills_error(e: ToolError) -> McpError {
    McpError::new(rmcp::model::ErrorCode::INVALID_PARAMS, e.0, None)
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BitrouterMcp {
    fn get_info(&self) -> ServerInfo {
        // Resources and the skills extension are declared only when a catalog
        // is wired. Unlike the gateway — which cannot know its upstreams'
        // capabilities when it answers `initialize` (see the optimistic
        // declaration in `bitrouter-sdk`'s `mcp_invoke`) — the origin server
        // knows its own catalog at build time, so it can declare honestly.
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        if self.caps.skill_catalog.is_some() {
            capabilities.resources = Some(rmcp::model::ResourcesCapability::default());
            capabilities.extensions = Some(
                [(
                    SKILLS_EXTENSION_ID.to_string(),
                    // `directoryRead` is deliberately absent (it defaults to
                    // `false`): a skill entry's `resources` already enumerates
                    // every file of the skill, so a host holding the entry can
                    // filter it by prefix instead of walking directories.
                    // Declaring the optional method would oblige us to serve it
                    // for a navigation need that is already met.
                    serde_json::Map::new(),
                )]
                .into_iter()
                .collect(),
            );
        }
        ServerInfo::new(capabilities)
            // Without this the identity defaults to `Implementation::from_build_env()`,
            // which resolves against *rmcp's* build environment — every client
            // asking who we are (via `server/discover`, `initialize`, or the
            // SEP-2575 `io.modelcontextprotocol/serverInfo` result metadata)
            // would be told it is talking to "rmcp". The client half of the
            // gateway has always identified itself; this is the matching half.
            .with_server_info(rmcp::model::Implementation::new(
                "bitrouter",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions())
    }

    /// Same as the `#[tool_handler]`-generated implementation (the macro skips
    /// generating one when the impl already defines it), plus SEP-2549 cache
    /// hints for peers that negotiated `2026-07-28`.
    ///
    /// `cacheScope: public` is accurate here and worth stating explicitly: the
    /// tool set is fixed at [`Builder::build`] from the wired capabilities, so
    /// every caller of a given server instance sees the same list. It would
    /// *not* be accurate if tool visibility ever became caller-dependent — that
    /// change must revisit this scope.
    ///
    /// The hints are version-gated because rmcp only strips `resultType` for
    /// legacy peers (`ServerResult::strip_result_type_for_legacy_peer`), not
    /// `ttlMs`/`cacheScope`; emitting them unconditionally would send
    /// draft-only fields to a `2025-11-25` client.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let draft = context
            .protocol_version()
            .is_some_and(|v| v.as_str() >= rmcp::model::ProtocolVersion::V_2026_07_28.as_str());
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms: draft.then_some(TOOLS_LIST_TTL_MS),
            cache_scope: draft.then_some(rmcp::model::CacheScope::Public),
        })
    }

    /// SEP-2640's `skills/list` and `skills/get`. Both are mandatory for a
    /// server declaring the extension, so both are answered whenever a catalog
    /// is wired — and neither exists when one is not.
    async fn on_custom_request(
        &self,
        request: rmcp::model::CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CustomResult, McpError> {
        let method = request.method.as_str();
        if !matches!(method, SKILLS_LIST_METHOD | SKILLS_GET_METHOD) {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method.clone(),
                None,
            ));
        }
        // No catalog wired means this server does not implement the extension
        // at all, which is "method not found" rather than a bad request.
        let catalog = self.caps.skill_catalog.as_ref().ok_or_else(|| {
            McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method.clone(),
                None,
            )
        })?;
        let value = match method {
            SKILLS_LIST_METHOD => {
                let result = catalog.list().await.map_err(skills_error)?;
                serde_json::to_value(result)
            }
            // `skills/get` — anything else was rejected above.
            _ => {
                let params: GetSkillParams = request
                    .params_as()
                    .map_err(|e| {
                        McpError::new(
                            rmcp::model::ErrorCode::INVALID_PARAMS,
                            format!("skills/get params: {e}"),
                            None,
                        )
                    })?
                    .ok_or_else(|| {
                        McpError::new(
                            rmcp::model::ErrorCode::INVALID_PARAMS,
                            "skills/get requires params.uri".to_string(),
                            None,
                        )
                    })?;
                let result = catalog.get(&params.uri).await.map_err(skills_error)?;
                serde_json::to_value(result)
            }
        }
        .map_err(|e| {
            McpError::new(
                rmcp::model::ErrorCode::INTERNAL_ERROR,
                format!("{method} serialise: {e}"),
                None,
            )
        })?;
        Ok(rmcp::model::CustomResult::new(value))
    }

    /// Every file of every skill, so a generic MCP client can discover the
    /// skill namespace without knowing about SEP-2640 at all.
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, McpError> {
        let catalog = self.skill_catalog()?;
        let listed = catalog.list().await.map_err(skills_error)?;
        let resources = listed
            .skills
            .iter()
            // A dynamic skill enumerates nothing, so it contributes no
            // resources here; its files are reachable only by direct read.
            .filter_map(|skill| skill.resources.entries())
            .flatten()
            .map(|resource| {
                let name = resource
                    .uri
                    .rsplit('/')
                    .next()
                    .unwrap_or(resource.uri.as_str());
                rmcp::model::Resource::new(resource.uri.clone(), name.to_string())
            })
            .collect();
        Ok(rmcp::model::ListResourcesResult::with_all_items(resources))
    }

    /// Read one skill file. Resolution is by lookup against the catalog's own
    /// enumeration, so `resources/read` and a skill entry's `resources` cannot
    /// disagree — which SEP-2640 requires, since a host must treat a read of an
    /// unlisted file as a verification failure.
    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, McpError> {
        let catalog = self.skill_catalog()?;
        let file = catalog.read(&request.uri).await.map_err(skills_error)?;
        let contents = match file.body {
            SkillFileBody::Text(text) => rmcp::model::ResourceContents::TextResourceContents {
                uri: file.uri,
                mime_type: file.mime_type,
                text,
                meta: None,
            },
            SkillFileBody::Blob(blob) => rmcp::model::ResourceContents::BlobResourceContents {
                uri: file.uri,
                mime_type: file.mime_type,
                blob,
                meta: None,
            },
        };
        Ok(rmcp::model::ReadResourceResponse::Complete(
            rmcp::model::ReadResourceResult::new(vec![contents]),
        ))
    }
}

use crate::backend::cloud::{CloudAuth, CloudBackend};
use crate::backend::local::LocalBackend;

/// Whether an `Authorization` header value carries a Bearer token (scheme
/// matched case-insensitively per RFC 7235).
fn has_bearer(value: Option<&str>) -> bool {
    value.and_then(parse_bearer).is_some()
}

/// Refuse a non-loopback HTTP bind when the server runs without auth (the
/// local backend). Binding the unauthenticated local backend to a public
/// address would expose the BYOK daemon — running on the user's own provider
/// keys — to the whole network.
///
/// Loopback means "this machine", not "this user": on a multi-user host any
/// local user can reach the bound port and spend through the daemon's keys.
/// That is the same trust boundary as the daemon's own `127.0.0.1:4356`
/// listener, so it is accepted here deliberately — "loopback" is being relied
/// on as *trusted*, not as *mine*.
pub(crate) fn ensure_loopback_bind(bind: &str) -> anyhow::Result<()> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = bind
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("invalid --bind '{bind}': {e}"))?
        .collect();
    match addrs.iter().find(|a| !a.ip().is_loopback()) {
        None if addrs.is_empty() => {
            anyhow::bail!("invalid --bind '{bind}': resolved to no socket addresses")
        }
        None => Ok(()),
        Some(addr) => anyhow::bail!(
            "refusing to bind the unauthenticated local backend to non-loopback address \
             {addr}: this would expose your provider keys to the network. Bind a loopback \
             address (e.g. 127.0.0.1) or use --backend cloud (which requires Authorization)."
        ),
    }
}

/// Reject requests without a `Bearer` Authorization header (presence only;
/// the cloud validates the token's validity).
async fn require_bearer(
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let present = has_bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok()),
    );
    if present {
        next.run(request).await
    } else {
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Build the `/mcp-control` axum router for `backend`, optionally gated by the
/// pre-auth bearer middleware. HTTP is the public profile: whatever the backend
/// itself can answer about its own deployment, and nothing else.
///
/// That coupling is a crate-level invariant, deliberately: the remaining tools
/// are semantically bound to one machine (`route_preview` resolves against the
/// serving host's own config and control socket; `skills_search`/`skills_get`
/// read its installed-skills root), so no multi-tenant HTTP profile may carry
/// them. Should a remote client ever need more of that *read-only*
/// introspection surface without a stdio pipe, this is the single function to
/// change — take a handler factory, keep the profile strictly loopback and
/// incompatible with the cloud backend, and prefer modeling that read-only data
/// as MCP resources over widening the tool surface.
///
/// The invariant is load-bearing for a second reason. Under SEP-2567 a peer
/// negotiating `2026-07-28` is **always served statelessly**, regardless of
/// `StreamableHttpServerConfig::legacy_session_mode` — each request gets a
/// fresh handler from the factory below, so nothing connection-scoped survives
/// between requests. Adding a stateful tool to this HTTP profile would
/// therefore break under draft-version clients even though it works today
/// against `2025-11-25`.
fn build_http_router(
    backend: Arc<dyn Backend>,
    require_auth: bool,
    config: rmcp::transport::streamable_http_server::StreamableHttpServerConfig,
) -> axum::Router {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let service = StreamableHttpService::new(
        move || Ok(http_profile(backend.clone())),
        LocalSessionManager::default().into(),
        config,
    );
    let mut router = axum::Router::new().nest_service("/mcp-control", service);
    if require_auth {
        router = router.layer(axum::middleware::from_fn(require_bearer));
    }
    router
}

/// The whole HTTP tool surface, in one function so a test can assert it.
///
/// `list_models` and `status`, where the backend itself can answer them (its
/// own `GET /v1/models`; the cloud account's remaining credit, read with the
/// caller's own bearer). Nothing else: the host-bound tools stay off this
/// transport, per the invariant above, and there is no inference tool on any
/// transport.
/// [`http_profile`] for the crate's own profile tests, which live in `lib.rs`
/// (they compare it against [`crate::stdio_profile`], and only one of the two
/// can be module-private).
#[cfg(test)]
pub(crate) fn http_profile_for_test(backend: Arc<dyn Backend>) -> BitrouterMcp {
    http_profile(backend)
}

fn http_profile(backend: Arc<dyn Backend>) -> BitrouterMcp {
    let mut builder = BitrouterMcp::builder();
    if let Some(models) = backend.clone().models_port() {
        builder = builder.models(models);
    }
    if let Some(status) = backend.status_port() {
        builder = builder.status(status);
    }
    builder.build()
}

/// Serve streamable HTTP on an already-bound listener until the task is dropped.
/// Exposed for integration tests of real multi-tenant forwarding.
#[doc(hidden)]
pub async fn serve_http_on(
    backend: Arc<dyn Backend>,
    listener: tokio::net::TcpListener,
    require_auth: bool,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    axum::serve(
        listener,
        build_http_router(backend, require_auth, StreamableHttpServerConfig::default()),
    )
    .await?;
    Ok(())
}

/// Transport wrapper that replays one message already read during stdio
/// lifecycle preflight, then delegates every operation to rmcp's transport.
struct PrefetchedTransport<T> {
    first: Option<ClientJsonRpcMessage>,
    inner: T,
}

impl<T> Transport<RoleServer> for PrefetchedTransport<T>
where
    T: Transport<RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        match self.first.take() {
            Some(first) => Some(first),
            None => self.inner.receive().await,
        }
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

/// Identify an inline-lifecycle opener whose `_meta` announces that lifecycle
/// but does not contain its required client context.
fn malformed_inline_opener(
    message: &ClientJsonRpcMessage,
) -> Option<(RequestId, Vec<&'static str>)> {
    let ClientJsonRpcMessage::Request(request) = message else {
        return None;
    };
    // These requests belong to stable startup regardless of extension keys in
    // `_meta`. Replaying them unchanged lets rmcp answer a pre-init ping or
    // negotiate the legacy initialize instead of guessing an inline lifecycle.
    if matches!(
        &request.request,
        ClientRequest::InitializeRequest(_) | ClientRequest::PingRequest(_)
    ) {
        return None;
    }
    let meta = request.request.get_meta();
    let inline = matches!(&request.request, ClientRequest::DiscoverRequest(_))
        || rmcp::model::RequestMetaObject::DRAFT_REQUIRED_KEYS
            .iter()
            .any(|key| meta.contains_key(*key));
    if !inline {
        return None;
    }
    let missing = meta.missing_required_keys(&ProtocolVersion::V_2026_07_28);
    (!missing.is_empty()).then(|| (request.id.clone(), missing))
}

/// Serve `server` over stdio until the client disconnects.
pub async fn serve_stdio(server: BitrouterMcp) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    use rmcp::transport::async_rw::AsyncRwTransport;
    let mut transport =
        AsyncRwTransport::<RoleServer, _, _>::new_server(tokio::io::stdin(), tokio::io::stdout());
    let first = loop {
        let Some(message) = transport.receive().await else {
            return Ok(());
        };
        let Some((id, missing)) = malformed_inline_opener(&message) else {
            break message;
        };
        transport
            .send(ServerJsonRpcMessage::error(
                ErrorData::invalid_params(
                    format!(
                        "request _meta is missing or has malformed required fields: {}",
                        missing.join(", ")
                    ),
                    None,
                ),
                Some(id),
            ))
            .await?;
    };
    // rmcp 3.1's normal startup accepts both lifecycle openers. A legacy
    // `initialize` negotiates and stores the agreed fallback version; a
    // self-contained modern request enables required per-request metadata for
    // the rest of the connection. Keeping those transitions inside rmcp is
    // essential — direct mode intentionally bypasses both. The preflight above
    // preserves the JSON-RPC `-32602` response for a malformed modern opener;
    // rmcp's startup otherwise closes before it can dispatch that request.
    let service = server
        .serve(PrefetchedTransport {
            first: Some(first),
            inner: transport,
        })
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Serve streamable HTTP at `/mcp-control` on `bind` until Ctrl-C.
///
/// When `require_auth` is `true`, requests without a `Bearer` Authorization
/// header are rejected with `401 Unauthorized` before reaching the MCP handler.
pub async fn serve_http(
    backend: Arc<dyn Backend>,
    bind: &str,
    require_auth: bool,
) -> anyhow::Result<()> {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    let ct = tokio_util::sync::CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default();
    config.cancellation_token = ct.child_token();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let shutdown = {
        let ct = ct.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        }
    };
    axum::serve(listener, build_http_router(backend, require_auth, config))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

/// Build the backend. The cloud auth mode depends on transport:
/// stdio→cloud uses the configured token (Static); http→cloud is multi-tenant
/// (PerCaller — each request must carry its own bearer).
pub fn build_backend(
    kind: crate::BackendKind,
    transport: crate::Transport,
    local_url: &str,
    cloud_url: &str,
    cloud_token: Option<&str>,
) -> anyhow::Result<Arc<dyn Backend>> {
    match kind {
        crate::BackendKind::Local => Ok(Arc::new(LocalBackend::new(local_url))),
        crate::BackendKind::Cloud => {
            let auth = match transport {
                crate::Transport::Http => CloudAuth::PerCaller,
                crate::Transport::Stdio => {
                    let token = cloud_token.ok_or_else(|| {
                        anyhow::anyhow!(
                            "stdio cloud backend needs a token (--token or BITROUTER_TOKEN)"
                        )
                    })?;
                    CloudAuth::Static(token.to_owned())
                }
            };
            Ok(Arc::new(CloudBackend::new(cloud_url, auth)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::models::ModelsSource;
    use crate::actions::status::{Spend, SpendLimit};
    use crate::backend::CallerAuth;
    use bitrouter_sdk::language_model::routing::ModelInfo;

    #[test]
    fn require_bearer_predicate() {
        assert!(has_bearer(Some("Bearer abc")));
        // RFC 7235 schemes are case-insensitive.
        assert!(has_bearer(Some("bearer abc")));
        assert!(has_bearer(Some("BEARER abc")));
        assert!(!has_bearer(Some("Basic abc")));
        assert!(!has_bearer(Some("Bearer")));
        // A whitespace-only token is not a credential.
        assert!(!has_bearer(Some("Bearer ")));
        assert!(!has_bearer(None));
    }

    #[test]
    fn parse_bearer_is_case_insensitive_and_trims() {
        assert_eq!(parse_bearer("Bearer xyz"), Some("xyz"));
        assert_eq!(parse_bearer("bearer  xyz"), Some("xyz"));
        assert_eq!(parse_bearer("Basic xyz"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        // Whitespace-only tokens are rejected, not read as `Some("")`.
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer("Bearer   "), None);
    }

    #[test]
    fn ensure_loopback_bind_allows_loopback_rejects_public() {
        assert!(ensure_loopback_bind("127.0.0.1:4357").is_ok());
        assert!(ensure_loopback_bind("[::1]:4357").is_ok());
        assert!(ensure_loopback_bind("0.0.0.0:4357").is_err());
        assert!(ensure_loopback_bind("192.168.1.10:4357").is_err());
        assert!(ensure_loopback_bind("not-a-bind").is_err());
    }

    struct StubBackend;
    #[async_trait::async_trait]
    impl Backend for StubBackend {
        /// Answers `status` itself, standing in for the cloud backend — which
        /// is the only shape in which the HTTP profile carries the tool.
        fn status_port(self: Arc<Self>) -> Option<Arc<dyn StatusQuery>> {
            Some(self)
        }
        /// …and its own catalog, as both real backends do.
        fn models_port(self: Arc<Self>) -> Option<Arc<dyn ModelsQuery>> {
            Some(self)
        }
    }

    #[async_trait::async_trait]
    impl ModelsQuery for StubBackend {
        async fn list_models(&self, _: &CallerAuth) -> Result<ModelsReport, ToolError> {
            Ok(ModelsReport::live(vec![ModelInfo {
                id: "openai/gpt-4o".into(),
                providers: vec!["openai".into(), "azure".into()],
            }]))
        }
    }

    /// A backend that cannot answer `status`, like `LocalBackend`.
    struct StatuslessBackend;
    #[async_trait::async_trait]
    impl Backend for StatuslessBackend {
        fn status_port(self: Arc<Self>) -> Option<Arc<dyn StatusQuery>> {
            None
        }
        fn models_port(self: Arc<Self>) -> Option<Arc<dyn ModelsQuery>> {
            Some(self)
        }
    }

    #[async_trait::async_trait]
    impl ModelsQuery for StatuslessBackend {
        async fn list_models(&self, _: &CallerAuth) -> Result<ModelsReport, ToolError> {
            Ok(ModelsReport::live(Vec::new()))
        }
    }

    #[async_trait::async_trait]
    impl StatusQuery for StubBackend {
        async fn status(&self, _: &CallerAuth) -> Result<StatusReport, ToolError> {
            Ok(StatusReport::metered(Spend {
                currency: "USD".into(),
                spent: None,
                limit: Some(SpendLimit {
                    balance_micro_usd: 1,
                    pending_micro_usd: 0,
                    remaining_micro_usd: 1,
                }),
            }))
        }
    }

    struct StubRouting;
    #[async_trait::async_trait]
    impl RouteQuery for StubRouting {
        async fn route(&self, input: RouteInput) -> Result<RouteReport, ToolError> {
            Ok(RouteReport {
                requested_model: input.model.clone(),
                effective_model: input.model,
                effective_effort: None,
                resolved_via: crate::actions::route::ResolvedVia::Config,
                policy_decision: None,
                provider_chain: Vec::new(),
                estimated_cost: None,
            })
        }
    }

    struct StubSkills;
    #[async_trait::async_trait]
    impl SkillsQuery for StubSkills {
        async fn list(&self) -> Result<SkillsReport, ToolError> {
            Ok(SkillsReport { skills: vec![] })
        }
        async fn get(&self, name: &str) -> Result<SkillDetail, ToolError> {
            Err(ToolError::new(format!("no installed skill named '{name}'")))
        }
    }

    /// A one-skill catalog: `skill://demo/SKILL.md` plus one supporting file.
    struct StubCatalog;

    impl StubCatalog {
        fn entry() -> bitrouter_sdk::mcp::skills::SkillEntry {
            let frontmatter = match serde_json::json!({
                "name": "demo",
                "description": "A demo skill",
            }) {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            bitrouter_sdk::mcp::skills::SkillEntry {
                uri: "skill://demo/SKILL.md".into(),
                frontmatter,
                resources: bitrouter_sdk::mcp::skills::SkillResources::Enumerated(vec![
                    bitrouter_sdk::mcp::skills::SkillResource {
                        uri: "skill://demo/SKILL.md".into(),
                        digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                        size: 22,
                    },
                    bitrouter_sdk::mcp::skills::SkillResource {
                        uri: "skill://demo/refs/GUIDE.md".into(),
                        digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                        size: 7,
                    },
                ]),
                extra: serde_json::Map::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl SkillCatalog for StubCatalog {
        async fn list(&self) -> Result<bitrouter_sdk::mcp::skills::ListSkillsResult, ToolError> {
            Ok(bitrouter_sdk::mcp::skills::ListSkillsResult {
                skills: vec![Self::entry()],
            })
        }
        async fn get(
            &self,
            uri: &str,
        ) -> Result<bitrouter_sdk::mcp::skills::GetSkillResult, ToolError> {
            if uri == "skill://demo/SKILL.md" {
                Ok(bitrouter_sdk::mcp::skills::GetSkillResult {
                    skill: Self::entry(),
                })
            } else {
                Err(ToolError::new(format!("no installed skill at '{uri}'")))
            }
        }
        async fn read(
            &self,
            uri: &str,
        ) -> Result<crate::capabilities::skill_catalog::SkillFile, ToolError> {
            if uri == "skill://demo/SKILL.md" {
                Ok(crate::capabilities::skill_catalog::SkillFile {
                    uri: uri.to_string(),
                    mime_type: Some("text/markdown".into()),
                    body: SkillFileBody::Text("# Demo".into()),
                })
            } else {
                Err(ToolError::new(format!(
                    "'{uri}' is not a file of any installed skill"
                )))
            }
        }
    }

    fn tool_names(server: &BitrouterMcp) -> Vec<String> {
        let mut names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    /// The origin server knows its own catalog at build time, so unlike the
    /// gateway it declares the extension only when it can actually serve it.
    #[test]
    fn skills_extension_is_declared_only_when_a_catalog_is_wired() {
        let without = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .build();
        let caps = without.get_info().capabilities;
        assert!(caps.extensions.is_none(), "no catalog, no extension");
        assert!(caps.resources.is_none(), "no catalog, no resources");

        let with = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .skill_catalog(Arc::new(StubCatalog))
            .build();
        let caps = with.get_info().capabilities;
        let extensions = caps.extensions.expect("extension declared");
        assert!(extensions.contains_key("io.modelcontextprotocol/skills"));
        // Skills ride on `resources/read`, so the resources capability must
        // come with them.
        assert!(caps.resources.is_some(), "resources declared alongside");
        // `directoryRead` is not claimed: an entry's `resources` already
        // enumerates every file, so the optional method has nothing to add.
        let settings = &extensions["io.modelcontextprotocol/skills"];
        assert!(
            settings.get("directoryRead").is_none(),
            "directoryRead defaults to false and is not claimed: {settings:?}"
        );
    }

    /// Wiring the SEP surface must not disturb the tool surface — the two are
    /// independent, and the tool form is what clients can use today.
    #[test]
    fn skill_catalog_adds_no_tools_and_leaves_skills_tools_alone() {
        let server = BitrouterMcp::builder()
            .skills(Arc::new(StubSkills))
            .skill_catalog(Arc::new(StubCatalog))
            .build();
        let names = tool_names(&server);
        assert!(names.contains(&"skills_search".to_string()));
        assert!(names.contains(&"skills_get".to_string()));
        assert!(
            !names.iter().any(|n| n.contains("catalog")),
            "the catalog is served as methods, not tools: {names:?}"
        );
    }

    #[test]
    fn public_profile_advertises_exactly_the_wired_tools() {
        // Nothing wired, nothing registered — every tool is a capability's, and
        // there is no always-present tool left to stand in for a profile.
        let bare = BitrouterMcp::builder().build();
        assert!(tool_names(&bare).is_empty());
        let with_models = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .build();
        assert_eq!(tool_names(&with_models), ["list_models"]);
        let with_both = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .status(Arc::new(StubBackend))
            .build();
        assert_eq!(tool_names(&with_both), ["list_models", "status"]);
    }

    /// The drift phase 2 removes, asserted at the tool boundary: a model served
    /// by two providers must reach the client with both. The tool returns the
    /// shared `ModelsReport`, so this is the same value `bitrouter models`
    /// emits.
    #[tokio::test]
    async fn list_models_carries_the_whole_fallback_chain() {
        let report = ModelsQuery::list_models(&StubBackend, &CallerAuth::default())
            .await
            .expect("list_models");
        assert_eq!(
            report.models.first().map(|m| m.providers.as_slice()),
            Some(["openai".to_string(), "azure".to_string()].as_slice())
        );
        assert_eq!(report.resolved_via, ModelsSource::Live);
        // …and the filter both surfaces share narrows the same list.
        assert!(
            report
                .clone()
                .filtered(Some("azure"))
                .models
                .iter()
                .any(|m| m.id == "openai/gpt-4o")
        );
        assert!(report.filtered(Some("nobody")).models.is_empty());
    }

    /// Invariant: the HTTP profile is at most `list_models` and `status` —
    /// only what the backend itself can answer for its own deployment. It
    /// must never carry the host-bound tools — `route_preview` resolves against
    /// the serving machine's own config and control socket, and the skills
    /// tools read its installed-skills root, so neither means anything to a
    /// multi-tenant caller. Nor may it regrow an inference tool: this transport
    /// is not what completions go over.
    #[test]
    fn http_profile_never_carries_host_bound_tools() {
        assert_eq!(
            tool_names(&http_profile(Arc::new(StubBackend))),
            ["list_models", "status"]
        );
        // A backend with no status of its own gets no `status` tool rather than
        // a fabricated one: the local daemon's liveness is a control-socket
        // question the HTTP profile cannot answer. Its catalog it *can* answer,
        // so `list_models` stays.
        assert_eq!(
            tool_names(&http_profile(Arc::new(StatuslessBackend))),
            ["list_models"]
        );
    }

    /// A stopped daemon is a *result*, not a tool error: an agent polling for
    /// health has to be able to tell "down" from "broken".
    #[tokio::test]
    async fn stopped_daemon_is_a_result_not_a_tool_error() {
        struct StoppedStatus;
        #[async_trait::async_trait]
        impl StatusQuery for StoppedStatus {
            async fn status(&self, _: &CallerAuth) -> Result<StatusReport, ToolError> {
                Ok(StatusReport::stopped("/x.sock".into(), None))
            }
        }
        let report = StoppedStatus.status(&CallerAuth::default()).await;
        let report = report.expect("a stopped daemon must not be an error");
        assert!(!report.running);
        assert_eq!(
            serde_json::to_value(&report).expect("serialize"),
            serde_json::json!({
                "running": false, "providers": [], "socket": "/x.sock"
            })
        );
    }

    /// The `status` tool advertises the shared report's schema — the whole
    /// point of returning `Json<StatusReport>` rather than a `CallToolResult`.
    #[test]
    fn status_tool_advertises_the_shared_report_schema() {
        let server = BitrouterMcp::builder()
            .status(Arc::new(StubBackend))
            .build();
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "status")
            .expect("status tool registered");
        let schema = crate::actions::ACTIONS
            .iter()
            .find(|a| a.id == "status")
            .and_then(|row| row.output_schema)
            .expect("the status row carries a shared schema");
        assert_eq!(
            tool.output_schema.as_deref(),
            Some(&schema()),
            "the `status` tool must advertise its row's schema"
        );
    }

    #[test]
    fn public_profile_never_exposes_the_host_bound_tools() {
        // The safety boundary: a public client must not even see the tools
        // that resolve against the serving machine.
        let server = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .status(Arc::new(StubBackend))
            .build();
        let names = tool_names(&server);
        for hidden in ["route_preview", "skills_search", "skills_get"] {
            assert!(
                !names.contains(&hidden.to_string()),
                "public profile must not advertise `{hidden}`: {names:?}"
            );
        }
    }

    #[test]
    fn routing_capability_adds_route_preview() {
        let public = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .build();
        let with_routing = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .routing(Arc::new(StubRouting))
            .build();
        assert_eq!(
            tool_names(&public).len() + 1,
            tool_names(&with_routing).len()
        );
        assert!(tool_names(&with_routing).contains(&"route_preview".to_string()));
    }

    #[test]
    fn router_profile_is_the_introspection_pair_plus_route_preview() {
        // What `bitrouter mcp serve --transport stdio --backend local` wires
        // (before the skills ports, which every stdio profile adds on top).
        let server = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .status(Arc::new(StubBackend))
            .routing(Arc::new(StubRouting))
            .build();
        assert_eq!(
            tool_names(&server),
            ["list_models", "route_preview", "status"]
        );
    }

    #[test]
    fn annotations_classify_the_tool_surface() {
        // The full surface: every tool declares explicit annotations, and the
        // whole of it is read-only introspection — nothing destructive, and
        // nothing open-world, because no tool here reaches an upstream LLM.
        let server = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .status(Arc::new(StubBackend))
            .routing(Arc::new(StubRouting))
            .skills(Arc::new(StubSkills))
            .build();
        let tools = server.tool_router.list_all();
        assert_eq!(tools.len(), 5);
        for tool in &tools {
            assert!(
                tool.annotations.is_some(),
                "`{}` declares no annotations",
                tool.name
            );
        }
        let with_hint = |pick: fn(&rmcp::model::ToolAnnotations) -> Option<bool>| {
            let mut names: Vec<String> = tools
                .iter()
                .filter(|t| t.annotations.as_ref().and_then(pick) == Some(true))
                .map(|t| t.name.to_string())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            with_hint(|a| a.read_only_hint),
            [
                "list_models",
                "route_preview",
                "skills_get",
                "skills_search",
                "status",
            ],
            "every tool on this surface is read-only introspection"
        );
        assert!(
            with_hint(|a| a.destructive_hint).is_empty(),
            "nothing on the router/skills surface is destructive"
        );
        assert!(
            with_hint(|a| a.open_world_hint).is_empty(),
            "open-world would mean reaching an upstream LLM, which no tool here does"
        );
    }

    #[test]
    fn instructions_reflect_the_wired_capabilities() {
        // The public profile advertises only what it wired — no guidance for
        // tools an HTTP client couldn't call.
        let public = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .build()
            .instructions();
        assert!(public.starts_with("BitRouter origin MCP server"));
        assert!(public.contains("list_models"));
        for absent in ["route_preview", "skills_search"] {
            assert!(
                !public.contains(absent),
                "public omits `{absent}`: {public}"
            );
        }

        let wired = BitrouterMcp::builder()
            .models(Arc::new(StubBackend))
            .routing(Arc::new(StubRouting))
            .skills(Arc::new(StubSkills))
            .build()
            .instructions();
        for present in ["list_models", "route_preview", "skills_search"] {
            assert!(
                wired.contains(present),
                "wired mentions `{present}`: {wired}"
            );
        }
    }

    #[test]
    fn caller_from_extensions_reads_bearer() {
        use rmcp::model::Extensions;
        let mut ext = Extensions::new();
        let req = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Bearer xyz")
            .body(())
            .expect("req");
        let (parts, _) = req.into_parts();
        ext.insert(parts);
        assert_eq!(caller_from_extensions(&ext).bearer.as_deref(), Some("xyz"));

        let empty = Extensions::new();
        assert_eq!(caller_from_extensions(&empty).bearer, None);

        // non-Bearer scheme → None
        let mut ext2 = Extensions::new();
        let req2 = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Basic abc")
            .body(())
            .expect("req2");
        let (parts2, _) = req2.into_parts();
        ext2.insert(parts2);
        assert_eq!(caller_from_extensions(&ext2).bearer, None);
    }
}

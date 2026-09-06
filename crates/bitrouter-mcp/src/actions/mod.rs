//! Actions — one typed question with one typed answer, expressible on more
//! than one surface.
//!
//! Each action owns exactly one report type, deriving `Serialize` +
//! `Deserialize` + `JsonSchema`. The MCP tool returns it as `Json<Report>`, so
//! rmcp advertises an `output_schema` derived from that same type; the CLI
//! `emit`s it, so `bitrouter <leaf> --json` and the tool's structured content
//! are the same bytes. Human rendering stays app-side (`impl CliReport for
//! <report>` is legal there — local trait, foreign type), which keeps this
//! crate free of the CLI's `Human`/table vocabulary.
//!
//! The implementation of an action lives app-side, behind the port trait
//! declared beside its types. This crate keeps the schemas and the wiring.
//!
//! [`ACTIONS`] is the inventory the guard test walks: every MCP tool must have
//! a row, every row's `cli_leaf` must resolve in clap, and every row's tool
//! must advertise the row's schema. Only actions with more than one surface
//! belong here — the CLI's ~100 other leaves keep their own report types.

pub mod models;
pub mod route;
pub mod skills;
pub mod status;

/// One action, and the surfaces that answer it.
pub struct ActionSpec {
    /// Stable action id, e.g. `"status"`.
    pub id: &'static str,
    /// The CLI leaf that answers it, space-separated (`"skills list"`), or
    /// `None` when no CLI command does.
    pub cli_leaf: Option<&'static str>,
    /// The MCP tool that answers it, or `None` when no tool does.
    pub mcp_tool: Option<&'static str>,
    /// The shared report's JSON Schema — the thing that must not drift — or
    /// `None` while the action has not been migrated onto a shared type yet.
    ///
    /// Held as a function pointer rather than a literal so the table is
    /// load-bearing instead of documentary: the guard test compares the MCP
    /// tool's advertised `output_schema` against this, so a row cannot claim
    /// an agreement it does not have.
    ///
    /// `None` is the migration backlog, not an exemption. The row still has to
    /// exist — that is what stops a remotable action going uninventoried — but
    /// until a shared report type replaces the two hand-written shapes, there
    /// is no schema to hold the two surfaces to. It stays `Option` because §5
    /// obliges a *new* tool to have a row from the moment it is registered,
    /// which is generally before its report type is shared; the backlog is
    /// empty today, so every row below carries a real schema.
    pub output_schema: Option<fn() -> rmcp::model::JsonObject>,
}

/// Every action BitRouter answers on more than one surface.
///
/// A tool without a row here fails the guard test. Running a completion is not
/// among them and has no row: MCP is a control and introspection surface, and
/// inference goes over the daemon's HTTP API (`/v1/messages`,
/// `/v1/chat/completions`).
pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        id: "status",
        cli_leaf: Some("status"),
        mcp_tool: Some("status"),
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<status::StatusReport>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        id: "list_models",
        cli_leaf: Some("models"),
        mcp_tool: Some("list_models"),
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<models::ModelsReport>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        // The tool keeps its published name; the *action* is `route`, which is
        // what the CLI leaf is called and what the shared report answers.
        id: "route",
        cli_leaf: Some("route"),
        mcp_tool: Some("route_preview"),
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<route::RouteReport>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        id: "skills_search",
        cli_leaf: Some("skills list"),
        mcp_tool: Some("skills_search"),
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<skills::SkillsReport>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        // No CLI twin, and adding `bitrouter skills show` for the table's sake
        // would be dead surface — so this row has one surface, and the schema
        // it carries pins nothing against a second one. It is here because the
        // tool returns `Json<SkillDetail>` and therefore *does* advertise a
        // schema; a `None` beside a tool that advertises one would be the table
        // understating what it knows.
        id: "skills_get",
        cli_leaf: None,
        mcp_tool: Some("skills_get"),
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<skills::SkillDetail>()
                .as_ref()
                .clone()
        }),
    },
];

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
    /// is no schema to hold the two surfaces to.
    pub output_schema: Option<fn() -> rmcp::model::JsonObject>,
}

/// Every action BitRouter answers on more than one surface.
///
/// A tool without a row here fails the guard test. Rows whose `output_schema`
/// is `None` are inventoried but not yet unified; each is one phase of
/// `docs/ACTIONS_SPEC.md`.
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
        // No CLI twin: running a completion is what the daemon's HTTP API is
        // for. Inventoried anyway — it is remotable, so it must be listed.
        id: "complete",
        cli_leaf: None,
        mcp_tool: Some("complete"),
        output_schema: None,
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
        id: "route",
        cli_leaf: Some("route"),
        mcp_tool: Some("route_preview"),
        output_schema: None,
    },
    ActionSpec {
        id: "skills_search",
        cli_leaf: Some("skills list"),
        mcp_tool: Some("skills_search"),
        output_schema: None,
    },
    ActionSpec {
        // No CLI twin, and adding `bitrouter skills show` for the table's sake
        // would be dead surface.
        id: "skills_get",
        cli_leaf: None,
        mcp_tool: Some("skills_get"),
        output_schema: None,
    },
];

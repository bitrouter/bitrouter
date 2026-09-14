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
//!
//! A row's surfaces are a CLI leaf, an MCP tool, and — since the CLI/TUI
//! parity work — a slash command in an interactive session. The last of these
//! is what [`ActionSpec::tui_command`] names, and [`Effect`], [`Requires`] and
//! [`Reach`] are what let the guards say whether a row is offered coherently
//! across the three.
//!
//! **Not every row carries a schema.** `route_set` and `route_reset` answer on
//! one surface each — the session — so there is no second shape to hold them
//! to, and their `output_schema` is permanently `None`; the wire response they
//! produce belongs to the SDK client, not to a report. Every other row has a
//! real schema, `commands` included — it has a CLI leaf and therefore owes its
//! two surfaces an agreement.

pub mod commands;
pub mod models;
pub mod route;
pub mod skills;
pub mod status;

/// Observes or changes state.
///
/// Distinct from `bitrouter_tui::machine::Effect`, which the chat driver
/// imports. The driver never needs this one — only the guards read it — so the
/// two names never meet in a `use` list; refer to this one by path
/// (`actions::Effect`) where both are in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Safe to run at any time, including twice.
    Read,
    /// Changes state. `inverse` is the `id` of the row that undoes it.
    ///
    /// Not `Option`: a write with no inverse is not admitted to the table at
    /// all, and no CLI-only write has a row. That is what lets a session offer
    /// a write without a confirmation modal — the undo is always typable.
    Write { inverse: &'static str },
}

/// What a session must hold before the command is *offered*.
///
/// Resolved by the app once, at launch, from what the controller advertised;
/// the action itself never asks. A row whose requirement is unmet is still
/// listed, with its reason — absent, never dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requires {
    /// Offered in every session. The answer may still degrade — `status`
    /// reports `running: false`, `list_models` and `route` fall back to
    /// config — but the report says so itself (`resolved_via`), so the driver
    /// has nothing to decide.
    Nothing,
    /// Needs the controller to have advertised `_bitrouter/route/*` for the
    /// method the action uses. Absent under `--direct` or an explicit
    /// `--base-url`: listed with the reason, not run.
    Binding,
}

/// How far an answer travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Answerable for any caller from any host. May appear on the HTTP
    /// profile — a backend that cannot answer omits the tool rather than
    /// fabricating.
    Portable,
    /// Resolves against the serving machine's own config, control socket, or
    /// installed-skills root. Local transports only.
    HostBound,
    /// Its subject is a live session on the serving process. Local only, and
    /// the only `Reach` a row may have when it has a `tui_command` and no
    /// `cli_leaf`.
    SessionBound,
}

/// One action, and the surfaces that answer it.
pub struct ActionSpec {
    /// Stable action id, e.g. `"status"`.
    pub id: &'static str,
    /// The CLI leaf that answers it, space-separated (`"skills list"`), or
    /// `None` when no CLI command does.
    pub cli_leaf: Option<&'static str>,
    /// The MCP tool that answers it, or `None` when no tool does.
    pub mcp_tool: Option<&'static str>,
    /// The TUI slash command that invokes it, without the leading `/`,
    /// space-separated when the second word is fixed (`"route reset"`).
    ///
    /// `None` is a declaration — this action is not offered in a session — not
    /// a backlog. Read by the app when it builds the reducer's command list,
    /// and by the guards.
    pub tui_command: Option<&'static str>,
    /// Whether the action observes or changes state. Read by the guards only;
    /// nothing branches on it at run time.
    pub effect: Effect,
    /// What a session must hold before the command is *offered*. Read by the
    /// app when it fills the offered command's `unavailable` reason.
    pub requires: Requires,
    /// How far the answer travels. Read by the guard that holds a row with a
    /// `tui_command` and no `cli_leaf` to `SessionBound`, and by the
    /// HTTP-profile guard, which admits only `Portable` rows.
    pub reach: Reach,
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
    /// which is generally before its report type is shared. The rows that
    /// carry `None` today, and why, are listed in this module's doc.
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
        tui_command: Some("status"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::Portable,
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
        tui_command: Some("models"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::Portable,
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
        // `/preview`, not `/route`: `/route` already means the picker, and one
        // name meaning two things on one surface is what the second name buys
        // its way out of.
        tui_command: Some("preview"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::HostBound,
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
        tui_command: None,
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::HostBound,
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<skills::SkillsReport>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        // No CLI twin, and adding `bro skills show` for the table's sake
        // would be dead surface — so this row has one surface, and the schema
        // it carries pins nothing against a second one. It is here because the
        // tool returns `Json<SkillDetail>` and therefore *does* advertise a
        // schema; a `None` beside a tool that advertises one would be the table
        // understating what it knows.
        id: "skills_get",
        cli_leaf: None,
        mcp_tool: Some("skills_get"),
        tui_command: None,
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::HostBound,
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<skills::SkillDetail>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        // Set C: the agent's advertised command list is an artefact of one
        // live ACP connection. Its CLI leaf opens a *fresh* session to ask, so
        // it cannot report on the session the TUI is in — which is why the row
        // is `SessionBound` even once that leaf exists.
        id: "commands",
        cli_leaf: Some("acp commands"),
        mcp_tool: None,
        tui_command: Some("commands"),
        effect: Effect::Read,
        requires: Requires::Nothing,
        reach: Reach::SessionBound,
        output_schema: Some(|| {
            rmcp::handler::server::tool::schema_for_output::<commands::CommandsReport>()
                .as_ref()
                .clone()
        }),
    },
    ActionSpec {
        // The route picker. No CLI leaf: naming another process's session
        // needs daemon-side session discovery and a lease-change notification,
        // neither of which exists — so a route set from a second terminal
        // could not be reflected in this session's footer.
        id: "route_set",
        cli_leaf: None,
        mcp_tool: None,
        tui_command: Some("route"),
        effect: Effect::Write {
            inverse: "route_reset",
        },
        requires: Requires::Binding,
        reach: Reach::SessionBound,
        output_schema: None,
    },
    ActionSpec {
        // `route_set`'s undo, and the reason `route_set` needs no confirmation
        // modal. One surface, so no schema to agree on: the wire response is
        // the SDK client's, not a report.
        id: "route_reset",
        cli_leaf: None,
        mcp_tool: None,
        tui_command: Some("route reset"),
        effect: Effect::Write {
            inverse: "route_set",
        },
        requires: Requires::Binding,
        reach: Reach::SessionBound,
        output_schema: None,
    },
];

#[cfg(test)]
mod tests {
    use super::{ACTIONS, Effect, Reach};

    /// A row offered in a session is inventoried on every surface it reaches.
    ///
    /// The first clause is the load-bearing one: a slash command with no CLI
    /// twin is legal only when the question it asks is *about this session*,
    /// because that is the one question a headless leaf could not answer. Any
    /// other row missing its leaf is a gap, not a design.
    #[test]
    fn every_row_with_a_tui_command_is_fully_inventoried() {
        for action in ACTIONS {
            let Some(command) = action.tui_command else {
                continue;
            };
            assert!(
                action.cli_leaf.is_some() || action.reach == Reach::SessionBound,
                "`{}` is offered in a session as `/{command}` with no CLI leaf, but its \
                 subject is not a live session (`{:?}`); add the leaf or mark it \
                 `Reach::SessionBound`",
                action.id,
                action.reach
            );
            if let (Some(leaf), Some(tool)) = (action.cli_leaf, action.mcp_tool) {
                assert!(
                    action.output_schema.is_some(),
                    "`{}` answers on both machine surfaces (`bro {leaf}` and the \
                     `{tool}` tool) but carries no `output_schema`, so nothing holds the \
                     two shapes to one another",
                    action.id
                );
            }
        }
    }

    /// Every write names an inverse row, and that row names it back.
    ///
    /// This is what lets a session offer a write with no confirmation modal:
    /// the undo is a row, so it is typable, and the pairing is checked here
    /// rather than remembered.
    #[test]
    fn every_write_names_an_inverse_row_that_names_it_back() {
        for action in ACTIONS {
            let Effect::Write { inverse } = action.effect else {
                continue;
            };
            let Some(row) = ACTIONS.iter().find(|candidate| candidate.id == inverse) else {
                panic!(
                    "`{}` names inverse `{inverse}`, which is not a row. A write's undo \
                     must itself be an action",
                    action.id
                )
            };
            let Effect::Write { inverse: back } = row.effect else {
                panic!(
                    "`{}` names `{inverse}` as its inverse, but `{inverse}` is a read. An \
                     undo that changes nothing undoes nothing",
                    action.id
                )
            };
            assert_eq!(
                back, action.id,
                "`{}` names `{inverse}` as its inverse, but `{inverse}` names `{back}`. \
                 The pairing has to close",
                action.id
            );
            if action.tui_command.is_some() {
                assert!(
                    row.tui_command.is_some(),
                    "`{}` is typable in a session but its inverse `{inverse}` is not; an \
                     undo the user cannot reach is not an undo",
                    action.id
                );
            }
        }
    }
}

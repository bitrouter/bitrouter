//! The session surface of the shared actions.
//!
//! Lives in `crate::actions`, not `crate::chat`: deciding what a session offers
//! reads the `ACTIONS` table and what the controller advertised, and the chat
//! guard keeps `chat/` from naming anything daemon-wide. The driver is *handed*
//! the finished list and names nothing else — the same move `route_control`
//! already makes.

use bitrouter_mcp::actions::{ACTIONS, Requires};
use bitrouter_sdk::acp::client::{AcpClient, RouteMethod};
use bitrouter_tui::machine::Command;

/// What `/route` says at a session with no route surface to open a picker over.
const NOT_ROUTABLE: &str = "this session cannot be rerouted (the controller advertises no route \
                            control: running direct, or without a trusted local daemon binding)";
/// What `/route reset` says at the same session. Distinct from [`NOT_ROUTABLE`]
/// because a controller may advertise `set` without `reset`, and a reader told
/// "cannot be rerouted" would not know which of the two they had.
const NOT_RESETTABLE: &str = "this session has no route lease to drop (the controller advertises \
                              no route reset)";

/// One line of help per row, for `/commands`.
///
/// App-side rather than on the row because no other surface reads it: the MCP
/// tool has its own description and the CLI leaf has clap's. A guard fails a
/// row that carries a `tui_command` and has no arm here, so this cannot fall
/// behind the table.
pub fn summary_for(action: &str) -> &'static str {
    match action {
        "commands" => "list the commands this session offers",
        "route_set" => "choose the route for the rest of the session",
        "route_reset" => "drop the route lease, so the daemon's default applies",
        _ => "",
    }
}

/// The `ACTIONS` rows that carry a `tui_command`, in table order, with each
/// row's requirement resolved against what this controller advertised.
///
/// Resolved once, at launch, and handed to the reducer as plain data. A row
/// whose requirement is unmet is still returned — with the reason — because a
/// control the session cannot run should say why rather than vanish.
pub fn offered_commands(client: &AcpClient) -> Vec<Command> {
    let capability = client.route_control();
    ACTIONS
        .iter()
        .filter_map(|row| {
            let name = row.tui_command?;
            let unavailable = match (row.requires, row.id) {
                (Requires::Nothing, _) => None,
                // `reset` is advertised separately from `list`/`set`, so it is
                // asked about separately.
                (Requires::Binding, "route_reset") => {
                    (!capability.allows(RouteMethod::Reset)).then_some(NOT_RESETTABLE)
                }
                // The picker lists with one method and sets with another, so
                // both must be there for it to be worth opening.
                (Requires::Binding, _) => (!(capability.allows(RouteMethod::List)
                    && capability.allows(RouteMethod::Set)))
                .then_some(NOT_ROUTABLE),
            };
            Some(Command {
                name,
                action: row.id,
                summary: summary_for(row.id),
                unavailable,
            })
        })
        .collect()
}

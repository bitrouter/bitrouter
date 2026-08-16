//! The `_meta` key that tells an ACP client whose spend a cost figure is.
//!
//! ACP carries `UsageUpdate.cost` but no field saying what it covers, so
//! BitRouter puts the scope in `_meta` under a namespaced key. Both sides of
//! that key live here — `acp_cli` writes it, [`from_usage`] reads it — and
//! neither is in `bitrouter-tui`, because the key is BitRouter's and that
//! crate knows only ACP.
//!
//! The rendering *is* in the crate ([`bitrouter_tui::cost`]): a figure and
//! whose it is are protocol-shaped once the scope has been decided. What stays
//! here is the deciding — the wire spelling, and the refusal to render an
//! unscoped figure at all.

use agent_client_protocol::schema::v1::UsageUpdate;
use bitrouter_tui::cost::{Cost, Scope};

/// `_meta` key naming whose spend a `UsageUpdate.cost` describes.
///
/// Namespaced, because `_meta` is a shared extension space and ACP tells
/// implementations to assume nothing about keys they do not own.
pub const COST_SCOPE_META_KEY: &str = "bitrouter/costScope";

/// The wire spelling of a scope BitRouter measured.
///
/// Paired with the private `from_wire` — the two are the only place these strings
/// are written down, so a change to one is a change to the other in the same
/// diff.
pub fn to_wire(scope: Scope) -> &'static str {
    match scope {
        Scope::Session => "session",
        Scope::Wider => "daemon_wide",
    }
}

/// Parse the wire spelling. An unknown value is **not** treated as
/// session-scoped: an unrecognised label means we do not know whose number
/// this is, which is exactly [`Scope::Wider`]'s warning.
fn from_wire(value: &str) -> Scope {
    match value {
        "session" => Scope::Session,
        _ => Scope::Wider,
    }
}

/// Read a cost off a `UsageUpdate`, when it carries one *and* says whose it
/// is.
///
/// `None` when the agent reported no cost, or reported one with no scope. An
/// unscoped figure is dropped rather than guessed at: a generic ACP agent may
/// well send `cost` with no idea of this distinction, and assuming it meant
/// "the session's" is the precise error this exists to prevent.
pub fn from_usage(usage: &UsageUpdate) -> Option<Cost> {
    let cost = usage.cost.as_ref()?;
    let scope = usage
        .meta
        .as_ref()
        .and_then(|meta| meta.get(COST_SCOPE_META_KEY))
        .and_then(|value| value.as_str())
        .map(from_wire)?;
    Some(Cost::new(cost.amount, cost.currency.clone(), scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::Cost as WireCost;

    fn usage_with(cost: Option<WireCost>, scope: Option<&str>) -> UsageUpdate {
        let mut usage = UsageUpdate::new(1_500, 200_000);
        usage.cost = cost;
        if let Some(scope) = scope {
            let mut meta = serde_json::Map::new();
            meta.insert(
                COST_SCOPE_META_KEY.to_string(),
                serde_json::Value::String(scope.to_string()),
            );
            usage.meta = Some(meta);
        }
        usage
    }

    /// Rendered through the crate's own plain-text writer, so this module
    /// never names a widget type — the app decides scope, the crate draws.
    fn text(cost: &Cost) -> String {
        bitrouter_tui::plain::text(&cost.render())
    }

    /// Both scopes survive the round trip they actually take: `acp_cli` writes
    /// `to_wire`, a client reads `from_usage`. The two spellings are only
    /// correct together, which is why one test covers both directions.
    #[test]
    fn the_wire_spelling_round_trips_in_both_scopes() {
        for scope in [Scope::Session, Scope::Wider] {
            let usage = usage_with(Some(WireCost::new(0.42, "USD")), Some(to_wire(scope)));
            let read = from_usage(&usage).expect("a scoped cost");
            assert_eq!(
                read,
                Cost::new(0.42, "USD", scope),
                "{scope:?} did not survive the wire"
            );
        }
    }

    /// The scope reaches the rendered line, not just the struct — the two
    /// scopes must be tellable apart by a reader of the screen.
    #[test]
    fn the_scope_reaches_the_rendered_line() {
        let session = from_usage(&usage_with(
            Some(WireCost::new(0.42, "USD")),
            Some(to_wire(Scope::Session)),
        ))
        .expect("cost");
        let wider = from_usage(&usage_with(
            Some(WireCost::new(1.32, "USD")),
            Some(to_wire(Scope::Wider)),
        ))
        .expect("cost");
        assert_ne!(text(&session), text(&wider));
        assert!(text(&wider).contains("all callers"));
    }

    /// A cost with no scope is not rendered. A generic ACP agent may send
    /// `cost` with no notion of this distinction, and treating that as the
    /// session's is the exact error this module exists to prevent.
    #[test]
    fn an_unscoped_cost_is_never_rendered() {
        assert!(
            from_usage(&usage_with(Some(WireCost::new(0.42, "USD")), None)).is_none(),
            "a figure with no scope must not reach the screen"
        );
    }

    /// An unrecognised scope is treated as wider-than-this-session, not as the
    /// session's: not knowing whose number it is *is* the warning.
    #[test]
    fn an_unknown_scope_degrades_to_the_cautious_reading() {
        let cost = from_usage(&usage_with(
            Some(WireCost::new(0.42, "USD")),
            Some("something-new"),
        ))
        .expect("cost");
        assert_eq!(cost, Cost::new(0.42, "USD", Scope::Wider));
    }

    /// No cost reported is not a cost of zero — `from_usage` yields nothing
    /// rather than a figure, and the caller draws `unreported`.
    #[test]
    fn an_absent_cost_is_not_a_zero() {
        assert!(from_usage(&usage_with(None, Some("session"))).is_none());
    }
}

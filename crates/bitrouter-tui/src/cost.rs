//! The cost line: what this session spent, and **whose** spend that number is.
//!
//! # Why scope is not optional
//!
//! A currency figure with no scope is the single most misleading thing a
//! router's UI can draw. The previous status bar rendered the daemon's total
//! and labelled it the session's, so a user with anything else routing saw a
//! number that was real, precise, and about somebody else's work.
//!
//! ACP has no field for this, so BitRouter puts the scope in `UsageUpdate`'s
//! `_meta` (`bitrouter/costScope`). This module refuses to render a figure
//! without it. Three states, and each reads differently:
//!
//! - **Attributed** — this session's own spend. Shown plainly.
//! - **Daemon-wide** — every caller in the window, because this session's
//!   traffic could not be told apart. Shown, and visibly marked as not the
//!   session's.
//! - **Unreported** — the agent sent no cost at all. Shown as *unreported*,
//!   never as `$0.00`: a router that cannot see a price has not observed a
//!   free turn.

use agent_client_protocol_schema::v1::UsageUpdate;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// `_meta` key naming whose spend a `UsageUpdate.cost` describes.
///
/// Namespaced, because `_meta` is a shared extension space and ACP tells
/// implementations to assume nothing about keys they do not own. Must match
/// the emitting side (`bitrouter`'s `acp_cli::COST_SCOPE_META_KEY`).
pub const COST_SCOPE_META_KEY: &str = "bitrouter/costScope";

/// Whose traffic a cost figure describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// This session's own spend.
    Session,
    /// Every caller the daemon served in this session's window. Reported when
    /// the session's traffic carries no attribution — a caller's own
    /// credential must never be rewritten to tag it.
    DaemonWide,
}

impl Scope {
    /// Parse the wire spelling. An unknown value is **not** treated as
    /// session-scoped: an unrecognised label means we do not know whose
    /// number this is, which is exactly `DaemonWide`'s warning.
    fn from_wire(value: &str) -> Self {
        match value {
            "session" => Self::Session,
            _ => Self::DaemonWide,
        }
    }
}

/// A cost figure and the scope it applies to.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    amount: f64,
    currency: String,
    scope: Scope,
}

impl Cost {
    /// Read a cost off a `UsageUpdate`, when it carries one *and* says whose
    /// it is.
    ///
    /// `None` when the agent reported no cost, or reported one with no scope.
    /// An unscoped figure is dropped rather than guessed at: a generic ACP
    /// agent may well send `cost` with no idea of this distinction, and
    /// assuming it meant "the session's" is the precise error this exists to
    /// prevent.
    pub fn from_usage(usage: &UsageUpdate) -> Option<Self> {
        let cost = usage.cost.as_ref()?;
        let scope = usage
            .meta
            .as_ref()
            .and_then(|meta| meta.get(COST_SCOPE_META_KEY))
            .and_then(|value| value.as_str())
            .map(Scope::from_wire)?;
        Some(Self {
            amount: cost.amount,
            currency: cost.currency.clone(),
            scope,
        })
    }

    /// The cost as it appears in the live area.
    pub fn render(&self) -> Line<'static> {
        let figure = format!("{} {:.4}", self.currency, self.amount);
        match self.scope {
            Scope::Session => Line::from(Span::styled(
                figure,
                Style::default().add_modifier(Modifier::DIM),
            )),
            // The qualifier is part of the figure, not a footnote: whatever
            // truncates this line must lose the number before the caveat.
            Scope::DaemonWide => Line::from(vec![
                Span::styled(
                    "all callers ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(figure, Style::default().fg(Color::Yellow)),
            ]),
        }
    }
}

/// What to draw when the agent reported no cost at all.
///
/// A separate function rather than a `Cost` variant, so there is no way to
/// render an absent figure as a number. Most ACP agents are not routers and
/// will never send `cost`; that is not a zero.
pub fn unreported() -> Line<'static> {
    Line::from(Span::styled(
        "cost unreported",
        Style::default().add_modifier(Modifier::DIM),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::Cost as WireCost;

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

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

    /// The plan's done-when: the scope label renders in both scopes, and the
    /// two must be tellable apart at a glance.
    #[test]
    fn the_scope_label_renders_in_both_scopes() {
        let session = Cost::from_usage(&usage_with(
            Some(WireCost::new(0.42, "USD")),
            Some("session"),
        ))
        .expect("an attributed cost");
        let session = text(&session.render());
        assert!(session.contains("0.42"), "{session:?}");
        assert!(
            !session.contains("all callers"),
            "the session's own spend carries no caveat: {session:?}"
        );

        let wide = Cost::from_usage(&usage_with(
            Some(WireCost::new(1.32, "USD")),
            Some("daemon_wide"),
        ))
        .expect("a daemon-wide cost");
        let wide = text(&wide.render());
        assert!(wide.contains("1.32"), "{wide:?}");
        assert!(
            wide.contains("all callers"),
            "daemon-wide spend must be visibly marked as not the session's: {wide:?}"
        );
        assert_ne!(session, wide, "the two scopes must not render alike");
    }

    /// The caveat precedes the number, so truncation loses the figure rather
    /// than the warning about it.
    #[test]
    fn the_daemon_wide_caveat_precedes_the_figure() {
        let wide = Cost::from_usage(&usage_with(
            Some(WireCost::new(1.32, "USD")),
            Some("daemon_wide"),
        ))
        .expect("cost");
        let rendered = text(&wide.render());
        let caveat = rendered.find("all callers").expect("caveat present");
        let figure = rendered.find("1.32").expect("figure present");
        assert!(caveat < figure, "{rendered:?}");
    }

    /// A cost with no scope is not rendered. A generic ACP agent may send
    /// `cost` with no notion of this distinction, and treating that as the
    /// session's is the exact error this module exists to prevent.
    #[test]
    fn an_unscoped_cost_is_never_rendered() {
        assert!(
            Cost::from_usage(&usage_with(Some(WireCost::new(0.42, "USD")), None)).is_none(),
            "a figure with no scope must not reach the screen"
        );
    }

    /// An unrecognised scope is treated as daemon-wide, not as the session's:
    /// not knowing whose number it is *is* the daemon-wide warning.
    #[test]
    fn an_unknown_scope_degrades_to_the_cautious_reading() {
        let cost = Cost::from_usage(&usage_with(
            Some(WireCost::new(0.42, "USD")),
            Some("something-new"),
        ))
        .expect("cost");
        assert_eq!(cost.scope, Scope::DaemonWide);
        assert!(text(&cost.render()).contains("all callers"));
    }

    /// No cost reported is not a cost of zero.
    #[test]
    fn an_absent_cost_is_unreported_not_zero() {
        assert!(Cost::from_usage(&usage_with(None, Some("session"))).is_none());
        let rendered = text(&unreported());
        assert!(rendered.contains("unreported"), "{rendered:?}");
        assert!(
            !rendered.contains('0'),
            "an unobserved price must not read as a free turn: {rendered:?}"
        );
    }
}

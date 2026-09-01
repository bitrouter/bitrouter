//! The cost line: what a session spent, and **whose** spend that number is.
//!
//! # Why scope is not optional
//!
//! A currency figure with no scope is the single most misleading thing an
//! agent's UI can draw. BitRouter's previous status bar rendered the daemon's
//! total and labelled it the session's, so a user with anything else routing
//! saw a number that was real, precise, and about somebody else's work.
//!
//! ACP carries the figure — `UsageUpdate.cost` — but no field saying whose it
//! is, so BitRouter puts the scope in `_meta` under a namespaced key. Both
//! sides of that key live here: `acp_cli` writes it with [`to_wire`], and
//! [`from_usage`] reads it back.
//!
//! [`Scope`] is still never inferred. [`Cost::new`] has no constructor that
//! skips it, and [`from_usage`] returns `None` for a figure that arrives
//! without one rather than guessing "the session's" — which is the precise
//! error this module exists to prevent, and the likely one, since a generic
//! ACP agent may send `cost` with no notion of the distinction.
//!
//! Three states, and each reads differently:
//!
//! - **Session** — this session's own spend. Shown plainly.
//! - **Wider** — a figure covering more than this session. Shown, and visibly
//!   marked as not the session's.
//! - **Unreported** — no cost was reported at all. Shown as *unreported* via
//!   [`unreported`], never as `$0.00`: a client that cannot see a price has
//!   not observed a free turn.

use agent_client_protocol_schema::v1::UsageUpdate;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Whose traffic a cost figure describes.
///
/// Two variants because two is what honesty needs: either the number is this
/// session's, or it is not and must say so. A third state — no number at all —
/// is [`unreported`] rather than a variant, so there is no way to render an
/// absent figure through this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// This session's own spend.
    Session,
    /// A figure covering more than this session — every caller the agent's
    /// backend served in the window, because this session's traffic could not
    /// be told apart.
    Wider,
}

/// A cost figure and the scope it applies to.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    amount: f64,
    currency: String,
    scope: Scope,
}

impl Cost {
    /// A figure that knows whose it is.
    ///
    /// There is deliberately no constructor taking a bare `UsageUpdate`: the
    /// scope is not on the ACP wire, so a `From<UsageUpdate>` would have to
    /// guess, and guessing "the session's" is the precise error this module
    /// exists to prevent.
    pub fn new(amount: f64, currency: impl Into<String>, scope: Scope) -> Self {
        Self {
            amount,
            currency: currency.into(),
            scope,
        }
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
            Scope::Wider => Line::from(vec![
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

/// What to draw when no cost was reported at all.
///
/// A separate function rather than a [`Scope`] variant, so there is no way to
/// render an absent figure as a number. Most ACP agents are not routers and
/// will never send `cost`; that is not a zero.
pub fn unreported() -> Line<'static> {
    Line::from(Span::styled(
        "cost unreported",
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// `_meta` key naming whose spend a `UsageUpdate.cost` describes.
///
/// Namespaced, because `_meta` is a shared extension space and ACP tells
/// implementations to assume nothing about keys they do not own.
pub const COST_SCOPE_META_KEY: &str = "bitrouter/costScope";

/// The wire spelling of a scope BitRouter measured.
///
/// Paired with the private `from_wire` — the two are the only place these
/// strings are written down, so a change to one is a change to the other in
/// the same diff.
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
/// unscoped figure is dropped rather than guessed at, and the footer draws
/// [`unreported`] in its place.
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

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// The scope label renders in both scopes, and the two must be tellable
    /// apart at a glance.
    #[test]
    fn the_scope_label_renders_in_both_scopes() {
        let session = text(&Cost::new(0.42, "USD", Scope::Session).render());
        assert!(session.contains("0.42"), "{session:?}");
        assert!(
            !session.contains("all callers"),
            "the session's own spend carries no caveat: {session:?}"
        );

        let wider = text(&Cost::new(1.32, "USD", Scope::Wider).render());
        assert!(wider.contains("1.32"), "{wider:?}");
        assert!(
            wider.contains("all callers"),
            "a wider figure must be visibly marked as not the session's: {wider:?}"
        );
        assert_ne!(session, wider, "the two scopes must not render alike");
    }

    /// The caveat precedes the number, so truncation loses the figure rather
    /// than the warning about it.
    #[test]
    fn the_wider_caveat_precedes_the_figure() {
        let rendered = text(&Cost::new(1.32, "USD", Scope::Wider).render());
        let caveat = rendered.find("all callers").expect("caveat present");
        let figure = rendered.find("1.32").expect("figure present");
        assert!(caveat < figure, "{rendered:?}");
    }

    /// No cost reported is not a cost of zero.
    #[test]
    fn an_absent_cost_is_unreported_not_zero() {
        let rendered = text(&unreported());
        assert!(rendered.contains("unreported"), "{rendered:?}");
        assert!(
            !rendered.contains('0'),
            "an unobserved price must not read as a free turn: {rendered:?}"
        );
    }

    use agent_client_protocol_schema::v1::Cost as WireCost;

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

    /// Rendered through the plain-text writer so a test never depends on a
    /// terminal.
    fn shown(cost: &Cost) -> String {
        text(&cost.render())
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
        assert_ne!(shown(&session), shown(&wider));
        assert!(shown(&wider).contains("all callers"));
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

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
//! is. So [`Scope`] is a **parameter**, not something this module infers: the
//! caller has to answer it before a figure can be built, and there is no
//! constructor that skips the question. Where the answer comes from is the
//! caller's business; for BitRouter it rides in `_meta` under a namespaced key
//! that lives in the app, not here.
//!
//! Three states, and each reads differently:
//!
//! - **Session** — this session's own spend. Shown plainly.
//! - **Wider** — a figure covering more than this session. Shown, and visibly
//!   marked as not the session's.
//! - **Unreported** — no cost was reported at all. Shown as *unreported* via
//!   [`unreported`], never as `$0.00`: a client that cannot see a price has
//!   not observed a free turn.

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
}

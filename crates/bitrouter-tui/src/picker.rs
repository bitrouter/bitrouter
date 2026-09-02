//! The route picker: choose where this session's traffic goes, mid-session.
//!
//! Drawn from a `_bitrouter/route/list` response and nothing else: the routes
//! the daemon suggests, and the lease it confirms is in force. Whether the
//! control should appear at all is the caller's to decide and is passed in —
//! [`Picker::open`] takes `available`, the controller's three-condition
//! `routeControl` capability gate, and there is no way to draw a picker
//! without answering it.
//!
//! It is the easiest control to get dishonestly wrong, in two ways this module
//! refuses:
//!
//! - **Against a controller that advertises no route control, there is no
//!   picker.** Not a greyed-out one, not an empty list: [`Picker::open`]
//!   returns `None` and nothing is drawn. A control that cannot act is worse
//!   than an absent one, because absence is legible and a dead control is a
//!   lie.
//! - **Selecting does not mark the route as changed.** [`Picker::choose`]
//!   returns the route the caller must go and *set*; only what the daemon
//!   confirms afterwards — the `current` in the set response, or the next
//!   list — is treated as in force. The difference matters because
//!   `_bitrouter/route/set` can refuse, and a picker that painted the new
//!   route on selection would report a switch that never happened.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// The routes on offer, and which one is in force.
#[derive(Debug, Clone)]
pub struct Picker {
    entries: Vec<Entry>,
}

/// One route, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    route: String,
    /// Whether the daemon reported this as the lease in force.
    current: bool,
}

impl Picker {
    /// Build a picker from a `_bitrouter/route/list` response: the suggested
    /// `routes`, and the `current` lease if one is installed.
    ///
    /// `None` when the controller does not advertise route control
    /// (`available` is false) or the daemon suggested no routes — in both
    /// cases there is nothing to choose between, and drawing a chooser would
    /// imply otherwise.
    pub fn open(available: bool, routes: &[String], current: Option<&str>) -> Option<Self> {
        if !available || routes.is_empty() {
            return None;
        }
        Some(Self {
            entries: routes
                .iter()
                .map(|route| Entry {
                    route: route.clone(),
                    // `current` is the daemon's word for "this is the route",
                    // and the only thing that may mark one.
                    current: current == Some(route.as_str()),
                })
                .collect(),
        })
    }

    /// The picker as it appears in the live area.
    pub fn render(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "route: ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        for (index, entry) in self.entries.iter().enumerate() {
            spans.push(Span::styled(
                format!("[{}]", index + 1),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            let style = if entry.current {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(format!("{} ", entry.route), style));
        }
        spans.push(Span::styled(
            "[esc] keep",
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }

    /// The route a keypress selects, if any.
    ///
    /// Returns a route to *attempt*, not a decision already taken — see the
    /// module docs. An unoffered key selects nothing.
    pub fn choose(&self, key: char) -> Option<String> {
        let index = key.to_digit(10)?.checked_sub(1)? as usize;
        self.entries.get(index).map(|entry| entry.route.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> Vec<String> {
        vec!["@balanced".to_string(), "openai:gpt-5".to_string()]
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// Selecting a route yields the route to set — at the renderer's
    /// boundary. Issuing `_bitrouter/route/set` is the caller's job; this
    /// returns what to issue it for.
    #[test]
    fn selecting_yields_the_route_to_set() {
        let picker = Picker::open(true, &routes(), Some("@balanced"))
            .expect("a picker with routes to choose between");
        assert_eq!(picker.choose('2').as_deref(), Some("openai:gpt-5"));
        assert_eq!(picker.choose('1').as_deref(), Some("@balanced"));
    }

    /// A controller with no route control gets no picker at all — not an
    /// empty one, not a disabled one. A control that cannot act is worse than
    /// an absent one.
    #[test]
    fn no_route_control_means_no_picker() {
        assert!(
            Picker::open(false, &routes(), Some("@balanced")).is_none(),
            "a controller that advertises no route control must show no picker"
        );
        assert!(
            Picker::open(true, &[], None).is_none(),
            "nothing to choose between is also no picker"
        );
    }

    /// Only what the daemon reported as `current` is marked. The picker never
    /// paints a selection as in force, because `set` can refuse.
    #[test]
    fn only_the_daemons_reported_route_is_marked() {
        let picker = Picker::open(true, &routes(), Some("@balanced")).expect("picker");
        let before = picker.render();

        // Choosing does not change what is marked; only a fresh, confirmed
        // state may.
        assert_eq!(picker.choose('2').as_deref(), Some("openai:gpt-5"));
        assert_eq!(
            text(&picker.render()),
            text(&before),
            "selection must not repaint the route as changed"
        );

        // A session where nothing is in force yet marks nothing.
        let none_current = Picker::open(true, &routes(), None).expect("picker");
        assert!(
            none_current.entries.iter().all(|e| !e.current),
            "no route reported means none marked"
        );
        // And a lease outside the suggestion list marks nothing either: the
        // list is suggestions, not the grammar of what may be leased.
        let elsewhere =
            Picker::open(true, &routes(), Some("anthropic:claude-opus")).expect("picker");
        assert!(elsewhere.entries.iter().all(|e| !e.current));
    }

    /// The list names every suggested route and how to pick it.
    #[test]
    fn the_picker_names_every_route() {
        let picker = Picker::open(true, &routes(), Some("@balanced")).expect("picker");
        let rendered = text(&picker.render());
        assert!(rendered.contains("[1]@balanced"), "{rendered:?}");
        assert!(rendered.contains("[2]openai:gpt-5"), "{rendered:?}");
    }

    /// A key outside the offered range selects nothing rather than wrapping.
    #[test]
    fn an_unoffered_key_selects_nothing() {
        let picker = Picker::open(true, &routes()[..1], None).expect("picker");
        assert!(picker.choose('2').is_none());
        assert!(picker.choose('0').is_none());
        assert!(picker.choose('x').is_none());
    }
}

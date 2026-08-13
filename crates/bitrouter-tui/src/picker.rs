//! The provider picker: choose where this session's traffic goes, mid-session.
//!
//! This is the one thing no harness and no other ACP client can offer, because
//! it is the router's own surface — but it is also the easiest control to get
//! dishonestly wrong, in two ways this module refuses:
//!
//! - **Against an agent with no `providers/*`, there is no picker.** Not a
//!   greyed-out one, not an empty list: [`Picker::open`] returns `None` and
//!   nothing is drawn. A control that cannot act is worse than an absent one,
//!   because absence is legible and a dead control is a lie.
//! - **Selecting does not mark the route as changed.** [`Picker::choose`]
//!   returns the provider the caller must go and *set*; only what
//!   `providers/list` reports afterwards is treated as in force. The
//!   difference matters because `providers/set` can fail — a session with no
//!   attributable traffic cannot be rerouted at all — and a picker that
//!   painted the new provider on selection would report a switch that never
//!   happened.

use agent_client_protocol_schema::v1::ProviderInfo;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// The routable providers, and which one is in force.
#[derive(Debug, Clone)]
pub struct Picker {
    entries: Vec<Entry>,
}

/// One provider, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: String,
    /// Whether `providers/list` reported this as the effective route.
    current: bool,
}

impl Picker {
    /// Build a picker from a `providers/list` response.
    ///
    /// `None` when the agent does not serve `providers/*` (`available` is
    /// false) or reported no routable providers — in both cases there is
    /// nothing to choose between, and drawing a chooser would imply
    /// otherwise.
    pub fn open(available: bool, providers: &[ProviderInfo]) -> Option<Self> {
        if !available || providers.is_empty() {
            return None;
        }
        Some(Self {
            entries: providers
                .iter()
                .map(|provider| Entry {
                    id: provider.provider_id.0.to_string(),
                    // `current` is the agent's word for "this is the route",
                    // and the only thing that may mark one.
                    current: provider.current.is_some(),
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
            spans.push(Span::styled(format!("{} ", entry.id), style));
        }
        spans.push(Span::styled(
            "[esc] keep",
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }

    /// The provider a keypress selects, if any.
    ///
    /// Returns an id to *attempt*, not a decision already taken — see the
    /// module docs. An unoffered key selects nothing.
    pub fn choose(&self, key: char) -> Option<String> {
        let index = key.to_digit(10)?.checked_sub(1)? as usize;
        self.entries.get(index).map(|entry| entry.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{
        LlmProtocol, ProviderCurrentConfig, ProviderId, ProviderInfo,
    };

    fn provider(id: &str, current: bool) -> ProviderInfo {
        ProviderInfo::new(
            ProviderId::new(id),
            vec![LlmProtocol::OpenAi],
            false,
            current
                .then(|| ProviderCurrentConfig::new(LlmProtocol::OpenAi, "https://example.com/v1")),
        )
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// Selecting a provider yields the id to set — the plan's done-when, at
    /// the renderer's boundary. Issuing `providers/set` is the caller's job;
    /// this returns what to issue it for.
    #[test]
    fn selecting_yields_the_provider_to_set() {
        let picker = Picker::open(true, &[provider("alpha", true), provider("beta", false)])
            .expect("a picker with providers to choose between");
        assert_eq!(picker.choose('2').as_deref(), Some("beta"));
        assert_eq!(picker.choose('1').as_deref(), Some("alpha"));
    }

    /// An agent with no routing surface gets no picker at all — not an empty
    /// one, not a disabled one. A control that cannot act is worse than an
    /// absent one.
    #[test]
    fn no_routing_surface_means_no_picker() {
        assert!(
            Picker::open(false, &[provider("alpha", true)]).is_none(),
            "an agent that does not serve providers/* must show no picker"
        );
        assert!(
            Picker::open(true, &[]).is_none(),
            "nothing to choose between is also no picker"
        );
    }

    /// Only what the agent reported as `current` is marked. The picker never
    /// paints a selection as in force, because `providers/set` can fail.
    #[test]
    fn only_the_agents_reported_route_is_marked() {
        let picker = Picker::open(true, &[provider("alpha", true), provider("beta", false)])
            .expect("picker");
        let before = picker.render();

        // Choosing does not change what is marked; only a fresh
        // `providers/list` may.
        assert_eq!(picker.choose('2').as_deref(), Some("beta"));
        assert_eq!(
            text(&picker.render()),
            text(&before),
            "selection must not repaint the route as changed"
        );

        // A session where nothing is in force yet marks nothing.
        let none_current = Picker::open(true, &[provider("alpha", false), provider("beta", false)])
            .expect("picker");
        assert!(
            none_current.entries.iter().all(|e| !e.current),
            "no route reported means none marked"
        );
    }

    /// The list names every routable provider and how to pick it.
    #[test]
    fn the_picker_names_every_provider() {
        let picker = Picker::open(true, &[provider("alpha", true), provider("beta", false)])
            .expect("picker");
        let rendered = text(&picker.render());
        assert!(rendered.contains("[1]alpha"), "{rendered:?}");
        assert!(rendered.contains("[2]beta"), "{rendered:?}");
    }

    /// A key outside the offered range selects nothing rather than wrapping.
    #[test]
    fn an_unoffered_key_selects_nothing() {
        let picker = Picker::open(true, &[provider("alpha", true)]).expect("picker");
        assert!(picker.choose('2').is_none());
        assert!(picker.choose('0').is_none());
        assert!(picker.choose('x').is_none());
    }
}

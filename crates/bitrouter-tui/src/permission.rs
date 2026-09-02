//! The permission prompt: what the agent wants to do, and the choice.
//!
//! # Why this is a modal and not a line
//!
//! Every other thing this crate draws is a report of something that already
//! happened. This one blocks a turn until a person answers, and the answer may
//! let an agent write a file or run a command. It gets the live area, and the
//! options are numbered so choosing one is a single keystroke.
//!
//! # The safe default
//!
//! Two rules, both about what happens when the user does *not* engage:
//!
//! - An unrecognised key selects nothing. A prompt that treated a stray
//!   keystroke as consent would be worse than no prompt.
//! - Cancelling picks a reject option when the agent offered one. "I did not
//!   answer" must never resolve to "yes".
//!
//! The options come from the agent, so this module never invents one — it can
//! only choose among what was offered, or decline to choose.

use agent_client_protocol_schema::v1::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    SelectedPermissionOutcome,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// A permission request being shown, and the options it offered.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Which request this is, as the caller's own transport identifies it.
    ///
    /// Carried because questions can queue: a second request may arrive while
    /// the first is still on screen, and a queue of prompts with no identity
    /// cannot route its answers back to the right request.
    id: String,
    /// What the agent said it is about to do.
    title: String,
    /// The choices, in the order the agent listed them.
    options: Vec<PermissionOption>,
}

impl Prompt {
    /// Build a prompt from the parts of a `session/request_permission`.
    ///
    /// Takes the fields rather than the request struct: a caller may hold the
    /// pending request in its own type, and this crate should not require one
    /// shape of it. `tool_call_id` is the fallback label for an agent that
    /// sent no title — an unlabelled prompt is still answerable, and a blank
    /// one would not be.
    pub fn new(
        id: impl Into<String>,
        title: Option<String>,
        tool_call_id: impl Into<String>,
        options: Vec<PermissionOption>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.unwrap_or_else(|| tool_call_id.into()),
            options,
        }
    }

    /// Which request this prompt answers.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The prompt as it appears in the live area.
    ///
    /// One line: the terminal rows here are borrowed from the user's shell,
    /// and a permission question does not need a box drawn around it.
    pub fn render(&self) -> Line<'static> {
        let mut spans = vec![
            Span::styled(
                "allow? ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.title.clone()),
            Span::raw("  "),
        ];
        for (index, option) in self.options.iter().enumerate() {
            spans.push(Span::styled(
                format!("[{}]", index + 1),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("{} ", option.name)));
        }
        spans.push(Span::styled(
            "[esc] deny",
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }

    /// Resolve a keypress to the option it selects.
    ///
    /// `None` means the key chose nothing and the prompt stays up. Only the
    /// digits the agent actually offered select — `[3]` against a two-option
    /// request selects nothing rather than wrapping around.
    pub fn choose(&self, key: char) -> Option<PermissionOptionId> {
        let index = key.to_digit(10)?.checked_sub(1)? as usize;
        self.options
            .get(index)
            .map(|option| option.option_id.clone())
    }

    /// What to answer when the user declines to choose (escape, or the
    /// session ending under an open prompt).
    ///
    /// Prefers an explicit reject option so the agent hears a decision it
    /// understands. `None` when the agent offered no way to say no, in which
    /// case the caller cancels — never silently allows.
    pub fn deny(&self) -> Option<PermissionOptionId> {
        self.options
            .iter()
            .find(|option| {
                matches!(
                    option.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                )
            })
            .map(|option| option.option_id.clone())
    }

    /// The outcome to resolve this request with when nobody answered it.
    ///
    /// An explicit reject when the agent offered one, so it hears a decision
    /// it understands. Otherwise **cancelled** — never a selection, because
    /// the only options left would be ones that say yes.
    ///
    /// Every path that abandons a question goes through here: `Esc`, a control
    /// chord, the terminal ending, a cancelled turn, a turn that ended with a
    /// question still open, a signal. One rule, and it is the safe one.
    pub fn unanswered(&self) -> RequestPermissionOutcome {
        match self.deny() {
            Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
            None => RequestPermissionOutcome::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(options: Vec<PermissionOption>) -> Prompt {
        Prompt::new("r1", Some("Write src/main.rs".to_string()), "t1", options)
    }

    fn option(id: &str, name: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), name, kind)
    }

    fn offered() -> Vec<PermissionOption> {
        vec![
            option("allow", "allow once", PermissionOptionKind::AllowOnce),
            option("always", "always allow", PermissionOptionKind::AllowAlways),
            option("no", "reject", PermissionOptionKind::RejectOnce),
        ]
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// The prompt must say what is being permitted and how to answer.
    #[test]
    fn a_prompt_names_the_action_and_its_options() {
        let prompt = request(offered());
        let rendered = text(&prompt.render());
        assert!(
            rendered.contains("Write src/main.rs"),
            "the user must know what they are allowing: {rendered:?}"
        );
        for (index, option) in offered().iter().enumerate() {
            assert!(
                rendered.contains(&format!("[{}]", index + 1)),
                "{rendered:?}"
            );
            assert!(rendered.contains(&option.name), "{rendered:?}");
        }
    }

    /// A keypress drives the request to a decision — the whole point of the
    /// modal.
    #[test]
    fn a_keypress_selects_the_option_it_names() {
        let prompt = request(offered());
        assert_eq!(
            prompt.choose('1').map(|id| id.0.to_string()).as_deref(),
            Some("allow")
        );
        assert_eq!(
            prompt.choose('3').map(|id| id.0.to_string()).as_deref(),
            Some("no")
        );
    }

    /// A stray keystroke must not be read as consent, and a digit past the
    /// end must not wrap onto an option the user did not aim at.
    #[test]
    fn an_unoffered_key_selects_nothing() {
        let prompt = request(offered());
        assert!(prompt.choose('q').is_none(), "a letter selects nothing");
        assert!(prompt.choose('0').is_none(), "there is no option zero");
        assert!(
            prompt.choose('4').is_none(),
            "a digit past the end must not wrap onto an allow option"
        );
    }

    /// Declining resolves to a reject option, never to an allow one.
    #[test]
    fn declining_never_resolves_to_allow() {
        let prompt = request(offered());
        assert_eq!(
            prompt.deny().map(|id| id.0.to_string()).as_deref(),
            Some("no")
        );

        // An agent that offered no way to say no gets no answer invented for
        // it: the caller cancels rather than picking an allow option.
        let allow_only = request(vec![option(
            "allow",
            "allow once",
            PermissionOptionKind::AllowOnce,
        )]);
        assert!(
            allow_only.deny().is_none(),
            "with no reject offered, declining must not select allow"
        );
    }

    /// The rule a cancelled turn depends on: a question nobody answered
    /// resolves to the agent's own reject option.
    ///
    /// Cancelling is not consenting. A turn can be cancelled with a permission
    /// outstanding — `Esc` or Ctrl-C while the agent is asking — and every such
    /// path answers it here rather than leaving it for whichever key happens to
    /// arrive next.
    #[test]
    fn an_unanswered_permission_takes_the_reject_option() {
        let chosen = match request(offered()).unanswered() {
            RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.0.to_string()),
            // `Cancelled`, or a variant added after this build — either way,
            // not a selection.
            _ => None,
        };
        assert_eq!(
            chosen.as_deref(),
            Some("no"),
            "the reject option, not the first one offered"
        );
    }

    /// And when the agent offered no way to say no, the answer is **cancelled**
    /// — never one of the options, because every option left says yes.
    #[test]
    fn an_unanswered_permission_never_resolves_to_consent() {
        let only_yes = request(vec![
            option("allow", "allow once", PermissionOptionKind::AllowOnce),
            option("always", "always allow", PermissionOptionKind::AllowAlways),
        ]);
        assert!(
            matches!(only_yes.unanswered(), RequestPermissionOutcome::Cancelled),
            "an unanswerable question must not become an allow"
        );

        // Nor when the agent offered nothing at all.
        assert!(matches!(
            request(Vec::new()).unanswered(),
            RequestPermissionOutcome::Cancelled
        ));
    }
}

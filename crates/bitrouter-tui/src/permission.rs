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
//!
//! # The headless answer
//!
//! A pipe has nobody to ask, and until now every headless path answered every
//! question with the reject option. [`Policy`] is the one rule a headless
//! caller states instead — approve everything, approve reads, deny everything,
//! and a per-tool override list — and [`Policy::decide`] applies it to a
//! [`Prompt`]. It chooses only among what the agent offered: a policy that
//! says *approve* against a request with no allow option still resolves to the
//! reject option, and reports that it denied, because the alternative is a
//! consent the agent never asked for.

use std::fmt;

use agent_client_protocol_schema::v1::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    SelectedPermissionOutcome, ToolKind,
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
    /// What kind of tool the agent said this is, when it said. The protocol
    /// calls it a display hint; a headless policy reads it as the only
    /// classification the wire carries.
    kind: Option<ToolKind>,
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
        kind: Option<ToolKind>,
        options: Vec<PermissionOption>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.unwrap_or_else(|| tool_call_id.into()),
            kind,
            options,
        }
    }

    /// Which request this prompt answers.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What the agent said it is about to do.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The tool kind the agent labelled the call with, if it labelled it.
    pub fn kind(&self) -> Option<ToolKind> {
        self.kind
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

    /// The option that says yes, when the agent offered one.
    ///
    /// Prefers *allow once* over *allow always*: a headless policy answers one
    /// request at a time, and standing consent is not something a flag on one
    /// invocation should hand out. `None` when every option offered says no,
    /// in which case the caller reports a denial rather than inventing consent.
    pub fn allow(&self) -> Option<PermissionOptionId> {
        let of = |kind: PermissionOptionKind| {
            self.options
                .iter()
                .find(|option| option.kind == kind)
                .map(|option| option.option_id.clone())
        };
        of(PermissionOptionKind::AllowOnce).or_else(|| of(PermissionOptionKind::AllowAlways))
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

    /// Resolve a policy's decision to an outcome the agent offered.
    ///
    /// Returns the decision that was *actually* taken beside the outcome: an
    /// approval against a request with no allow option falls back to
    /// [`Prompt::unanswered`] and reports [`Decision::Deny`], so a caller
    /// tallying its exit status counts what the agent heard, not what the
    /// policy meant.
    pub fn answer(&self, decision: Decision) -> (Decision, RequestPermissionOutcome) {
        match decision {
            Decision::Approve => match self.allow() {
                Some(id) => (
                    Decision::Approve,
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                ),
                None => (Decision::Deny, self.unanswered()),
            },
            Decision::Deny => (Decision::Deny, self.unanswered()),
        }
    }
}

/// What a headless policy decided about one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Select the agent's allow option.
    Approve,
    /// Select the agent's reject option.
    Deny,
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Decision::Approve => "approved",
            Decision::Deny => "denied",
        })
    }
}

/// The blanket rule a headless caller runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every request is approved.
    ApproveAll,
    /// Requests the agent labelled `read` or `search` are approved; every
    /// other kind, and an unlabelled request, is denied.
    ApproveReads,
    /// Every request is denied. The default: the tool kind is the harness's
    /// own label, and trusting it is opted into.
    #[default]
    DenyAll,
}

/// How a headless path answers permission requests.
///
/// The per-tool lists outrank the mode, and a deny outranks an approve, so a
/// request matched by both is denied. `default_action`, when set, answers the
/// requests no list matched instead of the mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// The rule for requests no list names.
    pub mode: Mode,
    /// Patterns that approve. See [`Policy::decide`] for what a pattern matches.
    pub auto_approve: Vec<String>,
    /// Patterns that deny. Outrank `auto_approve`.
    pub auto_deny: Vec<String>,
    /// The answer for an unmatched request, in place of the mode.
    pub default_action: Option<Decision>,
}

impl Policy {
    /// Decide one request.
    ///
    /// A pattern matches, case-insensitively, the tool kind's wire name
    /// (`read`, `execute`, …), the whole title, or the title's first word — so
    /// `Write` names every `Write src/main.rs` without naming a path.
    pub fn decide(&self, prompt: &Prompt) -> Decision {
        let matches = |patterns: &[String]| patterns.iter().any(|pattern| prompt.matches(pattern));
        if matches(&self.auto_deny) {
            return Decision::Deny;
        }
        if matches(&self.auto_approve) {
            return Decision::Approve;
        }
        if let Some(action) = self.default_action {
            return action;
        }
        match self.mode {
            Mode::ApproveAll => Decision::Approve,
            Mode::ApproveReads => match prompt.kind {
                Some(ToolKind::Read | ToolKind::Search) => Decision::Approve,
                _ => Decision::Deny,
            },
            Mode::DenyAll => Decision::Deny,
        }
    }
}

impl Prompt {
    /// Whether a policy pattern names this request.
    fn matches(&self, pattern: &str) -> bool {
        let title = self.title.trim();
        let head = title.split_whitespace().next().unwrap_or_default();
        self.kind
            .is_some_and(|kind| kind_name(kind).eq_ignore_ascii_case(pattern))
            || title.eq_ignore_ascii_case(pattern)
            || head.eq_ignore_ascii_case(pattern)
    }
}

/// The wire spelling of a tool kind, which is what a policy pattern names.
///
/// Spelled here rather than through `serde_json` so that matching a pattern
/// never allocates; pinned to the serialised form by a test.
fn kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        // `Other`, and any kind a later schema adds: nothing a pattern can name.
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(options: Vec<PermissionOption>) -> Prompt {
        Prompt::new(
            "r1",
            Some("Write src/main.rs".to_string()),
            "t1",
            Some(ToolKind::Edit),
            options,
        )
    }

    fn selected(outcome: &RequestPermissionOutcome) -> Option<String> {
        match outcome {
            RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.0.to_string()),
            _ => None,
        }
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

    /// Approving selects *allow once* first, so one invocation's consent is
    /// never standing consent; with only *always* on offer it takes that; and
    /// with nothing that says yes it selects nothing.
    #[test]
    fn allow_prefers_once_over_always_and_never_invents() {
        assert_eq!(
            request(offered())
                .allow()
                .map(|id| id.0.to_string())
                .as_deref(),
            Some("allow")
        );
        let always_only = request(vec![
            option("always", "always allow", PermissionOptionKind::AllowAlways),
            option("no", "reject", PermissionOptionKind::RejectOnce),
        ]);
        assert_eq!(
            always_only.allow().map(|id| id.0.to_string()).as_deref(),
            Some("always")
        );
        let reject_only = request(vec![option(
            "no",
            "reject",
            PermissionOptionKind::RejectOnce,
        )]);
        assert!(reject_only.allow().is_none());
    }

    /// The decision reported is the one the agent heard: an approval the
    /// agent offered no option for is a denial, not an approval that failed.
    #[test]
    fn an_approval_with_nothing_to_select_reports_deny() {
        let (decision, outcome) = request(offered()).answer(Decision::Approve);
        assert_eq!(decision, Decision::Approve);
        assert_eq!(selected(&outcome).as_deref(), Some("allow"));

        let reject_only = request(vec![option(
            "no",
            "reject",
            PermissionOptionKind::RejectOnce,
        )]);
        let (decision, outcome) = reject_only.answer(Decision::Approve);
        assert_eq!(decision, Decision::Deny);
        assert_eq!(selected(&outcome).as_deref(), Some("no"));

        let (decision, outcome) = request(Vec::new()).answer(Decision::Approve);
        assert_eq!(decision, Decision::Deny);
        assert!(matches!(outcome, RequestPermissionOutcome::Cancelled));
    }

    /// The three modes, against the kind the agent labelled the call with.
    #[test]
    fn approve_reads_reads_the_tool_kind() {
        let with =
            |kind: Option<ToolKind>| Prompt::new("r", Some("t".to_string()), "t1", kind, offered());
        let reads = Policy {
            mode: Mode::ApproveReads,
            ..Policy::default()
        };
        assert_eq!(reads.decide(&with(Some(ToolKind::Read))), Decision::Approve);
        assert_eq!(
            reads.decide(&with(Some(ToolKind::Search))),
            Decision::Approve
        );
        for not_a_read in [
            Some(ToolKind::Edit),
            Some(ToolKind::Execute),
            Some(ToolKind::Other),
            None,
        ] {
            assert_eq!(
                reads.decide(&with(not_a_read)),
                Decision::Deny,
                "{not_a_read:?}"
            );
        }
        let all = Policy {
            mode: Mode::ApproveAll,
            ..Policy::default()
        };
        assert_eq!(
            all.decide(&with(Some(ToolKind::Execute))),
            Decision::Approve
        );
        assert_eq!(
            Policy::default().decide(&with(Some(ToolKind::Read))),
            Decision::Deny
        );
    }

    /// The four layers, in order: a deny list outranks an approve list, which
    /// outranks the default action, which outranks the mode. Patterns name the
    /// kind, the title, or the title's first word, and case does not matter.
    #[test]
    fn auto_deny_outranks_auto_approve_outranks_default_outranks_mode() {
        let prompt = request(offered());
        let policy =
            |approve: &[&str], deny: &[&str], default: Option<Decision>, mode: Mode| Policy {
                mode,
                auto_approve: approve.iter().map(|s| s.to_string()).collect(),
                auto_deny: deny.iter().map(|s| s.to_string()).collect(),
                default_action: default,
            };
        // The title's first word, and the kind, both name it — and deny wins.
        assert_eq!(
            policy(&["write"], &["edit"], None, Mode::ApproveAll).decide(&prompt),
            Decision::Deny
        );
        // The approve list beats a deny default and a deny mode.
        assert_eq!(
            policy(
                &["Write src/main.rs"],
                &[],
                Some(Decision::Deny),
                Mode::DenyAll
            )
            .decide(&prompt),
            Decision::Approve
        );
        // The default action beats the mode.
        assert_eq!(
            policy(&[], &[], Some(Decision::Approve), Mode::DenyAll).decide(&prompt),
            Decision::Approve
        );
        // Nothing matched, no default: the mode answers.
        assert_eq!(
            policy(&["read"], &["execute"], None, Mode::DenyAll).decide(&prompt),
            Decision::Deny
        );
        // A pattern that is neither the kind, the title, nor its first word
        // matches nothing — a path fragment does not name a tool.
        assert_eq!(
            policy(&["main.rs"], &[], None, Mode::DenyAll).decide(&prompt),
            Decision::Deny
        );
    }

    /// The names a pattern may use are the wire's own.
    #[test]
    fn kind_names_are_the_wire_spelling() {
        for kind in [
            ToolKind::Read,
            ToolKind::Edit,
            ToolKind::Delete,
            ToolKind::Move,
            ToolKind::Search,
            ToolKind::Execute,
            ToolKind::Think,
            ToolKind::Fetch,
            ToolKind::SwitchMode,
            ToolKind::Other,
        ] {
            let wire = serde_json::to_value(kind).expect("a tool kind serialises");
            assert_eq!(wire.as_str(), Some(kind_name(kind)), "{kind:?}");
        }
    }
}

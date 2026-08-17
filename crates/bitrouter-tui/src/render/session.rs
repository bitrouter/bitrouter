//! The variants that are about the session rather than about a turn.
//!
//! All five already reach the renderer — `chat` subscribes to the *raw* update
//! stream, not the translated one — and all five used to die in a catch-all
//! arm. They land in two places, by what they are:
//!
//! | Variant | Where |
//! |---|---|
//! | `Plan` | the document, in order, patched in place like a tool call |
//! | `AvailableCommandsUpdate` | listed on request, because a list of commands is not a thing to keep on screen |
//! | `CurrentModeUpdate` | the footer |
//! | `ConfigOptionUpdate` | the footer |
//! | `SessionInfoUpdate` | the footer, as the title |
//!
//! `PlanUpdate` and `PlanRemoved` are absent on purpose: they sit behind the
//! schema's `unstable_plan_operations` feature, which this workspace does not
//! enable, so they do not exist in the compiled schema. `Plan` itself is
//! unconditional in v1.

use agent_client_protocol_schema::v1::{
    AvailableCommand, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionConfigKind,
    SessionConfigOption, SessionModeId,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// The agent's plan, as a block in document order.
///
/// A plan is patched far more often than it is created — an agent ticks its
/// way down one — which is exactly what the journal makes cheap: the block
/// keeps the place it first took and is repainted where it stands.
pub fn plan(plan: &Plan) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("plan · {} steps", plan.entries.len()),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.extend(plan.entries.iter().map(entry));
    lines
}

/// One step: what it is, how it is going, and how much it matters.
fn entry(entry: &PlanEntry) -> Line<'static> {
    let (glyph, style) = match entry.status {
        PlanEntryStatus::Completed => ("✓", Style::default().fg(Color::Green)),
        PlanEntryStatus::InProgress => ("◍", Style::default().fg(Color::Yellow)),
        PlanEntryStatus::Pending => ("·", Style::default().fg(Color::DarkGray)),
        // `PlanEntryStatus` is `#[non_exhaustive]`; an unknown state is
        // reported as unknown rather than shown as one of the three.
        _ => ("?", Style::default().fg(Color::Magenta)),
    };
    // Priority is only worth a mark when it is not the middle of three:
    // labelling everything "medium" is noise that hides the two that matter.
    let priority = match entry.priority {
        PlanEntryPriority::High => " (high)",
        PlanEntryPriority::Low => " (low)",
        _ => "",
    };
    Line::from(vec![
        Span::styled(format!("  {glyph} "), style),
        Span::raw(format!("{}{priority}", entry.content)),
    ])
}

/// The agent's own slash commands.
///
/// Listed when asked for rather than kept on screen: the list is static for
/// most of a session and long for some agents, and rows on screen are rows the
/// transcript does not get.
pub fn commands(commands: &[AvailableCommand]) -> Vec<Line<'static>> {
    if commands.is_empty() {
        return vec![Line::from(Span::styled(
            "this agent advertises no commands",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    let mut lines = vec![Line::from(Span::styled(
        format!("commands · {}", commands.len()),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.extend(commands.iter().map(|command| {
        Line::from(vec![
            Span::styled(
                format!("  /{}", command.name),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(format!("  {}", command.description)),
        ])
    }));
    lines
}

/// Mode, configuration, and title, as spans for the caller's footer row.
///
/// Spans rather than a row of their own, because the caller has its own things
/// to put there — what the session costs, where it is routed — and a footer
/// that grew a row per source would eat the transcript it summarizes.
pub fn state(
    mode: Option<&SessionModeId>,
    config: &[SessionConfigOption],
    title: Option<&str>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(mode) = mode {
        spans.push(Span::styled(
            format!(" · {mode}"),
            Style::default().fg(Color::Cyan),
        ));
    }
    for option in config {
        if let Some(value) = configured(option) {
            spans.push(Span::styled(
                format!(" · {}: {value}", option.name),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if let Some(title) = title {
        spans.push(Span::raw(format!(" · {title}")));
    }
    spans
}

/// What a configuration option is currently set to.
///
/// A selector reports the id it is on rather than the label: the label lives
/// in the option list, which the footer does not carry, and an id the user can
/// pass back to the agent beats a name they cannot.
fn configured(option: &SessionConfigOption) -> Option<String> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
        SessionConfigKind::Boolean(boolean) => {
            Some(if boolean.current_value { "on" } else { "off" }.to_string())
        }
        // `SessionConfigKind` is `#[non_exhaustive]`: a kind this build cannot
        // read the value of says nothing rather than guessing at one.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{
        SessionConfigBoolean, SessionConfigId, SessionConfigSelect, SessionConfigSelectOption,
        SessionConfigSelectOptions, SessionConfigValueId,
    };

    use super::*;

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn spans_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// `Plan` renders — every step, its state, and the two priorities worth
    /// marking.
    #[test]
    fn a_plan_renders_every_step_and_its_state() {
        let rendered = plan(&Plan::new(vec![
            PlanEntry::new(
                "write the wrap",
                PlanEntryPriority::High,
                PlanEntryStatus::Completed,
            ),
            PlanEntry::new(
                "port the tests",
                PlanEntryPriority::Medium,
                PlanEntryStatus::InProgress,
            ),
            PlanEntry::new(
                "delete the old renderer",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            ),
        ]));
        let out = text(&rendered);
        assert!(out.contains("plan · 3 steps"), "{out:?}");
        assert!(out.contains("✓ write the wrap (high)"), "{out:?}");
        assert!(
            out.contains("◍ port the tests\n"),
            "medium is unmarked, so the two that matter stand out: {out:?}"
        );
        assert!(out.contains("· delete the old renderer (low)"), "{out:?}");
    }

    /// `AvailableCommandsUpdate` renders — the surface that matters most,
    /// because `/route` is ours and everything else the agent offers was
    /// invisible.
    #[test]
    fn available_commands_render_with_their_descriptions() {
        let rendered = commands(&[
            AvailableCommand::new("compact", "summarize the conversation"),
            AvailableCommand::new("init", "write an AGENTS.md"),
        ]);
        let out = text(&rendered);
        assert!(out.contains("commands · 2"), "{out:?}");
        assert!(
            out.contains("/compact  summarize the conversation"),
            "{out:?}"
        );
        assert!(out.contains("/init  write an AGENTS.md"), "{out:?}");
    }

    /// An agent that advertises none says so, rather than rendering a heading
    /// over nothing.
    #[test]
    fn no_commands_says_so() {
        assert!(text(&commands(&[])).contains("no commands"));
    }

    /// `CurrentModeUpdate` renders, in the footer.
    #[test]
    fn the_current_mode_renders() {
        let spans = state(Some(&SessionModeId::new("plan")), &[], None);
        assert_eq!(spans_text(&spans), " · plan");
    }

    /// `ConfigOptionUpdate` renders, in the footer, with what each option is
    /// actually set to.
    #[test]
    fn config_options_render_with_their_current_values() {
        let config = vec![
            SessionConfigOption::new(
                SessionConfigId::new("thinking"),
                "Extended thinking",
                SessionConfigKind::Boolean(SessionConfigBoolean::new(true)),
            ),
            SessionConfigOption::new(
                SessionConfigId::new("model"),
                "Model",
                SessionConfigKind::Select(SessionConfigSelect::new(
                    SessionConfigValueId::new("opus"),
                    SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                        SessionConfigValueId::new("opus"),
                        "Opus",
                    )]),
                )),
            ),
        ];
        let out = spans_text(&state(None, &config, None));
        assert!(out.contains("Extended thinking: on"), "{out:?}");
        assert!(out.contains("Model: opus"), "{out:?}");
    }

    /// `SessionInfoUpdate` renders, as the title in the footer.
    #[test]
    fn the_session_title_renders() {
        let spans = state(None, &[], Some("porting the renderer"));
        assert_eq!(spans_text(&spans), " · porting the renderer");
    }

    /// All three footer sources in one row, in a fixed order, so the row does
    /// not reshuffle as updates arrive.
    #[test]
    fn the_footer_state_keeps_one_order() {
        let config = vec![SessionConfigOption::new(
            SessionConfigId::new("thinking"),
            "Extended thinking",
            SessionConfigKind::Boolean(SessionConfigBoolean::new(false)),
        )];
        let out = spans_text(&state(
            Some(&SessionModeId::new("build")),
            &config,
            Some("a title"),
        ));
        assert_eq!(out, " · build · Extended thinking: off · a title");
    }

    /// A session that has said nothing about itself adds nothing to the
    /// footer, rather than separators around blanks.
    #[test]
    fn an_unreported_state_renders_nothing() {
        assert!(state(None, &[], None).is_empty());
    }
}

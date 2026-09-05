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
    SessionConfigOption, SessionModeId, UsageUpdate,
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

/// What this session offers: BitRouter's own commands, then the agent's.
///
/// Listed when asked for rather than kept on screen: the list is static for
/// most of a session and long for some agents, and rows on screen are rows the
/// transcript does not get.
///
/// Two groups rather than one merged list, because the two are answered by
/// different things and a reader who types `/status` should be able to see
/// which one will get it. An agent command whose name BitRouter also uses is
/// listed and marked shadowed rather than dropped: the agent did advertise it,
/// and hiding it would make the resolver's precedence invisible.
pub fn commands(
    bitrouter: &[crate::machine::Command],
    agent: &[AvailableCommand],
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !bitrouter.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("bitrouter · {}", bitrouter.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.extend(bitrouter.iter().map(|command| {
            let mut spans = vec![
                Span::styled(
                    format!("  /{}", command.name),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(format!("  {}", command.summary)),
            ];
            // A control that cannot act says why, in place, rather than being
            // greyed out or absent.
            if let Some(reason) = command.unavailable {
                spans.push(Span::styled(
                    format!(" — {reason}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        }));
    }
    if agent.is_empty() {
        lines.push(Line::from(Span::styled(
            "this agent advertises no commands",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }
    lines.push(Line::from(Span::styled(
        format!("agent · {}", agent.len()),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.extend(agent.iter().map(|command| {
        let shadowed = bitrouter
            .iter()
            .any(|ours| ours.name == command.name.as_str());
        let mut spans = vec![
            Span::styled(
                format!("  /{}", command.name),
                Style::default().fg(if shadowed {
                    Color::DarkGray
                } else {
                    Color::Cyan
                }),
            ),
            Span::raw(format!("  {}", command.description)),
        ];
        if shadowed {
            spans.push(Span::styled(
                " — shadowed by BitRouter's",
                Style::default().fg(Color::DarkGray),
            ));
        }
        Line::from(spans)
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

/// When the context window is close enough to full that the reader should be
/// told. Below this the figure is background; at or above it, running out is
/// plausibly the next thing that happens to the session.
const CROWDED: f64 = 0.8;

/// How full the context window is, for the footer.
///
/// `used` and `size` are the harness's own — it owns the context window, and
/// this renderer never computes or adjusts them. Unlike cost, they need no
/// attribution: the number means the same thing whoever routed the traffic.
///
/// **Empty when there is nothing honest to say.** `UsageUpdate` is optional,
/// so a session may never carry one; and a `size` of zero is the harness
/// saying it does not know its own window. Either way the pair is the unit of
/// meaning — almost all the value here is *proximity to a limit*, so a used
/// figure with no window to measure it against is the same error as an
/// unscoped cost: a number the reader cannot act on. Half a pair is not drawn.
pub fn context(usage: Option<&UsageUpdate>) -> Vec<Span<'static>> {
    let Some(usage) = usage.filter(|usage| usage.size > 0) else {
        return Vec::new();
    };
    // Saturating rather than exact: a harness that reports more used than its
    // window holds is describing a full context, not a 120% one.
    let share = (usage.used as f64 / usage.size as f64).min(1.0);
    let style = if share >= CROWDED {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    vec![Span::styled(
        format!(" · ctx {}/{}", compact(usage.used), compact(usage.size)),
        style,
    )]
}

/// `12_400` → `12.4k`. The footer is one row shared with everything else the
/// session has to say, and what the reader wants from a token count is its
/// magnitude rather than its digits.
fn compact(tokens: u64) -> String {
    let (value, suffix) = if tokens >= 1_000_000 {
        (tokens as f64 / 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        (tokens as f64 / 1_000.0, "k")
    } else {
        return tokens.to_string();
    };
    // A trailing `.0` is noise at this width: `200k`, not `200.0k`.
    if (value.fract() * 10.0).round() == 0.0 {
        format!("{value:.0}{suffix}")
    } else {
        format!("{value:.1}{suffix}")
    }
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

    fn usage(used: u64, size: u64) -> UsageUpdate {
        UsageUpdate::new(used, size)
    }

    /// The rule this renderer shares with `cost`: a figure the reader cannot
    /// act on is not drawn. Almost all the value of an occupancy figure is
    /// proximity to a limit, so without a window there is nothing to say.
    #[test]
    fn half_a_pair_is_never_drawn() {
        assert!(context(None).is_empty(), "no usage at all");
        assert!(
            context(Some(&usage(12_400, 0))).is_empty(),
            "a harness that does not know its own window"
        );
        assert!(context(Some(&usage(0, 0))).is_empty(), "neither half known");
    }

    /// Both halves, and in the compact spelling the one-row footer needs.
    #[test]
    fn context_reports_the_pair() {
        assert_eq!(
            spans_text(&context(Some(&usage(12_400, 200_000)))),
            " · ctx 12.4k/200k"
        );
    }

    /// Below the threshold the figure is background; at or above it the reader
    /// is told, because running out is plausibly what happens next.
    #[test]
    fn a_crowded_window_is_flagged_and_a_roomy_one_is_not() {
        let roomy = context(Some(&usage(20_000, 200_000)));
        let crowded = context(Some(&usage(160_000, 200_000)));

        assert_eq!(
            roomy.first().map(|span| span.style.fg),
            Some(None),
            "a roomy window carries no warning colour"
        );
        assert_eq!(
            crowded.first().and_then(|span| span.style.fg),
            Some(Color::Yellow),
            "at {CROWDED:.0?} of the window the reader is told"
        );
    }

    /// A harness reporting more used than its window holds is describing a
    /// full context, not a 120% one — and must not panic or render one.
    #[test]
    fn an_overfull_window_saturates() {
        let over = context(Some(&usage(250_000, 200_000)));
        assert_eq!(spans_text(&over), " · ctx 250k/200k");
        assert_eq!(
            over.first().and_then(|span| span.style.fg),
            Some(Color::Yellow)
        );
    }

    /// The compact spelling: magnitude, not digits — and no trailing `.0`,
    /// which is pure noise at this width.
    #[test]
    fn compact_keeps_the_magnitude_and_drops_the_noise() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_500), "1.5k");
        assert_eq!(compact(12_400), "12.4k");
        assert_eq!(compact(200_000), "200k", "no trailing .0");
        assert_eq!(compact(1_500_000), "1.5M");
        assert_eq!(compact(2_000_000), "2M");
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

    /// One BitRouter command, offered and runnable.
    fn ours(name: &'static str, action: &'static str) -> crate::machine::Command {
        crate::machine::Command {
            name,
            action,
            summary: "does a thing",
            unavailable: None,
        }
    }

    /// `AvailableCommandsUpdate` renders — the surface that matters most,
    /// because `/route` is ours and everything else the agent offers was
    /// invisible.
    #[test]
    fn available_commands_render_with_their_descriptions() {
        let rendered = commands(
            &[],
            &[
                AvailableCommand::new("compact", "summarize the conversation"),
                AvailableCommand::new("init", "write an AGENTS.md"),
            ],
        );
        let out = text(&rendered);
        assert!(out.contains("agent · 2"), "{out:?}");
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
        assert!(text(&commands(&[], &[])).contains("no commands"));
    }

    /// BitRouter's own come first and are labelled as ours, so a reader can
    /// see which half of the list answers a name.
    #[test]
    fn bitrouter_commands_are_listed_above_the_agents() {
        let out = text(&commands(
            &[ours("route", "route_set")],
            &[AvailableCommand::new("compact", "summarize")],
        ));
        let mine = out.find("bitrouter · 1").expect("our heading");
        let theirs = out.find("agent · 1").expect("their heading");
        assert!(mine < theirs, "ours must be listed first: {out:?}");
    }

    /// A command that cannot act says why, in place — listed, never dead.
    #[test]
    fn an_unavailable_command_is_listed_with_its_reason() {
        let mut gated = ours("route", "route_set");
        gated.unavailable = Some("this session cannot be rerouted");
        let out = text(&commands(&[gated], &[]));
        assert!(out.contains("/route"), "{out:?}");
        assert!(out.contains("cannot be rerouted"), "{out:?}");
    }

    /// An agent command BitRouter also answers is shown, and marked, because
    /// the alternative is precedence the reader cannot see.
    #[test]
    fn a_shadowed_agent_command_is_marked_not_dropped() {
        let out = text(&commands(
            &[ours("status", "status")],
            &[
                AvailableCommand::new("status", "the agent's own status"),
                AvailableCommand::new("compact", "summarize"),
            ],
        ));
        assert!(out.contains("the agent's own status"), "listed: {out:?}");
        assert!(out.contains("shadowed"), "and marked: {out:?}");
        let shadow_marks = out.matches("shadowed").count();
        assert_eq!(shadow_marks, 1, "only the clashing one: {out:?}");
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

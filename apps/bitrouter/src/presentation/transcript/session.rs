//! The variants that are about the session rather than about a turn.
//!
//! All five already reach the renderer — `chat` subscribes to the *raw* update
//! stream, not the translated one — and all five used to die in a catch-all
//! arm. They land in two places, by what they are:
//!
//! | Variant | Where |
//! |---|---|
//! | `Plan` | the document, in order, patched in place like a tool call |
//! | `AvailableCommandsUpdate` | the journal holds it; the *app* renders it, through the report `bro acp commands` shares, so one session is not described two ways |
//! | `CurrentModeUpdate` | the active Code view |
//! | `ConfigOptionUpdate` | the active Code view |
//! | `SessionInfoUpdate` | the active Code view, as the title |
//!
//! `PlanUpdate` and `PlanRemoved` are absent on purpose: they sit behind the
//! schema's `unstable_plan_operations` feature, which this workspace does not
//! enable, so they do not exist in the compiled schema. `Plan` itself is
//! unconditional in v1.

use super::text::{Format, Row, Run, Tone};

use agent_client_protocol_schema::v1::{Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus};

/// The agent's plan, as a block in document order.
///
/// A plan is patched far more often than it is created — an agent ticks its
/// way down one — which is exactly what the journal makes cheap: the block
/// keeps the place it first took and is repainted where it stands.
pub fn plan(plan: &Plan) -> Vec<Row> {
    let mut lines = vec![Row::from(Run::annotated(
        format!("plan · {} steps", plan.entries.len()),
        Format::default().bold(),
    ))];
    lines.extend(plan.entries.iter().map(entry));
    lines
}

/// One step: what it is, how it is going, and how much it matters.
fn entry(entry: &PlanEntry) -> Row {
    let (glyph, style) = match entry.status {
        PlanEntryStatus::Completed => ("✓", Format::default().tone(Tone::Success)),
        PlanEntryStatus::InProgress => ("◍", Format::default().tone(Tone::Pending)),
        PlanEntryStatus::Pending => ("·", Format::default().tone(Tone::Muted)),
        // `PlanEntryStatus` is `#[non_exhaustive]`; an unknown state is
        // reported as unknown rather than shown as one of the three.
        _ => ("?", Format::default().tone(Tone::Unknown)),
    };
    // Priority is only worth a mark when it is not the middle of three:
    // labelling everything "medium" is noise that hides the two that matter.
    let priority = match entry.priority {
        PlanEntryPriority::High => " (high)",
        PlanEntryPriority::Low => " (low)",
        _ => "",
    };
    Row::from(vec![
        Run::annotated(format!("  {glyph} "), style),
        Run::raw(format!("{}{priority}", entry.content)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Row]) -> String {
        lines
            .iter()
            .map(|line| {
                line.runs
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

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
}

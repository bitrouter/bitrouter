//! The human view of the commands report.
//!
//! Grouped by source, in precedence order, so a reader can see which half of
//! the list answers a name before typing it.
//!
//! **No tabs.** These lines are rendered into the interactive session's notice
//! as well as to stdout, and the differential writer measures a row with
//! `unicode-width` — where a tab is one column — while a terminal advances the
//! cursor to the next tab stop. `plain_lines` expands what does arrive, but a
//! report rendered into a session should not be creating the problem.

use bitrouter_mcp::actions::commands::{CommandSource, CommandsReport};

use crate::output::CliReport;
use crate::output::human::Human;

impl CliReport for CommandsReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        for source in [
            CommandSource::Bitrouter,
            CommandSource::Config,
            CommandSource::Agent,
        ] {
            let rows: Vec<_> = self
                .commands
                .iter()
                .filter(|row| row.source == source)
                .collect();
            if rows.is_empty() {
                // The agent's absence is the one worth reporting: it is the
                // difference between "said none" and "has not said yet".
                if source == CommandSource::Agent {
                    h.note(if self.received {
                        "This agent advertises no commands."
                    } else {
                        "This agent had not sent its command list yet."
                    })?;
                }
                continue;
            }
            h.line(&format!("{} · {}", heading(source), rows.len()))?;
            for row in rows {
                let hint = row
                    .hint
                    .as_ref()
                    .map(|hint| format!(" <{hint}>"))
                    .unwrap_or_default();
                let mut line = format!("  /{}{hint}  {}", row.name, row.description);
                if let Some(reason) = &row.unavailable {
                    line.push_str(&format!(" — {reason}"));
                }
                if row.shadowed {
                    line.push_str(" — shadowed");
                }
                h.line(&line)?;
            }
        }
        Ok(())
    }
}

/// What each group is called on screen.
fn heading(source: CommandSource) -> &'static str {
    match source {
        CommandSource::Bitrouter => "bitrouter",
        CommandSource::Config => "config",
        CommandSource::Agent => "agent",
    }
}

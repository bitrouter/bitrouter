//! Bottom status bar.
//!
//! Layout (single row):
//!   `/ commands · ? help`                          [<spinner> ]<agent> · <model>
//!
//! No mode label, no listen-address, no clutter. The right-hand slot
//! shows the active session's agent and resolved model. When the
//! session is busy mid-turn, a leading spinner glyph animates.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::model::{AgentStatus, SessionStatus};

/// Spinner frames used for the activity indicator (Braille dots).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let left_text = " /  commands  ·  ?  help";
    let (right_text, right_style) = build_right(state);

    let left_span = Span::styled(left_text, Style::default().fg(Color::DarkGray));
    let left_width = left_span.width();

    let mut spans: Vec<Span<'static>> = vec![left_span];

    let right_width = right_text.width();
    let total = left_width + right_width;
    let pad = (area.width as usize).saturating_sub(total);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(right_text);
    let _ = right_style; // style baked into right_text already

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn build_right(state: &AppState) -> (Span<'static>, Style) {
    let active = state.session_store.active.get(state.active_session);

    let Some(session) = active else {
        // No active session — show a hint.
        let span = Span::styled("(no session)  ", Style::default().fg(Color::DarkGray));
        return (span, Style::default().fg(Color::DarkGray));
    };

    let agent_name = session.agent_id.clone();
    // Resolve model: read it off the agent config if exposed.
    // For v1 we don't yet pipe the resolved model through, so show
    // `<unset>` placeholder. The doc allows this and we'll wire it
    // up when the session-system surfaces the model id.
    let model = state
        .agents
        .iter()
        .find(|a| a.name == agent_name)
        .and_then(|a| a.config.as_ref())
        .and_then(|c| c.session.as_ref())
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "default".to_string());

    let (status_label, status_color) = match (&session.status, agent_status(state, &agent_name)) {
        (SessionStatus::Connecting, _) => ("connecting…".to_string(), Color::Cyan),
        (SessionStatus::Error(msg), _) => (format!("error: {msg}"), Color::Red),
        (_, Some(AgentStatus::Installing { percent })) => {
            (format!("installing {percent}%"), Color::Cyan)
        }
        _ => (format!("{agent_name} · {model}"), Color::White),
    };

    // Activity spinner when busy.
    let busy = matches!(session.status, SessionStatus::Busy);
    let prefix = if busy {
        let frame = SPINNER_FRAMES[spinner_index(state)];
        format!("{frame} ")
    } else {
        String::from("  ")
    };

    let span = Span::styled(
        format!("{prefix}{status_label} "),
        Style::default().fg(status_color),
    );
    (
        span,
        Style::default()
            .fg(status_color)
            .add_modifier(Modifier::DIM),
    )
}

fn agent_status<'a>(state: &'a AppState, agent_id: &str) -> Option<&'a AgentStatus> {
    state
        .agents
        .iter()
        .find(|a| a.name == agent_id)
        .map(|a| &a.status)
}

/// Pick the current spinner frame from a coarse system clock so the
/// glyph rotates without a dedicated tick task. Granularity is ~80ms.
fn spinner_index(_state: &AppState) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    ((now / 80) as usize) % SPINNER_FRAMES.len()
}

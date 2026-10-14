use super::{Screen, State, render};
use crate::writer::Writer;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::widgets::Paragraph;
use std::io::{self, IsTerminal};

#[derive(Default)]
struct Document {
    logical: Vec<ratatui::text::Line<'static>>,
    rows: Vec<ratatui::text::Line<'static>>,
    width: u16,
}

impl Document {
    fn draw(&mut self, state: &State, width: u16) -> &[ratatui::text::Line<'static>] {
        let refresh = matches!(state.screen, Screen::Goal | Screen::New) || self.logical.is_empty();
        if refresh {
            self.logical = render::document(state);
        }
        if refresh || self.width != width {
            self.rows = self
                .logical
                .iter()
                .flat_map(|row| crate::wrap::wrap(row, width.max(1)))
                .collect();
        }
        self.width = width;
        &self.rows
    }
}

pub struct View {
    writer: Writer<CrosstermBackend<io::Stdout>>,
    document: Document,
    scope: Option<String>,
    fullscreen: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

impl View {
    pub fn open() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "BRO Goals requires an interactive terminal",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        if let Err(error) = crate::lifecycle::enable_session_keys() {
            crate::lifecycle::restore();
            return Err(error);
        }
        match Writer::new(CrosstermBackend::new(io::stdout())).and_then(|mut writer| {
            writer.claim_full_height()?;
            Ok(writer)
        }) {
            Ok(writer) => Ok(Self {
                writer,
                document: Document::default(),
                scope: None,
                fullscreen: None,
            }),
            Err(error) => {
                crate::lifecycle::restore();
                Err(error)
            }
        }
    }

    pub fn draw(&mut self, state: &mut State) -> io::Result<()> {
        if state.fullscreen && matches!(state.screen, Screen::Workflow | Screen::Detail) {
            return self.draw_fullscreen(state);
        }
        self.close_fullscreen()?;
        let scope = if state.screen == Screen::New {
            Some("new".to_owned())
        } else {
            state.current.clone()
        };
        if self.scope != scope {
            self.writer.new_document()?;
            self.writer.claim_full_height()?;
            self.scope = scope;
            self.document = Document::default();
        }
        let size = self.writer.size();
        state.viewport = Some((size.width, size.height));
        let document = self.document.draw(state, size.width);
        let (footer, cursor) = render::footer(state, size.width, size.height);
        self.writer.docked_frame(document, &footer)?;
        self.writer.cursor(cursor.map(|position| {
            Position::new(
                position.x,
                size.height
                    .saturating_sub(u16::try_from(footer.len()).unwrap_or(u16::MAX))
                    .saturating_add(position.y),
            )
        }))
    }

    pub fn invalidate(&mut self) {
        if let Some(terminal) = &mut self.fullscreen {
            let _ = terminal.clear();
        }
        self.writer.invalidate();
    }
    pub fn suspend(&mut self) -> io::Result<()> {
        self.close_fullscreen()?;
        self.writer.finish()?;
        crate::lifecycle::restore();
        Ok(())
    }
    pub fn resume(&mut self) -> io::Result<()> {
        crate::lifecycle::enter_raw()?;
        crate::lifecycle::enable_session_keys()?;
        self.writer.invalidate();
        Ok(())
    }

    fn draw_fullscreen(&mut self, state: &mut State) -> io::Result<()> {
        if self.fullscreen.is_none() {
            self.writer.cursor(None)?;
            crate::lifecycle::enter_alternate_screen()?;
            match Terminal::new(CrosstermBackend::new(io::stdout())) {
                Ok(terminal) => self.fullscreen = Some(terminal),
                Err(error) => {
                    let _ = crate::lifecycle::leave_alternate_screen();
                    return Err(error);
                }
            }
        }
        let Some(terminal) = &mut self.fullscreen else {
            return Ok(());
        };
        terminal
            .draw(|frame| {
                let area = frame.area();
                state.viewport = Some((area.width, area.height));
                let (rows, cursor) = render::footer(state, area.width, area.height);
                frame.render_widget(Paragraph::new(rows), area);
                if let Some(position) = cursor {
                    frame.set_cursor_position(position);
                }
            })
            .map(|_| ())
    }

    fn close_fullscreen(&mut self) -> io::Result<()> {
        if self.fullscreen.take().is_some() {
            crate::lifecycle::leave_alternate_screen()?;
            self.writer.invalidate();
        }
        Ok(())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = self.close_fullscreen();
        let _ = self.writer.finish();
        crate::lifecycle::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsing_resize_defers_new_goal_content_until_return() {
        let mut state = State::new("/project".into());
        state.receive(super::super::Goal {
            id: "first".into(),
            title: "Observed Goal".into(),
            workspace: "/project".into(),
            ..Default::default()
        });
        state.observe("first".into());
        state.event_line(
            "first",
            "already visible content wraps at narrower widths".into(),
        );
        let mut document = Document::default();
        document.draw(&state, 80);
        for screen in [Screen::Goals, Screen::Workflow, Screen::Detail] {
            state.screen = screen;
            state.event_line("first", format!("DEFERRED_{screen:?}"));
            for width in [40, 100] {
                let rows = document.draw(&state, width);
                let text = rows.iter().map(ToString::to_string).collect::<String>();
                assert!(!text.contains("DEFERRED_"));
                assert!(
                    text.replace(' ', "")
                        .contains("alreadyvisiblecontentwrapsatnarrowerwidths")
                );
                assert!(rows.iter().all(|row| row.width() <= usize::from(width)));
            }
        }
        state.screen = Screen::Goal;
        let rows = document.draw(&state, 100);
        let text = rows.iter().map(ToString::to_string).collect::<String>();
        for screen in [Screen::Goals, Screen::Workflow, Screen::Detail] {
            assert_eq!(text.matches(&format!("DEFERRED_{screen:?}")).count(), 1);
        }
    }
}

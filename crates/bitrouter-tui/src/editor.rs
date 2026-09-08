//! A multiline prompt editor. Cursor offsets are grapheme boundaries; layout
//! and vertical movement share the same terminal-cell coordinates.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// What one key did to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Ignored,
    Changed,
    Redrawn,
    Submitted,
    Ended,
}

#[derive(Debug, Clone)]
pub struct Editor {
    line: String,
    cursor: usize,
    width: u16,
    preferred_column: Option<u16>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            line: String::new(),
            cursor: 0,
            width: 80,
            preferred_column: None,
        }
    }
}

/// Hard and soft line breaks, and the cursor's physical position in them.
pub struct InputLayout {
    pub rows: Vec<String>,
    pub cursor: (usize, u16),
    positions: Vec<(usize, usize, u16)>,
}

impl Editor {
    pub fn line(&self) -> &str {
        &self.line
    }

    pub fn set_width(&mut self, width: u16) {
        self.width = width.max(1);
    }

    pub fn clear(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.preferred_column = None;
    }

    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.line);
        self.clear();
        text
    }

    /// Lay out exactly as the composer paints: preserve whitespace and wrap
    /// at grapheme boundaries. A full row leaves room on the next for a cursor.
    pub fn layout(&self, width: u16) -> InputLayout {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut positions = Vec::new();
        let mut column = 0;
        for (index, grapheme) in self.line.grapheme_indices(true) {
            let cells = u16::try_from(grapheme.width()).unwrap_or(width).min(width);
            if grapheme == "\n" {
                // An explicit newline at a soft-wrap boundary consumes that
                // break once, instead of inventing a blank line in the prompt.
                let position = if column == width {
                    (rows.len(), 0)
                } else {
                    (rows.len() - 1, column)
                };
                positions.push((index, position.0, position.1));
                rows.push(String::new());
                column = 0;
                continue;
            }
            if column > 0 && column + cells > width {
                rows.push(String::new());
                column = 0;
            }
            positions.push((index, rows.len() - 1, column));
            if let Some(row) = rows.last_mut() {
                row.push_str(grapheme);
            }
            column += cells;
        }
        if column == width {
            rows.push(String::new());
            column = 0;
        }
        positions.push((self.line.len(), rows.len() - 1, column));
        let cursor = positions
            .iter()
            .find(|(byte, _, _)| *byte == self.cursor)
            .map_or((0, 0), |(_, row, column)| (*row, *column));
        InputLayout {
            rows,
            cursor,
            positions,
        }
    }

    pub fn apply(&mut self, key: KeyEvent) -> Edit {
        if key.kind == KeyEventKind::Release {
            return Edit::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.preferred_column = None;
        }
        match key.code {
            KeyCode::Char('c' | 'd') if ctrl => return Edit::Ended,
            KeyCode::Char('l') if ctrl => return Edit::Redrawn,
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert("\n")
            }
            KeyCode::Enter => return Edit::Submitted,
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if alt => self.delete_word(),
            KeyCode::Backspace => {
                let previous = self.previous();
                self.line.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            }
            KeyCode::Delete => {
                let next = self.next();
                self.line.replace_range(self.cursor..next, "");
            }
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Up => self.vertical(-1),
            KeyCode::Down => self.vertical(1),
            KeyCode::Home if ctrl => self.cursor = 0,
            KeyCode::End if ctrl => self.cursor = self.line.len(),
            KeyCode::Home | KeyCode::Char('a') if key.code == KeyCode::Home || ctrl => {
                self.cursor = self.line[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
            }
            KeyCode::End | KeyCode::Char('e') if key.code == KeyCode::End || ctrl => {
                self.cursor += self.line[self.cursor..]
                    .find('\n')
                    .unwrap_or(self.line.len() - self.cursor);
            }
            KeyCode::Char('u') if ctrl => {
                let start = self.line[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
                self.line.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => self.insert(&c.to_string()),
            _ => return Edit::Ignored,
        }
        Edit::Changed
    }

    /// Bracketed paste never submits. Normalize newlines, expand tabs, and
    /// discard terminal controls; the entire block remains editable.
    pub fn paste(&mut self, text: &str) {
        let text = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ");
        let text: String = text
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        self.insert(&text);
    }

    fn insert(&mut self, text: &str) {
        self.line.insert_str(self.cursor, text);
        self.cursor += text.len();
        // Insertion can join an adjacent combining mark / ZWJ sequence.
        self.cursor = self
            .line
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|i| *i >= self.cursor)
            .unwrap_or(self.line.len());
        self.preferred_column = None;
    }

    fn previous(&self) -> usize {
        self.line[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.line[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |s| self.cursor + s.len())
    }

    fn vertical(&mut self, delta: isize) {
        let layout = self.layout(self.width);
        let (row, column) = layout.cursor;
        let goal = *self.preferred_column.get_or_insert(column);
        let target = row.saturating_add_signed(delta).min(layout.rows.len() - 1);
        if let Some((byte, _, _)) = layout
            .positions
            .iter()
            .filter(|(_, row, _)| *row == target)
            .min_by_key(|(_, _, col)| col.abs_diff(goal))
        {
            self.cursor = *byte;
        }
    }

    fn delete_word(&mut self) {
        let prefix = &self.line[..self.cursor];
        let end = prefix.trim_end().len();
        let start = prefix[..end]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        self.line.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
}

/// Escape and Ctrl-C cancel a running turn.
pub fn is_cancel(event: &Event) -> bool {
    press(event).is_some_and(|key| {
        key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
    })
}

/// Is this Ctrl-L — redraw the screen?
///
/// The writer paints against its own model of the terminal and never asks the
/// terminal what it holds, so it cannot notice when something else writes
/// there. This is how a person tells it.
pub fn is_redraw(event: &Event) -> bool {
    press(event).is_some_and(|key| {
        key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l')
    })
}

/// The key event behind a press, if this event is one.
///
/// Public because the two modals read the raw key rather than going through
/// [`Editor::apply`], and a key *release* must not answer a permission
/// question any more than it may type a character.
pub fn press(event: &Event) -> Option<&KeyEvent> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            let _ = editor.apply(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_backspace_build_the_line() {
        let mut editor = Editor::default();
        typed(&mut editor, "hello");
        let _ = editor.apply(press(KeyCode::Backspace));
        assert_eq!(editor.line(), "hell");
        assert_eq!(editor.apply(press(KeyCode::Enter)), Edit::Submitted);
    }

    /// Taking the line leaves the editor reusable rather than spent.
    #[test]
    fn taking_the_line_empties_the_editor() {
        let mut editor = Editor::default();
        typed(&mut editor, "route to opus");
        assert_eq!(editor.take(), "route to opus");
        assert_eq!(editor.line(), "");
    }

    /// Raw mode means these two keys are ours; if they were not recognised
    /// here the session would have no way out at all.
    #[test]
    fn ctrl_c_and_ctrl_d_end_the_session() {
        let mut editor = Editor::default();
        typed(&mut editor, "half a thought");
        assert_eq!(editor.apply(ctrl('c')), Edit::Ended);
        assert_eq!(editor.apply(ctrl('d')), Edit::Ended);
    }

    /// A control chord must never be mistaken for the character it carries.
    #[test]
    fn ctrl_chords_do_not_type_their_letter() {
        let mut editor = Editor::default();
        let _ = editor.apply(ctrl('a'));
        assert_eq!(editor.line(), "");
    }

    #[test]
    fn word_delete_takes_the_word_and_its_space() {
        let mut editor = Editor::default();
        typed(&mut editor, "route to opus  ");
        let _ = editor.apply(ctrl('w'));
        assert_eq!(editor.line(), "route to ");
        let _ = editor.apply(ctrl('w'));
        assert_eq!(editor.line(), "route ");
    }

    /// The reason `delete_word` counts characters instead of bytes: a
    /// non-breaking space is whitespace and is two bytes wide, so a byte
    /// index one past it lands inside a character.
    #[test]
    fn word_delete_survives_multibyte_whitespace() {
        let mut editor = Editor::default();
        typed(&mut editor, "héllo\u{a0}wörld");
        editor.delete_word();
        assert_eq!(editor.line(), "héllo\u{a0}");
        editor.delete_word();
        assert_eq!(editor.line(), "");
    }

    #[test]
    fn word_delete_on_an_empty_buffer_is_harmless() {
        let mut editor = Editor::default();
        editor.delete_word();
        assert_eq!(editor.line(), "");
    }

    /// A pasted paragraph is one prompt, not the first line of one.
    #[test]
    fn paste_preserves_paragraphs_as_one_prompt() {
        let mut editor = Editor::default();
        editor.paste("first\nsecond\r\nthird");
        assert_eq!(editor.line(), "first\nsecond\nthird");
    }

    /// A key release must not double the keystroke.
    #[test]
    fn a_key_release_changes_nothing() {
        let mut editor = Editor::default();
        let mut release = press(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        assert_eq!(editor.apply(release), Edit::Ignored);
        assert_eq!(editor.line(), "");
    }

    /// Both keys the binding table gives to turn-cancel, and nothing else.
    #[test]
    fn cancel_is_ctrl_c_or_escape() {
        assert!(is_cancel(&Event::Key(ctrl('c'))));
        assert!(is_cancel(&Event::Key(press(KeyCode::Esc))));
        assert!(
            !is_cancel(&Event::Key(ctrl('d'))),
            "Ctrl-D ends the session"
        );
        assert!(!is_cancel(&Event::Key(press(KeyCode::Char('c')))));
        assert!(!is_cancel(&Event::Paste("c".to_string())));
    }

    #[test]
    fn redraw_is_ctrl_l_and_nothing_else() {
        assert!(is_redraw(&Event::Key(ctrl('l'))));
        assert!(!is_redraw(&Event::Key(press(KeyCode::Char('l')))));
        assert!(!is_redraw(&Event::Key(ctrl('c'))));
    }

    /// Ctrl-L asks for the screen, not for a change to the line.
    #[test]
    fn ctrl_l_leaves_the_line_alone() {
        let mut editor = Editor::default();
        typed(&mut editor, "half typed");
        assert_eq!(editor.apply(ctrl('l')), Edit::Redrawn);
        assert_eq!(editor.line(), "half typed");
    }
}

#[cfg(test)]
mod multiline_tests {
    use super::*;

    fn key(editor: &mut Editor, code: KeyCode) -> Edit {
        editor.apply(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn newlines_are_editable_and_only_plain_enter_submits() {
        let mut editor = Editor::default();
        editor.paste("first");
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert_eq!(
                editor.apply(KeyEvent::new(KeyCode::Enter, modifiers)),
                Edit::Changed
            );
        }
        editor.paste("last");
        assert_eq!(editor.line(), "first\n\nlast");
        key(&mut editor, KeyCode::Up);
        editor.paste("middle");
        assert_eq!(editor.line(), "first\nmiddle\nlast");
        key(&mut editor, KeyCode::Home);
        key(&mut editor, KeyCode::Delete);
        assert_eq!(editor.line(), "first\niddle\nlast");
        assert_eq!(key(&mut editor, KeyCode::Enter), Edit::Submitted);
        assert_eq!(editor.take(), "first\niddle\nlast");
        assert_eq!(editor.layout(10).cursor, (0, 0));
    }

    #[test]
    fn cursor_and_deletion_follow_graphemes_including_emoji() {
        let mut editor = Editor::default();
        editor.paste("中e\u{301}👩‍💻文");
        key(&mut editor, KeyCode::Left);
        key(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.line(), "中e\u{301}文");
        key(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.line(), "中文");
        editor.paste("🙂");
        assert_eq!(editor.line(), "中🙂文");
        assert_eq!(editor.layout(6).cursor, (0, 4));
        key(&mut editor, KeyCode::Delete);
        assert_eq!(editor.line(), "中🙂");
    }

    #[test]
    fn vertical_motion_follows_soft_wrap_and_restores_column() {
        let mut editor = Editor::default();
        editor.set_width(4);
        editor.paste("abcdefghij");
        assert_eq!(editor.layout(4).cursor, (2, 2));
        key(&mut editor, KeyCode::Up);
        editor.paste("!");
        assert_eq!(editor.line(), "abcdef!ghij");
        editor.clear();
        editor.paste("1234\nx\n1234");
        editor.set_width(10);
        key(&mut editor, KeyCode::Up);
        assert_eq!(editor.layout(10).cursor, (1, 1));
        key(&mut editor, KeyCode::Up);
        assert_eq!(editor.layout(10).cursor, (0, 4));
    }

    #[test]
    fn newline_at_a_full_row_does_not_add_a_phantom_blank_line() {
        let mut editor = Editor::default();
        editor.paste("abcd\nnext");
        assert_eq!(editor.layout(4).rows, ["abcd", "next", ""]);
        assert_eq!(editor.line(), "abcd\nnext");
    }

    #[test]
    fn every_cursor_stays_inside_its_row_at_narrow_widths() {
        for width in [1, 2, 3, 4, 8] {
            let mut editor = Editor::default();
            editor.paste("abcd\n中文👩‍💻 e\u{301}\n");
            for _ in 0..20 {
                let layout = editor.layout(width);
                assert!(layout.cursor.0 < layout.rows.len());
                assert!(
                    layout.cursor.1 < width,
                    "width {width}, cursor {:?}",
                    layout.cursor
                );
                key(&mut editor, KeyCode::Left);
            }
        }
    }
}

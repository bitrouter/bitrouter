//! The composer state machine, and what raw-terminal keys mean.
//!
//! Raw mode is not free. It means the terminal no longer echoes, no longer
//! assembles a line, and no longer turns Ctrl-C into a signal — all three
//! become the client's. [Editor] pays for the first two; [is_cancel] pays for
//! the third.
//!
//! Nothing here reads a terminal. [Editor::apply] is a state machine over
//! crossterm's already-decoded [KeyEvent], so who owns stdin and how events are
//! delivered stay the caller's. Keeping this module free of a runtime lets this
//! crate remain a synchronous library.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

/// The composer's insertion point.
///
/// The fields are zero-based logical positions. The column counts grapheme
/// clusters rather than terminal cells; renderers can calculate cells with
/// unicode-width while using [Editor::cursor_byte] to split text safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// The zero-based logical line.
    pub line: usize,
    /// The zero-based grapheme column in that line.
    pub column: usize,
}

/// What one key did to the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Nothing — a key this editor does not bind, or a key release.
    Ignored,
    /// The draft or insertion point changed and the screen should show it.
    Changed,
    /// The line is unchanged; what is stale is the terminal (Ctrl-L).
    Redrawn,
    /// Enter at an idle prompt. Blank submission policy belongs to the caller.
    Submitted,
    /// Ctrl-G asks the caller to open the configured external editor.
    OpenExternalEditor,
    /// Ctrl-D with a nonempty draft. The caller retains the draft and can
    /// explain that Ctrl-D exits only from an empty composer.
    ExitRequested,
    /// Ctrl-C or Ctrl-D from an empty composer. The session is over.
    Ended,
}

/// The text and cursor to restore after walking back down from history.
#[derive(Debug, Clone)]
struct Draft {
    text: String,
    cursor: usize,
}

/// One logical line in a draft.
///
/// The end excludes its line ending. The scanner consumes CRLF as one line
/// ending without changing the draft's exact text.
#[derive(Debug, Clone, Copy)]
struct LogicalLine {
    start: usize,
    end: usize,
}

/// An editable, multiline draft.
///
/// The cursor is always a Unicode grapheme boundary. History belongs to this
/// process only and is separate from take: callers record only prompts that
/// were actually accepted with [Editor::push_history].
#[derive(Debug, Default, Clone)]
pub struct Editor {
    text: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<Draft>,
    preferred_column: Option<usize>,
}

impl Editor {
    /// The complete draft, retained under its legacy name for existing callers.
    pub fn line(&self) -> &str {
        &self.text
    }

    /// The complete draft.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The insertion point as a logical line and grapheme column.
    pub fn cursor(&self) -> Cursor {
        let lines = self.logical_lines();
        let index = self.current_line_index(&lines);
        let line = lines[index];
        let column = self.text[line.start..self.cursor].graphemes(true).count();
        Cursor {
            line: index,
            column,
        }
    }

    /// The UTF-8 byte offset of the insertion point.
    ///
    /// This is safe for slicing [Editor::text] because the editor maintains a
    /// grapheme boundary after every edit.
    pub fn cursor_byte(&self) -> usize {
        self.cursor
    }

    /// Whether the draft contains no text.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replace the draft and put the insertion point at its end.
    ///
    /// Process-local history is preserved, but any in-progress history
    /// traversal ends because its saved draft no longer applies.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.reset_history_navigation();
        self.preferred_column = None;
    }

    /// Discard the draft, keeping the editor and its process-local history.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.reset_history_navigation();
        self.preferred_column = None;
    }

    /// Take the draft and leave an empty editor behind.
    ///
    /// This does not record history: callers may take text to resolve a local
    /// action, reject a submission, or enqueue it. Call [Editor::push_history]
    /// only after accepting a prompt for the process-local history.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.reset_history_navigation();
        self.preferred_column = None;
        text
    }

    /// Record one accepted prompt in process-local history.
    ///
    /// Text is retained exactly, including leading, trailing, and embedded
    /// whitespace. Empty entries are ignored because a blank submission is not
    /// a prompt.
    pub fn push_history(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() {
            self.history.push(text);
        }
        self.reset_history_navigation();
    }

    /// Apply one key.
    pub fn apply(&mut self, key: KeyEvent) -> Edit {
        // Key release events exist on some platforms; acting on both would
        // double every keystroke.
        if key.kind != KeyEventKind::Press {
            return Edit::Ignored;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Ctrl-C clears a draft but exits from an empty idle composer.
            KeyCode::Char('c') if ctrl => {
                if self.is_empty() {
                    Edit::Ended
                } else {
                    self.clear();
                    Edit::Changed
                }
            }
            // Ctrl-D never destroys a draft.
            KeyCode::Char('d') if ctrl => {
                if self.is_empty() {
                    Edit::Ended
                } else {
                    Edit::ExitRequested
                }
            }
            // The screen, not the draft: the buffer is untouched.
            KeyCode::Char('l') if ctrl => Edit::Redrawn,
            // The caller owns terminal suspend/restore and editor lookup.
            KeyCode::Char('g') if ctrl => Edit::OpenExternalEditor,
            // Ctrl-J is the fallback for terminals that cannot distinguish
            // Shift-Enter. Shift-Enter remains a normal multiline edit.
            KeyCode::Char('j') if ctrl => self.insert_text("\n"),
            KeyCode::Enter if (alt || key.modifiers.contains(KeyModifiers::SHIFT)) && !ctrl => {
                self.insert_text("\n")
            }
            KeyCode::Enter => Edit::Submitted,
            KeyCode::Home => {
                if ctrl {
                    self.moved_to(0)
                } else {
                    self.move_to_line_start()
                }
            }
            KeyCode::End => {
                if ctrl {
                    self.moved_to(self.text.len())
                } else {
                    self.move_to_line_end()
                }
            }
            KeyCode::Char('a') if ctrl => self.move_to_line_start(),
            KeyCode::Char('e') if ctrl => self.move_to_line_end(),
            KeyCode::Left if ctrl || alt => self.move_word_left(),
            KeyCode::Right if ctrl || alt => self.move_word_right(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Char('w') if ctrl => self.delete_word_before(),
            KeyCode::Backspace if ctrl || alt => self.delete_word_before(),
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete if ctrl || alt => self.delete_word_after(),
            KeyCode::Delete => self.delete_after_cursor(),
            KeyCode::Char(character) if !ctrl && !alt => self.insert_char(character),
            _ => Edit::Ignored,
        }
    }

    /// Insert a bracketed paste unchanged at the insertion point.
    ///
    /// Bracketed paste arrives whole, so its newlines cannot be mistaken for
    /// Enter presses. The text is neither flattened nor submitted here.
    pub fn paste(&mut self, text: &str) {
        let _ = self.insert_text(text);
    }

    fn insert_char(&mut self, character: char) -> Edit {
        let mut text = [0; 4];
        self.insert_text(character.encode_utf8(&mut text))
    }

    fn insert_text(&mut self, text: &str) -> Edit {
        if text.is_empty() {
            return Edit::Ignored;
        }
        let start = self.cursor;
        self.text.insert_str(start, text);
        self.cursor = start.saturating_add(text.len());
        self.normalize_cursor();
        self.changed_text()
    }

    fn move_left(&mut self) -> Edit {
        match self.previous_boundary(self.cursor) {
            Some(cursor) => self.moved_to(cursor),
            None => Edit::Ignored,
        }
    }

    fn move_right(&mut self) -> Edit {
        match self.next_boundary(self.cursor) {
            Some(cursor) => self.moved_to(cursor),
            None => Edit::Ignored,
        }
    }

    fn move_word_left(&mut self) -> Edit {
        self.moved_to(self.word_start_before(self.cursor))
    }

    fn move_word_right(&mut self) -> Edit {
        self.moved_to(self.word_end_after(self.cursor))
    }

    fn move_to_line_start(&mut self) -> Edit {
        let lines = self.logical_lines();
        let line = lines[self.current_line_index(&lines)];
        self.moved_to(line.start)
    }

    fn move_to_line_end(&mut self) -> Edit {
        let lines = self.logical_lines();
        let line = lines[self.current_line_index(&lines)];
        self.moved_to(line.end)
    }

    fn move_up(&mut self) -> Edit {
        let lines = self.logical_lines();
        let current = self.current_line_index(&lines);
        if current == 0 {
            return self.previous_history();
        }
        self.move_to_vertical_line(&lines, current - 1)
    }

    fn move_down(&mut self) -> Edit {
        let lines = self.logical_lines();
        let current = self.current_line_index(&lines);
        if current + 1 == lines.len() {
            return self.next_history();
        }
        self.move_to_vertical_line(&lines, current + 1)
    }

    fn move_to_vertical_line(&mut self, lines: &[LogicalLine], target: usize) -> Edit {
        let desired_column = self
            .preferred_column
            .unwrap_or_else(|| self.cursor().column);
        let line = lines[target];
        let cursor = Self::offset_at_column(&self.text, line, desired_column);
        if cursor == self.cursor {
            return Edit::Ignored;
        }
        self.cursor = cursor;
        self.preferred_column = Some(desired_column);
        Edit::Changed
    }

    fn delete_before_cursor(&mut self) -> Edit {
        let Some(start) = self.previous_boundary(self.cursor) else {
            return Edit::Ignored;
        };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.changed_text()
    }

    fn delete_after_cursor(&mut self) -> Edit {
        let Some(end) = self.next_boundary(self.cursor) else {
            return Edit::Ignored;
        };
        self.text.replace_range(self.cursor..end, "");
        self.changed_text()
    }

    fn delete_word_before(&mut self) -> Edit {
        let start = self.word_start_before(self.cursor);
        if start == self.cursor {
            return Edit::Ignored;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.changed_text()
    }

    fn delete_word_after(&mut self) -> Edit {
        let end = self.word_end_after(self.cursor);
        if end == self.cursor {
            return Edit::Ignored;
        }
        self.text.replace_range(self.cursor..end, "");
        self.changed_text()
    }

    fn changed_text(&mut self) -> Edit {
        self.reset_history_navigation();
        self.preferred_column = None;
        Edit::Changed
    }

    fn moved_to(&mut self, cursor: usize) -> Edit {
        if cursor == self.cursor {
            return Edit::Ignored;
        }
        self.cursor = cursor;
        self.preferred_column = None;
        Edit::Changed
    }

    fn previous_history(&mut self) -> Edit {
        let Some(index) = self.history_index else {
            let Some(index) = self.history.len().checked_sub(1) else {
                return Edit::Ignored;
            };
            self.history_draft = Some(Draft {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            return self.load_history(index);
        };
        let Some(previous) = index.checked_sub(1) else {
            return Edit::Ignored;
        };
        self.load_history(previous)
    }

    fn next_history(&mut self) -> Edit {
        let Some(index) = self.history_index else {
            return Edit::Ignored;
        };
        let next = index.saturating_add(1);
        if next < self.history.len() {
            return self.load_history(next);
        }
        let Some(draft) = self.history_draft.take() else {
            self.history_index = None;
            return Edit::Ignored;
        };
        self.text = draft.text;
        self.cursor = draft.cursor;
        self.normalize_cursor();
        self.history_index = None;
        self.preferred_column = None;
        Edit::Changed
    }

    fn load_history(&mut self, index: usize) -> Edit {
        let Some(text) = self.history.get(index) else {
            return Edit::Ignored;
        };
        self.text.clone_from(text);
        self.cursor = self.text.len();
        self.history_index = Some(index);
        self.preferred_column = None;
        Edit::Changed
    }

    fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    fn word_start_before(&self, position: usize) -> usize {
        let mut cursor = position;
        while let Some(start) = self.previous_boundary(cursor) {
            if !Self::is_whitespace_grapheme(&self.text[start..cursor]) {
                break;
            }
            cursor = start;
        }
        while let Some(start) = self.previous_boundary(cursor) {
            if Self::is_whitespace_grapheme(&self.text[start..cursor]) {
                break;
            }
            cursor = start;
        }
        cursor
    }

    fn word_end_after(&self, position: usize) -> usize {
        let mut cursor = position;
        while let Some(end) = self.next_boundary(cursor) {
            if !Self::is_whitespace_grapheme(&self.text[cursor..end]) {
                break;
            }
            cursor = end;
        }
        while let Some(end) = self.next_boundary(cursor) {
            if Self::is_whitespace_grapheme(&self.text[cursor..end]) {
                break;
            }
            cursor = end;
        }
        cursor
    }

    fn is_whitespace_grapheme(grapheme: &str) -> bool {
        grapheme.chars().all(char::is_whitespace)
    }

    fn previous_boundary(&self, position: usize) -> Option<usize> {
        self.text[..position]
            .grapheme_indices(true)
            .next_back()
            .map(|(start, _)| start)
    }

    fn next_boundary(&self, position: usize) -> Option<usize> {
        self.text[position..]
            .graphemes(true)
            .next()
            .map(|grapheme| position.saturating_add(grapheme.len()))
    }

    fn normalize_cursor(&mut self) {
        if self.cursor == self.text.len()
            || self
                .text
                .grapheme_indices(true)
                .any(|(start, _)| start == self.cursor)
        {
            return;
        }
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(start, _)| start)
            .find(|start| *start > self.cursor)
            .unwrap_or(self.text.len());
    }

    fn logical_lines(&self) -> Vec<LogicalLine> {
        let bytes = self.text.as_bytes();
        let mut lines = Vec::new();
        let mut start = 0;
        let mut index = 0;

        while index < bytes.len() {
            let line_end = match bytes[index] {
                b'\n' | b'\r' => Some(index),
                _ => None,
            };
            let Some(end) = line_end else {
                index = index.saturating_add(1);
                continue;
            };
            let mut next_start = index.saturating_add(1);
            if bytes[index] == b'\r' && bytes.get(next_start) == Some(&b'\n') {
                next_start = next_start.saturating_add(1);
            }
            lines.push(LogicalLine { start, end });
            start = next_start;
            index = next_start;
        }
        lines.push(LogicalLine {
            start,
            end: self.text.len(),
        });
        lines
    }

    fn current_line_index(&self, lines: &[LogicalLine]) -> usize {
        lines
            .iter()
            .position(|line| self.cursor >= line.start && self.cursor <= line.end)
            .unwrap_or_else(|| lines.len().saturating_sub(1))
    }

    fn offset_at_column(text: &str, line: LogicalLine, column: usize) -> usize {
        text[line.start..line.end]
            .graphemes(true)
            .take(column)
            .map(str::len)
            .fold(line.start, usize::saturating_add)
    }
}

/// Does this event cancel a running turn — Ctrl-C, or Esc?
///
/// In raw mode the terminal no longer sends SIGINT, so every consumer that
/// wants to be interruptible has to recognise the key itself. One predicate,
/// so they all agree on what it looks like.
///
/// Esc belongs here only when no modal is open: a modal owns the key stream
/// while it runs, so by the time an event reaches the turn loop there is
/// nothing else for Esc to close.
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
/// Public because modals read the raw key rather than going through
/// [Editor::apply], and a key release must not answer a permission question any
/// more than it may type a character.
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

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn ctrl(character: char) -> KeyEvent {
        modified(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        modified(code, KeyModifiers::ALT)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for character in text.chars() {
            let _ = editor.apply(press(KeyCode::Char(character)));
        }
    }

    #[test]
    fn typing_inserts_at_the_grapheme_cursor() {
        let mut editor = Editor::default();
        typed(&mut editor, "hllo");
        let _ = editor.apply(press(KeyCode::Left));
        let _ = editor.apply(press(KeyCode::Left));
        let _ = editor.apply(press(KeyCode::Left));
        assert_eq!(editor.apply(press(KeyCode::Char('e'))), Edit::Changed);
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 2 });
    }

    #[test]
    fn cursor_and_deletion_keep_extended_graphemes_whole() {
        let family = "👨‍👩‍👧‍👦";
        let accent = "e\u{301}";
        let mut editor = Editor::default();
        editor.set_text(format!("a{family}{accent}z"));

        let _ = editor.apply(press(KeyCode::Left));
        let _ = editor.apply(press(KeyCode::Left));
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 2 });
        assert_eq!(editor.apply(press(KeyCode::Backspace)), Edit::Changed);
        assert_eq!(editor.text(), format!("a{accent}z"));
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 1 });
    }

    #[test]
    fn word_navigation_and_deletion_respect_cursor_position() {
        let mut editor = Editor::default();
        editor.set_text("héllo\u{a0}wide world");
        assert_eq!(
            editor.apply(modified(KeyCode::Left, KeyModifiers::CONTROL)),
            Edit::Changed
        );
        assert_eq!(
            editor.cursor(),
            Cursor {
                line: 0,
                column: 11
            }
        );
        assert_eq!(editor.apply(alt(KeyCode::Backspace)), Edit::Changed);
        assert_eq!(editor.text(), "héllo\u{a0}world");
        assert_eq!(
            editor.apply(modified(KeyCode::Right, KeyModifiers::CONTROL)),
            Edit::Changed
        );
        let _ = editor.apply(press(KeyCode::Home));
        assert_eq!(
            editor.apply(modified(KeyCode::Delete, KeyModifiers::CONTROL)),
            Edit::Changed
        );
        assert_eq!(editor.text(), "\u{a0}world");
    }

    #[test]
    fn home_end_and_vertical_movement_follow_logical_lines() {
        let mut editor = Editor::default();
        editor.set_text("first\nxy\nthird");
        assert_eq!(editor.cursor(), Cursor { line: 2, column: 5 });
        let _ = editor.apply(press(KeyCode::Up));
        assert_eq!(editor.cursor(), Cursor { line: 1, column: 2 });
        let _ = editor.apply(press(KeyCode::Up));
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 5 });
        let _ = editor.apply(press(KeyCode::End));
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 5 });
        let _ = editor.apply(press(KeyCode::Home));
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 0 });
    }

    #[test]
    fn bracketed_paste_preserves_line_endings_and_never_submits() {
        let mut editor = Editor::default();
        editor.paste("first\r\nsecond\nthird");
        assert_eq!(editor.text(), "first\r\nsecond\nthird");
        assert_eq!(editor.cursor(), Cursor { line: 2, column: 5 });
        assert_eq!(editor.apply(press(KeyCode::Enter)), Edit::Submitted);
        assert_eq!(editor.text(), "first\r\nsecond\nthird");
    }

    #[test]
    fn modified_enter_and_ctrl_j_insert_newlines() {
        let mut editor = Editor::default();
        typed(&mut editor, "one");
        assert_eq!(
            editor.apply(modified(KeyCode::Enter, KeyModifiers::SHIFT)),
            Edit::Changed
        );
        typed(&mut editor, "two");
        assert_eq!(editor.apply(ctrl('j')), Edit::Changed);
        typed(&mut editor, "three");
        assert_eq!(editor.text(), "one\ntwo\nthree");
        assert_eq!(editor.cursor(), Cursor { line: 2, column: 5 });
        assert_eq!(
            editor.apply(modified(KeyCode::Enter, KeyModifiers::ALT)),
            Edit::Changed
        );
        assert_eq!(editor.text(), "one\ntwo\nthree\n");
    }

    #[test]
    fn history_activates_only_at_the_outer_logical_lines() {
        let mut editor = Editor::default();
        editor.push_history("first");
        editor.push_history("second\nentry");
        editor.set_text("draft");

        assert_eq!(editor.apply(press(KeyCode::Up)), Edit::Changed);
        assert_eq!(editor.text(), "second\nentry");
        assert_eq!(editor.cursor(), Cursor { line: 1, column: 5 });
        assert_eq!(editor.apply(press(KeyCode::Up)), Edit::Changed);
        assert_eq!(editor.cursor(), Cursor { line: 0, column: 5 });
        assert_eq!(editor.apply(press(KeyCode::Up)), Edit::Changed);
        assert_eq!(editor.text(), "first");
        assert_eq!(editor.apply(press(KeyCode::Down)), Edit::Changed);
        assert_eq!(editor.text(), "second\nentry");
        let _ = editor.apply(press(KeyCode::End));
        assert_eq!(editor.apply(press(KeyCode::Down)), Edit::Changed);
        assert_eq!(editor.text(), "draft");
    }

    #[test]
    fn changing_recalled_history_keeps_the_edit_and_leaves_navigation() {
        let mut editor = Editor::default();
        editor.push_history("previous");
        editor.set_text("draft");
        let _ = editor.apply(press(KeyCode::Up));
        let _ = editor.apply(press(KeyCode::Char('!')));
        assert_eq!(editor.text(), "previous!");
        assert_eq!(editor.apply(press(KeyCode::Down)), Edit::Ignored);
    }

    #[test]
    fn take_preserves_history_until_the_caller_records_a_submission() {
        let mut editor = Editor::default();
        editor.set_text("accepted");
        assert_eq!(editor.take(), "accepted");
        assert_eq!(editor.apply(press(KeyCode::Up)), Edit::Ignored);
        editor.push_history("accepted");
        assert_eq!(editor.apply(press(KeyCode::Up)), Edit::Changed);
        assert_eq!(editor.text(), "accepted");
    }

    #[test]
    fn idle_control_keys_preserve_or_clear_the_draft_as_specified() {
        let mut editor = Editor::default();
        typed(&mut editor, "recoverable");
        assert_eq!(editor.apply(ctrl('d')), Edit::ExitRequested);
        assert_eq!(editor.text(), "recoverable");
        assert_eq!(editor.apply(ctrl('c')), Edit::Changed);
        assert!(editor.is_empty());
        assert_eq!(editor.apply(ctrl('c')), Edit::Ended);
        typed(&mut editor, "external");
        assert_eq!(editor.apply(ctrl('g')), Edit::OpenExternalEditor);
        assert_eq!(editor.text(), "external");
    }

    #[test]
    fn a_key_release_changes_nothing() {
        let mut editor = Editor::default();
        let mut release = press(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        assert_eq!(editor.apply(release), Edit::Ignored);
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn cancel_is_ctrl_c_or_escape() {
        assert!(is_cancel(&Event::Key(ctrl('c'))));
        assert!(is_cancel(&Event::Key(press(KeyCode::Esc))));
        assert!(!is_cancel(&Event::Key(ctrl('d'))));
        assert!(!is_cancel(&Event::Key(press(KeyCode::Char('c')))));
        assert!(!is_cancel(&Event::Paste("c".to_string())));
    }

    #[test]
    fn redraw_is_ctrl_l_and_nothing_else() {
        assert!(is_redraw(&Event::Key(ctrl('l'))));
        assert!(!is_redraw(&Event::Key(press(KeyCode::Char('l')))));
        assert!(!is_redraw(&Event::Key(ctrl('c'))));
    }
}

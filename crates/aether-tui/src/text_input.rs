//! Single-line text input with a cursor. Used by every status-bar / overlay prompt: search,
//! save-as, file-browser new-file/new-directory, picker query. The struct owns the buffer and
//! the byte-offset cursor; methods keep them in sync and on UTF-8 char boundaries.
//!
//! Deref<Target=str> makes read-only string ops (`.is_empty()`, `.width()`, `format!("{}", …)`)
//! work without unwrapping. Mutating callers go through the methods so the cursor never lands
//! between code units.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fmt;
use std::ops::Deref;
use unicode_width::UnicodeWidthStr;

/// What a single key event meant for a status-bar prompt. The shared keymap (in
/// `apply_prompt_key`) handles all editing locally; this enum communicates the user-intent keys
/// (Enter/Esc) back to the caller so each prompt can run its own commit/cancel action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKeyOutcome {
    /// Key was an edit (char insert, cursor move, backspace) or an ignored key.
    Edited,
    /// Enter pressed — the caller should commit.
    Commit,
    /// Esc pressed — the caller should cancel/close.
    Cancel,
}

/// Shared keymap for every single-line prompt overlay (save-as, new-file, file-browser
/// new-file/new-directory). Returns `Commit`/`Cancel` for Enter/Esc; otherwise applies the edit
/// to `input` and returns `Edited`. Ignores Ctrl-/Alt-modified chars so the caller's parent
/// keymap can still match those as commands rather than text.
pub fn apply_prompt_key(input: &mut TextInput, k: KeyEvent) -> PromptKeyOutcome {
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => PromptKeyOutcome::Cancel,
        (KeyCode::Enter, _) => PromptKeyOutcome::Commit,
        (KeyCode::Left, _) => {
            input.move_left();
            PromptKeyOutcome::Edited
        }
        (KeyCode::Right, _) => {
            input.move_right();
            PromptKeyOutcome::Edited
        }
        (KeyCode::Backspace, _) => {
            input.backspace();
            PromptKeyOutcome::Edited
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            input.insert_char(c);
            PromptKeyOutcome::Edited
        }
        _ => PromptKeyOutcome::Edited,
    }
}

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    /// Underlying text. Exposed so callers that need a `&String` (e.g. for `mem::take`) can
    /// reach it; prefer the methods for cursor-aware edits.
    pub text: String,
    /// Byte offset into `text` of the insertion point. Invariant: lies on a char boundary.
    pub cursor: usize,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    /// Move the cursor one char left. No-op at the start.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut i = self.cursor - 1;
        while !self.text.is_char_boundary(i) {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move the cursor one char right. No-op at the end.
    pub fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let mut i = self.cursor + 1;
        while i < self.text.len() && !self.text.is_char_boundary(i) {
            i += 1;
        }
        self.cursor = i;
    }

    /// Delete the char immediately before the cursor and step back over it.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = {
            let mut i = self.cursor - 1;
            while !self.text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// Insert `c` at the cursor and advance past it.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Wipe text and cursor.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Take the text out, leaving an empty input. Mirrors `String::take` / `mem::take` for the
    /// underlying buffer specifically — cursor returns to 0.
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Replace contents wholesale; cursor parks at the end. Use when restoring from history or
    /// any other "set the whole string" path.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    /// Display width of the text up to (but not including) the cursor — the column offset of
    /// the caret on a single-row prompt.
    pub fn width_to_cursor(&self) -> usize {
        self.text[..self.cursor].width()
    }
}

impl Deref for TextInput {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for TextInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl From<String> for TextInput {
    fn from(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }
}

impl From<&str> for TextInput {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_parks_cursor_at_end() {
        let t = TextInput::new("hello");
        assert_eq!(t.cursor, 5);
    }

    #[test]
    fn insert_char_advances_cursor() {
        let mut t = TextInput::new("abc");
        t.cursor = 1;
        t.insert_char('X');
        assert_eq!(t.text, "aXbc");
        assert_eq!(t.cursor, 2);
    }

    #[test]
    fn backspace_deletes_char_before_cursor() {
        let mut t = TextInput::new("abc");
        t.cursor = 2;
        t.backspace();
        assert_eq!(t.text, "ac");
        assert_eq!(t.cursor, 1);
    }

    #[test]
    fn backspace_at_start_is_no_op() {
        let mut t = TextInput::new("abc");
        t.cursor = 0;
        t.backspace();
        assert_eq!(t.text, "abc");
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn left_and_right_clamp_at_bounds() {
        let mut t = TextInput::new("abc");
        t.cursor = 0;
        t.move_left();
        assert_eq!(t.cursor, 0);
        t.cursor = 3;
        t.move_right();
        assert_eq!(t.cursor, 3);
    }

    #[test]
    fn move_left_steps_one_char_across_multibyte() {
        // "é" is 2 bytes in UTF-8.
        let mut t = TextInput::new("aéb");
        assert_eq!(t.cursor, 4);
        t.move_left();
        assert_eq!(t.cursor, 3); // before 'b'
        t.move_left();
        assert_eq!(t.cursor, 1); // before 'é'
        t.move_left();
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn backspace_handles_multibyte_chars() {
        let mut t = TextInput::new("aé");
        assert_eq!(t.cursor, 3);
        t.backspace();
        assert_eq!(t.text, "a");
        assert_eq!(t.cursor, 1);
    }

    #[test]
    fn width_to_cursor_uses_display_width() {
        // "a" has width 1; full-width "あ" has display width 2.
        let mut t = TextInput::new("aあ");
        t.cursor = 1;
        assert_eq!(t.width_to_cursor(), 1);
        t.move_right();
        assert_eq!(t.width_to_cursor(), 3);
    }

    #[test]
    fn set_resets_cursor_to_end() {
        let mut t = TextInput::new("abc");
        t.cursor = 1;
        t.set("xyzzy");
        assert_eq!(t.text, "xyzzy");
        assert_eq!(t.cursor, 5);
    }

    #[test]
    fn take_text_clears_and_returns_string() {
        let mut t = TextInput::new("abc");
        t.cursor = 2;
        let s = t.take_text();
        assert_eq!(s, "abc");
        assert_eq!(t.text, "");
        assert_eq!(t.cursor, 0);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn apply_prompt_key_routes_enter_and_esc() {
        let mut t = TextInput::new("abc");
        assert_eq!(
            apply_prompt_key(&mut t, key(KeyCode::Enter)),
            PromptKeyOutcome::Commit
        );
        assert_eq!(
            apply_prompt_key(&mut t, key(KeyCode::Esc)),
            PromptKeyOutcome::Cancel
        );
        assert_eq!(t.text, "abc"); // untouched on Enter/Esc
    }

    #[test]
    fn apply_prompt_key_edits_text() {
        let mut t = TextInput::new("");
        apply_prompt_key(&mut t, key(KeyCode::Char('h')));
        apply_prompt_key(&mut t, key(KeyCode::Char('i')));
        assert_eq!(t.text, "hi");
        apply_prompt_key(&mut t, key(KeyCode::Backspace));
        assert_eq!(t.text, "h");
        apply_prompt_key(&mut t, key(KeyCode::Left));
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn apply_prompt_key_ignores_ctrl_chars() {
        // Ctrl-modified chars are claimed by the surrounding keymap, not the prompt input.
        let mut t = TextInput::new("");
        let k = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(apply_prompt_key(&mut t, k), PromptKeyOutcome::Edited);
        assert_eq!(t.text, "");
    }
}

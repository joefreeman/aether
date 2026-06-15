//! Single-line text input with a cursor — the shell-owned editor for overlay inputs (save-as,
//! search, picker query, project-settings, chip editor). The struct owns the buffer and the
//! byte-offset cursor; methods keep them in sync and on UTF-8 char boundaries.
//!
//! Text editing for overlays lives client-side (docs/client-core.md): the core owns values and
//! command keys, the shell owns text entry. `crate::overlay_input` drives this type from key
//! events and syncs the whole value into the core; the renderer reads its caret column.
//!
//! Deref<Target=str> makes read-only string ops (`.is_empty()`, `.width()`, `format!("{}", …)`)
//! work without unwrapping. Mutating callers go through the methods so the cursor never lands
//! between code units.

use std::fmt;
use std::ops::Deref;
use unicode_width::UnicodeWidthStr;

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

    /// Insert `s` at the cursor and advance past it.
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete the char immediately after the cursor (Delete key). No-op at the end.
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let mut end = self.cursor + 1;
        while end < self.text.len() && !self.text.is_char_boundary(end) {
            end += 1;
        }
        self.text.replace_range(self.cursor..end, "");
    }

    /// Move the cursor to the start of the text.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the text.
    pub fn end(&mut self) {
        self.cursor = self.text.len();
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
    fn insert_str_advances_cursor() {
        let mut t = TextInput::new("abc");
        t.cursor = 1;
        t.insert_str("XY");
        assert_eq!(t.text, "aXYbc");
        assert_eq!(t.cursor, 3);
    }

    #[test]
    fn delete_forward_removes_char_after_cursor() {
        let mut t = TextInput::new("aéb"); // "é" is 2 bytes
        t.cursor = 1;
        t.delete_forward(); // removes 'é'
        assert_eq!(t.text, "ab");
        assert_eq!(t.cursor, 1);
        t.cursor = 2;
        t.delete_forward(); // at end — no-op
        assert_eq!(t.text, "ab");
    }

    #[test]
    fn home_and_end_jump_to_bounds() {
        let mut t = TextInput::new("abc");
        t.home();
        assert_eq!(t.cursor, 0);
        t.end();
        assert_eq!(t.cursor, 3);
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

}

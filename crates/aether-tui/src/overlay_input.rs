//! Shell-owned text entry for overlay inputs (save-as, and — as later phases land — search,
//! picker query, workspace-settings, chip editor).
//!
//! The split (docs/client-core.md): the core owns *values* and value-derived semantics; the
//! shell owns *text-entry mechanics*. For a terminal there's no native input widget, so the shell
//! drives a [`TextInput`] locally for the focused field — caret, insert, delete — and syncs the
//! whole value into the core (`*_set_*`). Command keys (commit / cancel / nav / chord) are
//! forwarded to the core's keycode dispatch unchanged.
//!
//! This mirrors what the rich shells already do: iced renders a controlled `text_input` and the
//! web a native `<input>`, both syncing values to the same core setters. The TUI is the third
//! shell of that model; the difference is only that it has to do the editing itself.

use crate::text_input::TextInput;
use aether_client::keymap::{KeyCode, Mods};

/// Which overlay text field the shell-owned editor currently drives. Each maps to a core `*_set_*`
/// setter (see [`crate::shell`]'s `overlay_field_value` / `set_overlay_field`) and to a
/// `desired_overlay_field` rule that decides when it's focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayField {
    /// The save-as path prompt's path segment (`Prompt::SaveAs`).
    SaveAs,
    /// The save-as prompt's root-typeahead segment (multi-root workspaces only).
    SaveAsRoot,
    /// The open-from-path prompt's single path field (`Prompt::OpenPath`).
    OpenPath,
    /// The incremental-search query bar (`Mode::Search`).
    Search,
    /// The workspace-settings overlay's name field.
    WorkspaceName,
    /// The workspace-settings overlay's add-root input row.
    WorkspaceAddRoot,
    /// A picker's query input (Files/Buffers/Grep/Explorer/…).
    PickerQuery,
    /// The filter chip editor's root-typeahead segment (multi-root dir filters).
    ChipRoot,
    /// The filter chip editor's path/glob segment.
    ChipPath,
}

/// The shell-owned single-line editor for the focused overlay field. Held across frames so the
/// caret survives re-renders (the rest of the view model is rebuilt from the core each `sync`);
/// reseeded from the core's current value when the focused field changes.
pub struct OverlayEdit {
    pub field: OverlayField,
    pub input: TextInput,
}

/// How a key should be handled while an overlay editor is active.
pub enum KeyClass {
    /// A text-editing key — apply it to the local input; sync the value if the text changed.
    Text,
    /// A command key (commit / cancel / nav / chord) — forward to the core's keycode dispatch.
    Command,
}

/// Classify a key for an active overlay editor. Mirrors the web's `isEditingKey`: an unmodified
/// printable char or an in-field caret/delete key is text; everything else (including any
/// Ctrl/Alt chord) is a command the core owns.
pub fn classify(code: KeyCode, mods: Mods) -> KeyClass {
    if mods.ctrl || mods.alt {
        return KeyClass::Command;
    }
    match code {
        KeyCode::Char(_)
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End => KeyClass::Text,
        _ => KeyClass::Command,
    }
}

/// Field-specific overrides where a key [`classify`] calls *text* is actually a command the core
/// owns, so the shell forwards it instead of editing locally. `cursor` is the byte caret in the
/// field. Today only the picker query has any:
///
/// - `Delete` trashes the highlighted entry (Files/Explorer) — never a forward-delete;
/// - `Left` / `Backspace` at the query start step into the filter-chip row (the browser
///   tag-input gesture) rather than moving/deleting in an empty-to-the-left field.
pub fn is_command_override(field: OverlayField, code: KeyCode, cursor: usize) -> bool {
    match field {
        // `Left` / `Backspace` at the query start step into the filter-chip row (the browser
        // tag-input gesture). `Delete` is a plain forward-delete here — trashing a file is
        // `Ctrl-d` (a deliberate chord), never a bare editing key. The search bar's option chips
        // use the same gesture.
        OverlayField::PickerQuery | OverlayField::Search => match code {
            KeyCode::Left | KeyCode::Backspace => cursor == 0,
            _ => false,
        },
        // `:` on a root-typeahead segment confirms it and moves into the path (it can never
        // extend a root-label prefix), so it's a command, not text. The chip editor's root and the
        // save-as prompt's root share this gesture.
        OverlayField::ChipRoot | OverlayField::SaveAsRoot => code == KeyCode::Char(':'),
        // Backspace at the path-segment start steps back into the root field (multi-root) — the
        // same leftward gesture the chip row uses from the query.
        OverlayField::ChipPath | OverlayField::SaveAs => code == KeyCode::Backspace && cursor == 0,
        _ => false,
    }
}

/// Apply a text key to `input`. Returns `true` when the *text* changed (so the caller syncs the
/// new value to the core); caret-only moves return `false`. `text` carries the typed grapheme(s)
/// for a `Char` key (already case-correct; control chars are filtered here).
pub fn apply_text_key(input: &mut TextInput, code: KeyCode, text: Option<String>) -> bool {
    let before = input.text.clone();
    match code {
        KeyCode::Char(_) => {
            if let Some(t) = text {
                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                input.insert_str(&t);
            }
        }
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete_forward(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => {}
    }
    input.text != before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(code: KeyCode) -> bool {
        matches!(classify(code, Mods::default()), KeyClass::Command)
    }

    #[test]
    fn text_keys_vs_command_keys() {
        // Unmodified text-entry keys are owned by the field.
        for code in [
            KeyCode::Char('a'),
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert!(!cmd(code), "{code:?} should be a text key");
        }
        // Commit / cancel / nav are commands.
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Tab, KeyCode::Up] {
            assert!(cmd(code), "{code:?} should be a command");
        }
        // Any chord is a command, even over a text key (Ctrl-W word-delete etc. belong to the core
        // / outer keymap, not the field).
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        assert!(matches!(
            classify(KeyCode::Char('w'), ctrl),
            KeyClass::Command
        ));
        assert!(matches!(classify(KeyCode::Left, ctrl), KeyClass::Command));
    }

    #[test]
    fn picker_query_command_overrides() {
        use OverlayField::{PickerQuery, SaveAs};
        // Left / Backspace are commands only at the query start (step into the chip row).
        assert!(is_command_override(PickerQuery, KeyCode::Left, 0));
        assert!(is_command_override(PickerQuery, KeyCode::Backspace, 0));
        assert!(!is_command_override(PickerQuery, KeyCode::Left, 3));
        assert!(!is_command_override(PickerQuery, KeyCode::Backspace, 3));
        // Delete is a plain forward-delete in the query (trashing is Ctrl-d, a chord handled by the
        // core, not an override); chars and Right are never overridden either.
        assert!(!is_command_override(PickerQuery, KeyCode::Delete, 5));
        assert!(!is_command_override(PickerQuery, KeyCode::Char('a'), 0));
        assert!(!is_command_override(PickerQuery, KeyCode::Right, 0));
        // The save-as path segment steps into the root field on Backspace at the start (its only
        // override); Delete / Left there are plain editing keys.
        assert!(is_command_override(SaveAs, KeyCode::Backspace, 0));
        assert!(!is_command_override(SaveAs, KeyCode::Backspace, 3));
        assert!(!is_command_override(SaveAs, KeyCode::Delete, 0));
        assert!(!is_command_override(SaveAs, KeyCode::Left, 0));
    }

    #[test]
    fn apply_reports_text_change_but_not_caret_moves() {
        let mut input = TextInput::new("ab");
        // Insert at end.
        assert!(apply_text_key(
            &mut input,
            KeyCode::Char('c'),
            Some("c".into())
        ));
        assert_eq!(input.text, "abc");
        // Caret moves don't change text.
        assert!(!apply_text_key(&mut input, KeyCode::Left, None));
        assert!(!apply_text_key(&mut input, KeyCode::Home, None));
        assert_eq!(input.cursor, 0);
        // Delete-forward at start removes the first char.
        assert!(apply_text_key(&mut input, KeyCode::Delete, None));
        assert_eq!(input.text, "bc");
        // Backspace at start is a no-op (no text change).
        assert!(!apply_text_key(&mut input, KeyCode::Backspace, None));
        assert_eq!(input.text, "bc");
        // Control chars in the typed text are dropped (no insert).
        assert!(!apply_text_key(
            &mut input,
            KeyCode::Char('\u{7}'),
            Some("\u{7}".into())
        ));
        assert_eq!(input.text, "bc");
    }
}

//! The iced shell's input edge: mapping `iced::keyboard` events onto the core's key types.
//! This is the only place iced key types and the core keymap meet.

use crate::keymap::{self, KeyCode, Mods};

/// Map iced modifiers onto the core's [`Mods`]. A free function, not `From` — both types
/// are foreign here now that the keymap lives in `aether-client` (orphan rule).
pub fn mods(m: iced::keyboard::Modifiers) -> Mods {
    Mods {
        ctrl: m.control(),
        alt: m.alt(),
        shift: m.shift(),
    }
}

/// Pick which logical key to resolve a binding against, then normalise it.
///
/// The base/modified selection rule (and its macOS Option-composition rationale) lives in the core
/// so every shell resolves Alt-chords identically — see [`keymap::keycode_for_binding`]. Here we
/// just normalise iced's base key (`key`, sourced from winit's `key_without_modifiers()`) and its
/// modified key, then hand both to the shared rule.
pub fn keycode_for_binding(
    key: &iced::keyboard::Key,
    modified_key: &iced::keyboard::Key,
    alt: bool,
) -> Option<KeyCode> {
    keymap::keycode_for_binding(keycode(key), keycode(modified_key), alt)
}

/// Normalise an iced key to the core's [`KeyCode`]. `None` for keys we don't bind
/// (modifiers themselves, function keys, …).
pub fn keycode(key: &iced::keyboard::Key) -> Option<KeyCode> {
    use iced::keyboard::key::Named;
    use iced::keyboard::Key;
    Some(match key {
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c.to_ascii_lowercase())
        }
        Key::Named(named) => match named {
            Named::Space => KeyCode::Char(' '),
            Named::Escape => KeyCode::Esc,
            Named::Enter => KeyCode::Enter,
            Named::Tab => KeyCode::Tab,
            Named::Backspace => KeyCode::Backspace,
            Named::Delete => KeyCode::Delete,
            Named::Home => KeyCode::Home,
            Named::End => KeyCode::End,
            Named::PageUp => KeyCode::PageUp,
            Named::PageDown => KeyCode::PageDown,
            Named::ArrowLeft => KeyCode::Left,
            Named::ArrowRight => KeyCode::Right,
            Named::ArrowUp => KeyCode::Up,
            Named::ArrowDown => KeyCode::Down,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::{key::Named, Key};

    #[test]
    fn keycode_normalises_letters_and_named_keys() {
        assert_eq!(
            keycode(&Key::Character("H".into())),
            Some(KeyCode::Char('h'))
        );
        assert_eq!(
            keycode(&Key::Character("?".into())),
            Some(KeyCode::Char('?'))
        );
        assert_eq!(keycode(&Key::Named(Named::Space)), Some(KeyCode::Char(' ')));
        assert_eq!(keycode(&Key::Named(Named::Escape)), Some(KeyCode::Esc));
        assert_eq!(keycode(&Key::Named(Named::Shift)), None);
    }

    #[test]
    fn alt_chord_uses_base_key_not_macos_composed_glyph() {
        // macOS delivers Option-f as base `f` + modified `ƒ`. With Alt held we must bind on the
        // base key, or the chord never matches.
        let base = Key::Character("f".into());
        let composed = Key::Character("ƒ".into());
        assert_eq!(
            keycode_for_binding(&base, &composed, true),
            Some(KeyCode::Char('f'))
        );
    }

    #[test]
    fn non_alt_key_uses_modified_key_for_shifted_symbols() {
        // No Alt: honour composition so Shift-/ resolves to `?`, not the base `/`.
        let base = Key::Character("/".into());
        let modified = Key::Character("?".into());
        assert_eq!(
            keycode_for_binding(&base, &modified, false),
            Some(KeyCode::Char('?'))
        );
    }
}

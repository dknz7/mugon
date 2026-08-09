//! VK <-> human-readable name mapping.
//!
//! DESIGN.md §4.4: names MUST come from this table, never from
//! `GetKeyNameTextW`. That API derives labels from scancodes, and F13-F24 have
//! non-standard scancodes — a remapper emitting a synthetic or zeroed scancode
//! makes it return an empty string.

use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Named keys. Letters and digits are handled arithmetically in `vk_to_name`
/// because Win32 defines no `VK_A` / `VK_0` constants — they are raw ASCII.
static NAMED: &[(VIRTUAL_KEY, &str)] = &[
    // Extended function keys first — the reason this table exists.
    (VK_F13, "F13"), (VK_F14, "F14"), (VK_F15, "F15"), (VK_F16, "F16"),
    (VK_F17, "F17"), (VK_F18, "F18"), (VK_F19, "F19"), (VK_F20, "F20"),
    (VK_F21, "F21"), (VK_F22, "F22"), (VK_F23, "F23"), (VK_F24, "F24"),
    // Standard function keys
    (VK_F1, "F1"), (VK_F2, "F2"), (VK_F3, "F3"), (VK_F4, "F4"),
    (VK_F5, "F5"), (VK_F6, "F6"), (VK_F7, "F7"), (VK_F8, "F8"),
    (VK_F9, "F9"), (VK_F10, "F10"), (VK_F11, "F11"), (VK_F12, "F12"),
    // Modifiers (side-specific)
    (VK_LCONTROL, "LeftCtrl"), (VK_RCONTROL, "RightCtrl"),
    (VK_LMENU, "LeftAlt"), (VK_RMENU, "RightAlt"),
    (VK_LSHIFT, "LeftShift"), (VK_RSHIFT, "RightShift"),
    (VK_LWIN, "LeftWin"), (VK_RWIN, "RightWin"),
    // Locks
    (VK_CAPITAL, "CapsLock"), (VK_SCROLL, "ScrollLock"), (VK_NUMLOCK, "NumLock"),
    // Navigation and editing
    (VK_UP, "Up"), (VK_DOWN, "Down"), (VK_LEFT, "Left"), (VK_RIGHT, "Right"),
    (VK_HOME, "Home"), (VK_END, "End"),
    (VK_PRIOR, "PageUp"), (VK_NEXT, "PageDown"),
    (VK_INSERT, "Insert"), (VK_DELETE, "Delete"),
    (VK_BACK, "Backspace"), (VK_TAB, "Tab"), (VK_RETURN, "Enter"),
    (VK_SPACE, "Space"), (VK_PAUSE, "Pause"), (VK_SNAPSHOT, "PrintScreen"),
    // Numpad
    (VK_NUMPAD0, "Numpad0"), (VK_NUMPAD1, "Numpad1"), (VK_NUMPAD2, "Numpad2"),
    (VK_NUMPAD3, "Numpad3"), (VK_NUMPAD4, "Numpad4"), (VK_NUMPAD5, "Numpad5"),
    (VK_NUMPAD6, "Numpad6"), (VK_NUMPAD7, "Numpad7"), (VK_NUMPAD8, "Numpad8"),
    (VK_NUMPAD9, "Numpad9"),
    (VK_MULTIPLY, "NumpadMultiply"), (VK_ADD, "NumpadAdd"),
    (VK_SUBTRACT, "NumpadSubtract"), (VK_DECIMAL, "NumpadDecimal"),
    (VK_DIVIDE, "NumpadDivide"),
    // Media and browser
    (VK_MEDIA_PLAY_PAUSE, "MediaPlayPause"), (VK_MEDIA_STOP, "MediaStop"),
    (VK_MEDIA_NEXT_TRACK, "MediaNext"), (VK_MEDIA_PREV_TRACK, "MediaPrev"),
    (VK_VOLUME_MUTE, "VolumeMute"), (VK_VOLUME_UP, "VolumeUp"),
    (VK_VOLUME_DOWN, "VolumeDown"),
    (VK_BROWSER_BACK, "BrowserBack"), (VK_BROWSER_FORWARD, "BrowserForward"),
    (VK_BROWSER_REFRESH, "BrowserRefresh"), (VK_BROWSER_HOME, "BrowserHome"),
];

pub fn vk_to_name(vk: u16) -> Option<&'static str> {
    if let Some((_, n)) = NAMED.iter().find(|(k, _)| k.0 == vk) {
        return Some(n);
    }
    // A-Z (0x41-0x5A) and 0-9 (0x30-0x39) are raw ASCII.
    if (0x41..=0x5A).contains(&vk) || (0x30..=0x39).contains(&vk) {
        return Some(ascii_name(vk));
    }
    None
}

/// Returns a `'static` single-character name for an ASCII alphanumeric VK.
fn ascii_name(vk: u16) -> &'static str {
    static CHARS: &str = "0123456789";
    static LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    if (0x30..=0x39).contains(&vk) {
        let i = (vk - 0x30) as usize;
        &CHARS[i..i + 1]
    } else {
        let i = (vk - 0x41) as usize;
        &LETTERS[i..i + 1]
    }
}

pub fn name_to_vk(name: &str) -> Option<u16> {
    if let Some((k, _)) = NAMED.iter().find(|(_, n)| *n == name) {
        return Some(k.0);
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_uppercase() || c.is_ascii_digit() => Some(c as u16),
        _ => None,
    }
}

pub fn is_modifier(vk: u16) -> bool {
    matches!(
        VIRTUAL_KEY(vk),
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL
            | VK_MENU | VK_LMENU | VK_RMENU
            | VK_SHIFT | VK_LSHIFT | VK_RSHIFT
            | VK_LWIN | VK_RWIN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The F13-F24 requirement. Every extended function key must resolve to a
    /// non-empty label and round-trip back to the same VK. See DESIGN.md §4.4.
    #[test]
    fn extended_function_keys_all_resolve() {
        for vk in 0x7Cu16..=0x87u16 {
            let name = vk_to_name(vk)
                .unwrap_or_else(|| panic!("VK {vk:#04X} has no name"));
            assert!(!name.is_empty(), "VK {vk:#04X} resolved to empty string");
            assert_eq!(name_to_vk(name), Some(vk), "round-trip failed for {name}");
        }
    }

    #[test]
    fn f13_through_f24_are_named_correctly() {
        assert_eq!(vk_to_name(0x7C), Some("F13"));
        assert_eq!(vk_to_name(0x83), Some("F20"));
        assert_eq!(vk_to_name(0x87), Some("F24"));
    }

    #[test]
    fn f12_and_f13_are_adjacent() {
        assert_eq!(vk_to_name(0x7B), Some("F12"));
        assert_eq!(vk_to_name(0x7C), Some("F13"));
    }

    #[test]
    fn letters_and_digits_resolve() {
        assert_eq!(vk_to_name(0x41), Some("A"));
        assert_eq!(vk_to_name(0x5A), Some("Z"));
        assert_eq!(vk_to_name(0x30), Some("0"));
        assert_eq!(name_to_vk("A"), Some(0x41));
    }

    #[test]
    fn lock_navigation_and_media_keys_resolve() {
        for name in ["CapsLock", "ScrollLock", "NumLock", "Home", "PageUp",
                     "Insert", "Delete", "Up", "Numpad0", "MediaPlayPause",
                     "VolumeMute", "BrowserBack"] {
            let vk = name_to_vk(name).unwrap_or_else(|| panic!("{name} not mapped"));
            assert_eq!(vk_to_name(vk), Some(name));
        }
    }

    #[test]
    fn modifiers_are_identified() {
        for name in ["LeftCtrl", "RightCtrl", "LeftAlt", "RightAlt",
                     "LeftShift", "RightShift", "LeftWin", "RightWin"] {
            let vk = name_to_vk(name).unwrap();
            assert!(is_modifier(vk), "{name} should be a modifier");
        }
        assert!(!is_modifier(0x7C), "F13 should not be a modifier");
    }
}

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
    // Punctuation. Named for the **unshifted US character on the keycap**,
    // which is what the picker shows and what the config file records. The VK
    // is what the hook matches on, so a binding follows the physical key rather
    // than the character: on a layout where VK_OEM_1 is not `;`, the label is
    // wrong but the binding still fires on the same key. Same US-centric
    // assumption the letter and digit handling above already makes.
    (VK_OEM_1, ";"), (VK_OEM_PLUS, "="), (VK_OEM_COMMA, ","),
    (VK_OEM_MINUS, "-"), (VK_OEM_PERIOD, "."), (VK_OEM_2, "/"),
    (VK_OEM_3, "`"), (VK_OEM_4, "["), (VK_OEM_5, "\\"),
    (VK_OEM_6, "]"), (VK_OEM_7, "'"),
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

/// The picker's `<optgroup>` slots, as indices into `bindable_groups`'s `ORDER`.
///
/// Named constants rather than bare numbers because [`group_of`] returns one of
/// these and `ORDER` renders them in this sequence: the two have to agree, and a
/// bare `6` in a match arm is unreviewable. `bindable_groups_are_offered_in_a_
/// stable_order` pins the sequence, so reordering `ORDER` without updating these
/// fails a test rather than quietly misfiling every key.
const ORDER_FUNCTION: &str = "Function keys";
const ORDER_LETTERS: &str = "Letters";
const ORDER_DIGITS: &str = "Digits";
const ORDER_NAVIGATION: &str = "Navigation & editing";
const ORDER_NUMPAD: &str = "Numpad";
const ORDER_MEDIA: &str = "Media & browser";
const ORDER_PUNCTUATION: &str = "Punctuation";
const ORDER_LOCKS: &str = "Locks";

const GROUPS: usize = 8;
const IDX_FUNCTION: usize = 0;
const IDX_LETTERS: usize = 1;
const IDX_DIGITS: usize = 2;
const IDX_NAVIGATION: usize = 3;
const IDX_NUMPAD: usize = 4;
const IDX_MEDIA: usize = 5;
const IDX_PUNCTUATION: usize = 6;
const IDX_LOCKS: usize = 7;

/// One `<optgroup>` in the picker.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KeyGroup {
    pub label: &'static str,
    pub keys: Vec<&'static str>,
}

/// Every key that may be bound, grouped for the picker's dropdown.
///
/// **Derived from [`NAMED`] and the arithmetic letter/digit ranges, never from a
/// second hand-written list.** The list the user is offered and the list
/// `set_hotkey` accepts have to be the same object: two lists is precisely how
/// `:` came to be displayed nowhere and bindable never, and how the reverse —
/// offering a key the backend rejects — would arrive.
///
/// Excluded, both deliberately:
/// - **modifiers**, because a binding whose key is Ctrl/Alt/Shift/Win fires on
///   every shortcut the user presses all day (see `Hotkey`'s `Deserialize`,
///   which enforces the same rule at the config-file entrance);
/// - **Escape**, the universal way out. It has never been in `NAMED`; the
///   assertion in the tests is there so it does not quietly arrive.
pub fn bindable_groups() -> Vec<KeyGroup> {
    const ORDER: [&str; GROUPS] = [
        ORDER_FUNCTION,
        ORDER_LETTERS,
        ORDER_DIGITS,
        ORDER_NAVIGATION,
        ORDER_NUMPAD,
        ORDER_MEDIA,
        ORDER_PUNCTUATION,
        ORDER_LOCKS,
    ];

    let mut groups: Vec<KeyGroup> =
        ORDER.iter().map(|label| KeyGroup { label, keys: Vec::new() }).collect();

    let named = NAMED.iter().map(|(k, n)| (k.0, *n));
    let letters = (0x41u16..=0x5A).map(|vk| (vk, ascii_name(vk)));
    let digits = (0x30u16..=0x39).map(|vk| (vk, ascii_name(vk)));

    for (vk, name) in named.chain(letters).chain(digits) {
        if is_modifier(vk) || vk == VK_ESCAPE.0 {
            continue;
        }
        groups[group_of(vk)].keys.push(name);
    }
    groups
}

/// Which [`ORDER`] group a VK belongs in, **as an index**.
///
/// An index rather than a label string on purpose. Matching group labels by
/// string means a group named here but missing from `ORDER` drops every key in
/// it — silently, with no compile error, no warning and no test failure, since
/// every other test only walks keys that already made it in. Indexing panics
/// loudly on a mismatch instead, and `no_key_is_dropped_on_the_way_into_a_group`
/// pins the total.
///
/// Arms are ordered, and the `is_printable` arm sits late on purpose: letters
/// and digits are printable too, and they have their own groups.
fn group_of(vk: u16) -> usize {
    match vk {
        0x70..=0x87 => IDX_FUNCTION, // F1-F24
        0x41..=0x5A => IDX_LETTERS,
        0x30..=0x39 => IDX_DIGITS,
        0x60..=0x6F => IDX_NUMPAD, // Numpad0-9 and its five operators
        0x14 | 0x90 | 0x91 => IDX_LOCKS, // Caps, Num, Scroll
        0xA6..=0xB7 => IDX_MEDIA,
        _ if is_printable(vk) => IDX_PUNCTUATION,
        _ => IDX_NAVIGATION,
    }
}

/// True for **letters, digits and punctuation**. Backs the bare-key warning
/// (`Hotkey::is_bare_printable`): bound with no modifier, these still type
/// themselves into whatever is focused *and* drive the microphone. F13-F24 are
/// deliberately excluded — they type nothing, which is exactly why they are the
/// recommended bare binding.
///
/// **Space, Tab, Enter and the numpad digits also type characters and are
/// deliberately not included.** Widening this to "every key that produces a
/// character" is a product decision about what the warning is for, not a bug —
/// see DESIGN.md §4.4, which enumerates the intended scope.
///
/// The OEM ranges cover the punctuation block: `0xBA..=0xC0` is `;` `=` `,` `-`
/// `.` `/` `` ` ``, and `0xDB..=0xDE` is `[` `\` `]` `'`.
pub fn is_printable(vk: u16) -> bool {
    (0x41..=0x5A).contains(&vk)
        || (0x30..=0x39).contains(&vk)
        || (0xBA..=0xC0).contains(&vk)
        || (0xDB..=0xDE).contains(&vk)
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

    /// Task 17. `NAMED` had no OEM entries, so `vk_to_name(0xBA)` was `None` and
    /// `:` — one of the four keys the owner tried — could never display, commit
    /// or persist. Names are the **unshifted US character on the keycap**, which
    /// is what the picker shows.
    #[test]
    fn punctuation_keys_resolve_and_round_trip() {
        for name in [";", "=", ",", "-", ".", "/", "`", "[", "\\", "]", "'"] {
            let vk = name_to_vk(name).unwrap_or_else(|| panic!("{name} not mapped"));
            assert_eq!(vk_to_name(vk), Some(name), "round-trip failed for {name}");
        }
    }

    #[test]
    fn the_key_that_could_never_be_bound_now_resolves() {
        assert_eq!(vk_to_name(0xBA), Some(";"));
    }

    /// Backs the bare-key warning. Punctuation belongs here for the same reason
    /// letters do: bound bare, the key still types its character everywhere
    /// while also toggling the microphone.
    #[test]
    fn printable_covers_letters_digits_and_punctuation_but_not_function_keys() {
        assert!(is_printable(0x41), "A is printable");
        assert!(is_printable(0x30), "0 is printable");
        assert!(is_printable(0xBA), "; is printable");
        assert!(is_printable(0xDE), "' is printable");
        assert!(!is_printable(0x7C), "F13 is not printable");
        assert!(!is_printable(0x25), "Left arrow is not printable");
    }

    /// The picker's dropdown. Order is asserted because it is the order the
    /// user reads, and an accidental reshuffle is invisible in review.
    #[test]
    fn bindable_groups_are_offered_in_a_stable_order() {
        let labels: Vec<&str> = bindable_groups().iter().map(|g| g.label).collect();
        assert_eq!(
            labels,
            vec![
                "Function keys",
                "Letters",
                "Digits",
                "Navigation & editing",
                "Numpad",
                "Media & browser",
                "Punctuation",
                "Locks",
            ]
        );
    }

    /// **The point of the whole picker**: the list the user is offered and the
    /// list `set_hotkey` accepts are the same object, so a key can never be
    /// offered that cannot be bound — the `:` bug — nor bound if not offered.
    #[test]
    fn every_offered_key_round_trips_and_is_legal_to_bind() {
        for g in bindable_groups() {
            assert!(!g.keys.is_empty(), "group {} is empty", g.label);
            for name in g.keys {
                let vk = name_to_vk(name).unwrap_or_else(|| panic!("{name} not mapped"));
                assert_eq!(vk_to_name(vk), Some(name), "round-trip failed for {name}");
                assert!(!is_modifier(vk), "{name} is a modifier and must not be offered");
                assert_ne!(vk, 0x1B, "Escape must never be offered");
            }
        }
    }

    #[test]
    fn bindable_groups_include_every_extended_function_key_and_every_punctuation_key() {
        let all: Vec<&str> = bindable_groups().into_iter().flat_map(|g| g.keys).collect();
        for vk in 0x7Cu16..=0x87 {
            let name = vk_to_name(vk).expect("F13-F24 are named");
            assert!(all.contains(&name), "{name} must be offerable");
        }
        for name in [";", "=", ",", "-", ".", "/", "`", "[", "\\", "]", "'"] {
            assert!(all.contains(&name), "{name} must be offerable");
        }
        for name in ["A", "Z", "0", "9"] {
            assert!(all.contains(&name), "{name} must be offerable");
        }
    }

    /// The counting test below proves no key is *lost*. It cannot prove a key
    /// is in the *right* group: swap the values of `IDX_PUNCTUATION` and
    /// `IDX_MEDIA` and every punctuation key files under "Media & browser" with
    /// the total unchanged, the labels unchanged, and every other test green.
    /// One member per group closes that.
    #[test]
    fn each_group_contains_the_keys_its_label_promises() {
        let groups = bindable_groups();
        let member = |label: &str, key: &str| {
            let g = groups
                .iter()
                .find(|g| g.label == label)
                .unwrap_or_else(|| panic!("no group labelled {label}"));
            assert!(g.keys.contains(&key), "{key} should be under {label}, found {:?}", g.keys);
        };

        member("Function keys", "F16");
        member("Letters", "M");
        member("Digits", "7");
        member("Navigation & editing", "Home");
        member("Numpad", "Numpad0");
        member("Media & browser", "VolumeMute");
        member("Punctuation", ";");
        member("Locks", "CapsLock");
    }

    /// `bindable_groups` buckets each key by [`group_of`]. If a key's group is
    /// not one of the groups that exist, it is dropped — silently, with no
    /// compile error and no other test noticing, because every other test here
    /// only walks keys that already made it in. This is the one assertion that
    /// catches that, so it counts rather than samples.
    #[test]
    fn no_key_is_dropped_on_the_way_into_a_group() {
        let offered: usize = bindable_groups().iter().map(|g| g.keys.len()).sum();

        let named = NAMED
            .iter()
            .filter(|(k, _)| !is_modifier(k.0) && k.0 != VK_ESCAPE.0)
            .count();
        // 26 letters + 10 digits, which `bindable_groups` generates rather than
        // reads out of `NAMED`.
        let expected = named + 26 + 10;

        assert_eq!(offered, expected, "a key was silently dropped into no group");
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

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub mod keys;
pub mod hook;
pub mod recorder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u16,
}

impl Hotkey {
    /// Human-readable label, e.g. "Ctrl + Alt + F13". Modifier order is fixed so
    /// the same binding always renders identically.
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if self.ctrl { parts.push("Ctrl"); }
        if self.alt { parts.push("Alt"); }
        if self.shift { parts.push("Shift"); }
        if self.win { parts.push("Win"); }
        parts.push(keys::vk_to_name(self.vk).unwrap_or("?"));
        parts.join(" + ")
    }

    /// True when the binding is a single printable character with no modifiers,
    /// which globally intercepts that character. Drives the recorder's warning
    /// (§4.4). F13-F24 are deliberately excluded — they are the recommended
    /// bare binding.
    pub fn is_bare_printable(&self) -> bool {
        if self.ctrl || self.alt || self.shift || self.win {
            return false;
        }
        (0x41..=0x5A).contains(&self.vk) || (0x30..=0x39).contains(&self.vk)
    }
}

/// Wire format. The key is stored by name so config files stay readable and so
/// a VK change in Windows can never silently repoint an existing binding.
#[derive(Serialize, Deserialize)]
struct HotkeyRepr {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
    key: String,
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let key = keys::vk_to_name(self.vk)
            .ok_or_else(|| serde::ser::Error::custom(format!("unnamed VK {:#04X}", self.vk)))?;
        HotkeyRepr {
            ctrl: self.ctrl, alt: self.alt, shift: self.shift, win: self.win,
            key: key.to_string(),
        }.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = HotkeyRepr::deserialize(d)?;
        let vk = keys::name_to_vk(&r.key)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown key name {:?}", r.key)))?;
        // A binding's key must never be a modifier (e.g. "LeftCtrl"). The
        // keyboard hook (hotkey::hook) swallows the physical key its binding
        // names on every matching press; a modifier-named binding would make
        // it swallow bare Ctrl/Alt/Shift/Win system-wide, breaking every
        // other shortcut on the machine. This is the config-file half of
        // that invariant — `Recorder::feed` (recorder.rs) is the other half,
        // rejecting modifiers as they're pressed during recording. Corrupt
        // or hand-edited config containing a modifier key name must fail to
        // deserialize so `Config::load` falls back to defaults instead of
        // ever handing the hook a modifier binding.
        if keys::is_modifier(vk) {
            return Err(serde::de::Error::custom(format!(
                "key {:?} is a modifier and cannot be a hotkey binding",
                r.key
            )));
        }
        Ok(Hotkey { ctrl: r.ctrl, alt: r.alt, shift: r.shift, win: r.win, vk })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_orders_modifiers_consistently() {
        let hk = Hotkey { ctrl: true, alt: true, shift: false, win: false, vk: 0x7C };
        assert_eq!(hk.display(), "Ctrl + Alt + F13");
    }

    #[test]
    fn display_handles_bare_key() {
        let hk = Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0x7C };
        assert_eq!(hk.display(), "F13");
    }

    #[test]
    fn serde_roundtrip_uses_key_name_not_vk() {
        let hk = Hotkey { ctrl: true, alt: false, shift: false, win: false, vk: 0x87 };
        let json = serde_json::to_string(&hk).unwrap();
        assert!(json.contains("\"F24\""), "expected key name in JSON, got {json}");
        assert!(!json.contains("135"), "raw VK leaked into JSON: {json}");
        assert_eq!(serde_json::from_str::<Hotkey>(&json).unwrap(), hk);
    }

    #[test]
    fn deserializing_unknown_key_name_fails_cleanly() {
        let json = r#"{"ctrl":false,"alt":false,"shift":false,"win":false,"key":"Nonsense"}"#;
        assert!(serde_json::from_str::<Hotkey>(json).is_err());
    }

    #[test]
    fn deserializing_a_modifier_key_name_fails() {
        // A hand-edited or corrupted config naming a modifier as the bound key
        // (e.g. "LeftCtrl") must not deserialize into a Hotkey: the keyboard
        // hook would swallow that modifier system-wide. Config::load's
        // fallback-to-defaults-on-error path handles the failure safely.
        let json = r#"{"ctrl":false,"alt":false,"shift":false,"win":false,"key":"LeftCtrl"}"#;
        assert!(serde_json::from_str::<Hotkey>(json).is_err());
    }

    #[test]
    fn is_bare_printable_flags_letters_but_not_function_keys() {
        let m = Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0x4D };
        assert!(m.is_bare_printable(), "bare M should warn");
        let f13 = Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0x7C };
        assert!(!f13.is_bare_printable(), "F13 must NOT warn");
        let ctrl_m = Hotkey { ctrl: true, alt: false, shift: false, win: false, vk: 0x4D };
        assert!(!ctrl_m.is_bare_printable(), "Ctrl+M is not bare");
    }
}

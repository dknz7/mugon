use crate::hotkey::Hotkey;
use crate::modes::Mode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE: &str = "config.json";
const BACKUP: &str = "config.json.bak";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationPrefs {
    pub toast: bool,
    pub sound: bool,
}

impl Default for NotificationPrefs {
    fn default() -> Self { Self { toast: true, sound: false } }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    /// `None` means follow the system default capture device (§4.5).
    pub device_id: Option<String>,
    pub mode: Mode,
    pub hotkey: Option<Hotkey>,
    /// Whether this binding has ever actually been *seen* firing (§4.4, Task 17).
    ///
    /// Drives the `HOTKEY STATUS` line: a binding is `Bound` until the hook
    /// observes it once, then `Confirmed`. It exists because the picker replaced
    /// recording, and recording was the only thing that used to prove a key
    /// reaches mugon at all — which matters most for F13-F24, since those are
    /// not physical keys on a standard board and arrive via a remapper that may
    /// simply not be running.
    ///
    /// `#[serde(default)]` so every config file written before this field
    /// existed still loads. They arrive `false`, which is the honest answer:
    /// this build has never seen those bindings fire.
    #[serde(default)]
    pub hotkey_confirmed: bool,
    pub notifications: NotificationPrefs,
    pub autostart: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            device_id: None,
            mode: Mode::default(),
            hotkey: None,
            hotkey_confirmed: false,
            notifications: NotificationPrefs::default(),
            autostart: false,
        }
    }
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("mugon")
}

impl Config {
    /// Never fails. A missing file yields defaults; an unreadable or invalid one
    /// is backed up to `config.json.bak` and replaced with defaults (§6).
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str::<Config>(&raw) {
            Ok(c) => c,
            Err(_) => {
                let _ = std::fs::rename(&path, dir.join(BACKUP));
                Self::default()
            }
        }
    }

    /// Atomic: writes to a temp file then renames, so an interrupted write can
    /// never leave a truncated config behind.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, dir.join(FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.version, 1);
        assert_eq!(c.device_id, None, "must follow system default device");
        assert_eq!(c.mode, crate::modes::Mode::MuteToggle);
        assert!(c.notifications.toast);
        assert!(!c.notifications.sound, "sound is off by default");
        assert!(!c.autostart);
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.hotkey = Some(Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0x7C });
        c.mode = crate::modes::Mode::PushToTalk;
        c.save(dir.path()).unwrap();
        assert_eq!(Config::load(dir.path()), c);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(dir.path()), Config::default());
    }

    #[test]
    fn corrupt_file_is_backed_up_and_replaced_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), b"{ not json at all").unwrap();
        assert_eq!(Config::load(dir.path()), Config::default());
        assert!(dir.path().join("config.json.bak").exists(), "corrupt file must be preserved");
    }

    #[test]
    fn unknown_hotkey_name_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            br#"{"version":1,"device_id":null,"mode":"MuteToggle",
                 "hotkey":{"ctrl":false,"alt":false,"shift":false,"win":false,"key":"Bogus"},
                 "notifications":{"toast":true,"sound":false},"autostart":false}"#,
        ).unwrap();
        assert_eq!(Config::load(dir.path()), Config::default());
    }

    /// Task 17 added `hotkey_confirmed`. Every config file on disk predates it,
    /// including the owner's, which holds a working `F16`. Loading one must not
    /// fall into `Config::load`'s corrupt-file path — that would rename his
    /// config to `.bak` and silently replace his binding and device choice with
    /// defaults.
    #[test]
    fn a_config_written_before_hotkey_confirmed_existed_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            br#"{"version":1,"device_id":"some-device","mode":"MuteToggle",
                 "hotkey":{"ctrl":false,"alt":false,"shift":false,"win":false,"key":"F16"},
                 "notifications":{"toast":true,"sound":true},"autostart":false}"#,
        )
        .unwrap();

        let c = Config::load(dir.path());

        assert_eq!(
            c.hotkey.map(|h| h.display()).as_deref(),
            Some("F16"),
            "the existing binding must survive the upgrade"
        );
        assert_eq!(c.device_id.as_deref(), Some("some-device"));
        assert!(!c.hotkey_confirmed, "a binding this build has never seen starts unconfirmed");
        assert!(
            !dir.path().join("config.json.bak").exists(),
            "an older config is not a corrupt one"
        );
    }

    #[test]
    fn volume_is_not_persisted() {
        // DESIGN.md §4.6: volume belongs to Windows, not to us.
        let json = serde_json::to_string(&Config::default()).unwrap();
        assert!(!json.contains("volume"), "volume must not be in config: {json}");
    }
}

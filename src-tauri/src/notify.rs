//! Toasts and the optional beep (DESIGN.md §4.8).

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

use crate::config::NotificationPrefs;
use crate::modes::Mode;

static MUTE_WAV: &[u8] = include_bytes!("../sounds/mute.wav");
static UNMUTE_WAV: &[u8] = include_bytes!("../sounds/unmute.wav");

/// Which side effects a mute-state change should trigger.
///
/// Pulled out of [`on_mute_change`] so the decision — which is the part that
/// actually matters, PTT must never toast — is a plain function of its inputs
/// and testable without a `AppHandle` or touching Win32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Actions {
    toast: bool,
    beep: bool,
}

/// §4.8: toasts fire in Mute Toggle mode only — push-to-talk would toast on
/// every utterance. The beep, when enabled, fires in both modes since you
/// often can't see the screen mid-call.
fn decide(mode: Mode, prefs: &NotificationPrefs) -> Actions {
    Actions {
        toast: prefs.toast && mode == Mode::MuteToggle,
        beep: prefs.sound,
    }
}

/// Fires the configured notifications for a mute state change.
///
/// Takes `mode` because toasts fire in Mute Toggle mode only — push-to-talk
/// would toast on every utterance (DESIGN.md §4.8). The beep, when enabled,
/// fires in both modes.
///
/// Called from the hook dispatch thread with **no locks held**.
pub fn on_mute_change(app: &AppHandle, mode: Mode, muted: bool, prefs: &NotificationPrefs) {
    let actions = decide(mode, prefs);

    if actions.toast {
        let body = if muted { "Microphone muted" } else { "Microphone live" };
        let _ = app.notification().builder().title("mugon").body(body).show();
    }
    if actions.beep {
        play(if muted { MUTE_WAV } else { UNMUTE_WAV });
    }
}

fn play(wav: &'static [u8]) {
    unsafe {
        // `PlaySoundW`'s `pszsound` parameter is typed as a wide-string
        // pointer (`PCWSTR`), but `SND_MEMORY` redefines its meaning: it's a
        // pointer to the in-memory image of a WAV file, not a string, so the
        // wide-char typing is just the Win32 API reusing one parameter slot
        // for two purposes. The cast to `*const u16` is required to satisfy
        // the type and is not a bug.
        //
        // `SND_ASYNC` so this never blocks the hotkey-dispatch thread.
        // `SND_NODEFAULT` suppresses the Windows ding if the embedded buffer
        // is somehow unplayable.
        let _ = PlaySoundW(
            windows::core::PCWSTR(wav.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs(toast: bool, sound: bool) -> NotificationPrefs {
        NotificationPrefs { toast, sound }
    }

    #[test]
    fn ptt_never_toasts_regardless_of_prefs() {
        assert!(!decide(Mode::PushToTalk, &prefs(true, false)).toast);
        assert!(!decide(Mode::PushToTalk, &prefs(true, true)).toast);
        assert!(!decide(Mode::PushToTalk, &prefs(false, false)).toast);
    }

    #[test]
    fn mute_toggle_toasts_when_enabled() {
        assert!(decide(Mode::MuteToggle, &prefs(true, false)).toast);
    }

    #[test]
    fn mute_toggle_does_not_toast_when_disabled() {
        assert!(!decide(Mode::MuteToggle, &prefs(false, false)).toast);
    }

    #[test]
    fn beep_fires_in_both_modes_when_enabled() {
        assert!(decide(Mode::MuteToggle, &prefs(false, true)).beep);
        assert!(decide(Mode::PushToTalk, &prefs(false, true)).beep);
    }

    #[test]
    fn beep_never_fires_when_disabled() {
        assert!(!decide(Mode::MuteToggle, &prefs(true, false)).beep);
        assert!(!decide(Mode::PushToTalk, &prefs(true, false)).beep);
    }

    #[test]
    fn toast_and_beep_are_independent() {
        let a = decide(Mode::MuteToggle, &prefs(true, true));
        assert!(a.toast && a.beep);
        let b = decide(Mode::MuteToggle, &prefs(false, true));
        assert!(!b.toast && b.beep);
    }
}

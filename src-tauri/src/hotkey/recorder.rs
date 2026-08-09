//! Hotkey recording state machine (§4.4). Pure logic — it consumes `HookEvent`s
//! and produces outcomes, so it is fully unit tested without Win32.

use super::hook::HookEvent;
use super::{keys, Hotkey};

const VK_ESCAPE: u16 = 0x1B;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecorderOutcome {
    /// Recorder is not running; the event was ignored.
    Idle,
    /// Recording; carries the combo so far, or `None` if only modifiers are held.
    InProgress(Option<Hotkey>),
    Committed(Hotkey),
    Cancelled,
}

#[derive(Default)]
pub struct Recorder {
    active: bool,
    pending: Option<Hotkey>,
}

impl Recorder {
    pub fn start(&mut self) {
        self.active = true;
        self.pending = None;
    }

    pub fn cancel(&mut self) {
        self.active = false;
        self.pending = None;
    }

    pub fn is_active(&self) -> bool { self.active }

    /// Feeds one hook event. Commits on the release of a non-modifier key, which
    /// lets the user see the combo build up while holding it.
    pub fn feed(&mut self, ev: &HookEvent) -> RecorderOutcome {
        if !self.active {
            return RecorderOutcome::Idle;
        }
        // Esc cancels and cannot be bound (§4.4).
        if ev.vk == VK_ESCAPE {
            self.cancel();
            return RecorderOutcome::Cancelled;
        }
        if keys::is_modifier(ev.vk) {
            return RecorderOutcome::InProgress(self.pending);
        }
        // Reject keys with no name — they cannot be displayed or persisted.
        if keys::vk_to_name(ev.vk).is_none() {
            return RecorderOutcome::InProgress(self.pending);
        }

        let hk = Hotkey {
            ctrl: ev.ctrl, alt: ev.alt, shift: ev.shift, win: ev.win, vk: ev.vk,
        };
        if ev.down {
            self.pending = Some(hk);
            RecorderOutcome::InProgress(self.pending)
        } else {
            // Commit the combo captured on key-down; modifiers may already have
            // been released by the time this key-up arrives.
            let committed = self.pending.unwrap_or(hk);
            self.active = false;
            self.pending = None;
            RecorderOutcome::Committed(committed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::hook::HookEvent;

    fn ev(vk: u16, down: bool) -> HookEvent {
        HookEvent { vk, down, ctrl: false, alt: false, shift: false, win: false }
    }
    fn ev_mods(vk: u16, down: bool, ctrl: bool, alt: bool) -> HookEvent {
        HookEvent { vk, down, ctrl, alt, shift: false, win: false }
    }

    #[test]
    fn inactive_recorder_ignores_events() {
        let mut r = Recorder::default();
        assert!(matches!(r.feed(&ev(0x7C, true)), RecorderOutcome::Idle));
    }

    #[test]
    fn records_a_bare_extended_function_key() {
        let mut r = Recorder::default();
        r.start();
        assert!(matches!(r.feed(&ev(0x7C, true)), RecorderOutcome::InProgress(Some(_))));
        match r.feed(&ev(0x7C, false)) {
            RecorderOutcome::Committed(hk) => {
                assert_eq!(hk.vk, 0x7C);
                assert_eq!(hk.display(), "F13");
            }
            other => panic!("expected commit, got {other:?}"),
        }
        assert!(!r.is_active(), "recorder must stop after committing");
    }

    #[test]
    fn records_a_modifier_combo() {
        let mut r = Recorder::default();
        r.start();
        r.feed(&ev_mods(0x11, true, true, false));           // Ctrl down
        r.feed(&ev_mods(0x12, true, true, true));            // Alt down
        r.feed(&ev_mods(0x4D, true, true, true));            // M down
        match r.feed(&ev_mods(0x4D, false, true, true)) {
            RecorderOutcome::Committed(hk) => assert_eq!(hk.display(), "Ctrl + Alt + M"),
            other => panic!("expected commit, got {other:?}"),
        }
    }

    #[test]
    fn modifier_only_press_does_not_commit() {
        let mut r = Recorder::default();
        r.start();
        r.feed(&ev_mods(0x11, true, true, false));
        assert!(matches!(r.feed(&ev_mods(0x11, false, false, false)),
                         RecorderOutcome::InProgress(None)));
        assert!(r.is_active(), "releasing a lone modifier must not commit");
    }

    #[test]
    fn escape_cancels() {
        let mut r = Recorder::default();
        r.start();
        assert!(matches!(r.feed(&ev(0x1B, true)), RecorderOutcome::Cancelled));
        assert!(!r.is_active());
    }

    #[test]
    fn extended_function_keys_all_record() {
        for vk in 0x7Cu16..=0x87u16 {
            let mut r = Recorder::default();
            r.start();
            r.feed(&ev(vk, true));
            match r.feed(&ev(vk, false)) {
                RecorderOutcome::Committed(hk) => assert_eq!(hk.vk, vk),
                other => panic!("VK {vk:#04X} failed to record: {other:?}"),
            }
        }
    }
}

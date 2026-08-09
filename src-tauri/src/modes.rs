use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    MuteToggle,
    PushToTalk,
}

impl Default for Mode {
    fn default() -> Self { Mode::MuteToggle }
}

use crate::audio::MicControl;

/// A key state transition. Auto-repeat is filtered by the machine, not the hook,
/// so the hook stays a dumb translator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEdge {
    Down,
    Up,
}

/// Sole authority on the microphone's mute state (§3). Owns the `MicControl`
/// so nothing else can write mute behind its back.
pub struct ModeMachine<M: MicControl> {
    mic: M,
    mode: Mode,
    held: bool,
}

impl<M: MicControl> ModeMachine<M> {
    pub fn new(mic: M, mode: Mode) -> Self {
        let mut m = Self { mic, mode, held: false };
        m.reapply_resting_state();
        m
    }

    pub fn mode(&self) -> Mode { self.mode }
    pub fn is_held(&self) -> bool { self.held }
    pub fn mic(&self) -> &M { &self.mic }
    pub fn mic_mut(&mut self) -> &mut M { &mut self.mic }

    /// False in Push to Talk, where the machine owns mute and a manual override
    /// would be undone by the next key event (§4.1).
    pub fn manual_controls_enabled(&self) -> bool {
        self.mode == Mode::MuteToggle
    }

    /// PTT rests muted; Mute Toggle rests unmuted. Applied on construction, on
    /// every mode change (§4.2), and — this is why it is public — after the
    /// mic is re-pointed at a different device.
    ///
    /// The resting state is a property of the **endpoint**, not of this
    /// machine. `MicControl::select` moves the mic to a device sitting at
    /// whatever mute state Windows last left it in, while the old device keeps
    /// the state this machine set. In Push to Talk that is the dangerous
    /// direction: the user believes they are muted at rest and the newly
    /// selected device is live. Whoever calls `select` must call this
    /// immediately afterwards.
    ///
    /// Known wart, deliberately not handled: called mid-hold in PTT (the user
    /// changes device while holding the key) this mutes a device that should be
    /// live. It self-corrects on the next press, and it fails *muted* rather
    /// than *hot*, which is the direction this app should fail in.
    ///
    /// Idempotent, and safe to call in any mode.
    pub fn reapply_resting_state(&mut self) {
        let muted = self.mode == Mode::PushToTalk;
        let _ = self.mic.set_muted(muted);
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        self.held = false;
        self.reapply_resting_state();
    }

    pub fn on_key(&mut self, edge: KeyEdge) {
        match (self.mode, edge) {
            // Auto-repeat: a Down with no intervening Up.
            (_, KeyEdge::Down) if self.held => {}
            (Mode::MuteToggle, KeyEdge::Down) => {
                self.held = true;
                let target = !self.mic.is_muted().unwrap_or(false);
                let _ = self.mic.set_muted(target);
            }
            (Mode::MuteToggle, KeyEdge::Up) => {
                self.held = false;
            }
            (Mode::PushToTalk, KeyEdge::Down) => {
                self.held = true;
                let _ = self.mic.set_muted(false);
            }
            (Mode::PushToTalk, KeyEdge::Up) => {
                if self.held {
                    self.held = false;
                    let _ = self.mic.set_muted(true);
                }
            }
        }
    }

    /// UI/tray mute switch. No-op in PTT (§4.1).
    pub fn toggle_manual(&mut self) {
        if !self.manual_controls_enabled() {
            return;
        }
        let target = !self.mic.is_muted().unwrap_or(false);
        let _ = self.mic.set_muted(target);
    }

    /// Restores the microphone before exit. Idempotent, and must be reachable
    /// from tray Quit, WM_QUERYENDSESSION, and the panic hook (§4.2).
    pub fn shutdown(&mut self) {
        self.held = false;
        let _ = self.mic.set_muted(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fake::FakeMic;

    fn machine(mode: Mode) -> ModeMachine<FakeMic> {
        ModeMachine::new(FakeMic::new(), mode)
    }

    // --- Mute Toggle (§4.1) ---

    #[test]
    fn toggle_mode_flips_on_key_down() {
        let mut m = machine(Mode::MuteToggle);
        m.on_key(KeyEdge::Down);
        assert!(m.mic().muted);
        m.on_key(KeyEdge::Up);      // a real second press requires releasing first
        m.on_key(KeyEdge::Down);
        assert!(!m.mic().muted);
    }

    #[test]
    fn toggle_mode_ignores_key_up() {
        let mut m = machine(Mode::MuteToggle);
        m.mic_mut().mute_calls.clear();   // drop the constructor's resting-state call
        m.on_key(KeyEdge::Down);
        m.on_key(KeyEdge::Up);
        assert!(m.mic().muted, "key up must not change state in toggle mode");
        assert_eq!(m.mic().mute_calls, vec![true]);
    }

    #[test]
    fn toggle_mode_ignores_autorepeat() {
        let mut m = machine(Mode::MuteToggle);
        m.mic_mut().mute_calls.clear();   // drop the constructor's resting-state call
        m.on_key(KeyEdge::Down);
        m.on_key(KeyEdge::Down); // no intervening Up == auto-repeat
        m.on_key(KeyEdge::Down);
        assert_eq!(m.mic().mute_calls, vec![true], "auto-repeat must not re-fire");
    }

    // --- Push to Talk (§4.1) ---

    #[test]
    fn entering_ptt_mutes_immediately() {
        let mut m = machine(Mode::MuteToggle);
        m.set_mode(Mode::PushToTalk);
        assert!(m.mic().muted, "PTT must mute on entry");
    }

    #[test]
    fn ptt_unmutes_on_hold_and_remutes_on_release() {
        let mut m = machine(Mode::PushToTalk);
        assert!(m.mic().muted, "PTT rests muted");
        m.on_key(KeyEdge::Down);
        assert!(!m.mic().muted, "holding the key must unmute");
        m.on_key(KeyEdge::Up);
        assert!(m.mic().muted, "release must re-mute");
    }

    #[test]
    fn ptt_ignores_autorepeat_while_held() {
        let mut m = machine(Mode::PushToTalk);
        m.mic_mut().mute_calls.clear();
        m.on_key(KeyEdge::Down);
        m.on_key(KeyEdge::Down);
        m.on_key(KeyEdge::Down);
        m.on_key(KeyEdge::Up);
        assert_eq!(m.mic().mute_calls, vec![false, true], "one unmute, one re-mute");
    }

    #[test]
    fn ptt_release_without_press_is_a_noop() {
        let mut m = machine(Mode::PushToTalk);
        m.mic_mut().mute_calls.clear();
        m.on_key(KeyEdge::Up);
        assert!(m.mic().mute_calls.is_empty(), "stray key-up must do nothing");
    }

    // --- Transitions (§4.2) ---

    #[test]
    fn leaving_ptt_for_toggle_unmutes() {
        let mut m = machine(Mode::PushToTalk);
        assert!(m.mic().muted);
        m.set_mode(Mode::MuteToggle);
        assert!(!m.mic().muted, "leaving PTT must unmute");
    }

    #[test]
    fn leaving_ptt_while_held_clears_the_hold() {
        let mut m = machine(Mode::PushToTalk);
        m.on_key(KeyEdge::Down);
        m.set_mode(Mode::MuteToggle);
        assert!(!m.is_held(), "hold state must not survive a mode change");
        assert!(!m.mic().muted);
    }

    /// The device-change hazard. A newly selected endpoint arrives at whatever
    /// mute state Windows last left it in — `FakeMic::select` cannot model that
    /// on its own, so the new device's live state is set directly here. In PTT
    /// the machine must pull it back to muted, or the user is hot while
    /// believing they are muted at rest.
    #[test]
    fn reapplying_the_resting_state_mutes_a_newly_selected_device_in_ptt() {
        let mut m = machine(Mode::PushToTalk);
        assert!(m.mic().muted, "PTT rests muted on the original device");

        m.mic_mut().selected = Some("device-2".into());
        m.mic_mut().muted = false; // the new endpoint is live
        m.mic_mut().mute_calls.clear();

        m.reapply_resting_state();
        assert!(m.mic().muted, "the newly selected device must be muted at rest in PTT");
        assert_eq!(m.mic().mute_calls, vec![true]);
    }

    #[test]
    fn reapplying_the_resting_state_unmutes_a_newly_selected_device_in_mute_toggle() {
        let mut m = machine(Mode::MuteToggle);

        m.mic_mut().selected = Some("device-2".into());
        m.mic_mut().muted = true; // the new endpoint arrives muted
        m.mic_mut().mute_calls.clear();

        m.reapply_resting_state();
        assert!(!m.mic().muted, "Mute Toggle rests unmuted");
        assert_eq!(m.mic().mute_calls, vec![false]);
    }

    #[test]
    fn shutdown_always_unmutes() {
        for mode in [Mode::MuteToggle, Mode::PushToTalk] {
            let mut m = machine(mode);
            m.on_key(KeyEdge::Down);
            m.shutdown();
            assert!(!m.mic().muted, "{mode:?}: mic must be live after shutdown");
        }
    }

    #[test]
    fn shutdown_is_idempotent() {
        let mut m = machine(Mode::PushToTalk);
        m.shutdown();
        m.shutdown();
        assert!(!m.mic().muted);
    }

    // --- Manual controls (§4.1) ---

    #[test]
    fn manual_controls_disabled_in_ptt() {
        assert!(machine(Mode::MuteToggle).manual_controls_enabled());
        assert!(!machine(Mode::PushToTalk).manual_controls_enabled());
    }

    #[test]
    fn manual_toggle_is_ignored_in_ptt() {
        let mut m = machine(Mode::PushToTalk);
        m.mic_mut().mute_calls.clear();
        m.toggle_manual();
        assert!(m.mic().mute_calls.is_empty(), "manual toggle must no-op in PTT");
        assert!(m.mic().muted, "PTT resting state must be preserved");
    }

    #[test]
    fn manual_toggle_works_in_toggle_mode() {
        let mut m = machine(Mode::MuteToggle);
        m.toggle_manual();
        assert!(m.mic().muted);
    }

    // --- Error resilience (§7) ---

    #[test]
    fn device_error_does_not_panic_or_corrupt_hold_state() {
        let mut m = machine(Mode::PushToTalk);
        m.mic_mut().fail_next = true;
        m.on_key(KeyEdge::Down);
        m.on_key(KeyEdge::Up);
        assert!(!m.is_held(), "hold must clear even if the device call failed");
    }
}

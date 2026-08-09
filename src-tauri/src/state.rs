//! Application state: the runtime the Tauri commands mutate, and the flat
//! snapshot the frontend renders.
//!
//! # Locking discipline
//!
//! Exactly one mutex guards mutable app state: [`Shared`] (`Mutex<Core>`).
//! Two rules keep it deadlock-free, and both are load-bearing:
//!
//! 1. **The audio worker never takes the `Core` lock.** It owns a `!Send`
//!    Core Audio endpoint and services commands over a channel
//!    ([`crate::audio::thread`]); it has no `AppHandle` and no way to reach
//!    `Core`. So holding `Core` while blocking on an audio reply cannot
//!    cycle — the reply does not need `Core` to be produced.
//! 2. **The 30Hz level meter never takes the `Core` lock either.** It polls
//!    through a [`MeterTap`], which is a bare channel sender onto the same
//!    worker. Keep it that way: a meter that needed `Core` would be taking
//!    the lock 30 times a second against every command handler.
//!
//! [`emit_state`] takes the lock to build its snapshot, so it must never be
//! called while a guard is already held. Command bodies scope their guard in
//! an inner block and call it after.

use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio::thread::{MeterTap, MicHandle};
use crate::audio::{AudioError, DeviceInfo, MicBackend};
use crate::config::{Config, NotificationPrefs};
use crate::hotkey::recorder::Recorder;
use crate::modes::{Mode, ModeMachine};

/// The one event carrying [`AppState`]. Task 14's frontend re-renders
/// wholesale on it and stores no state of its own (DESIGN.md §3).
pub const STATE_CHANGED: &str = "state-changed";

/// How long startup waits for the audio worker to enter the MTA and activate
/// its interfaces before giving up.
///
/// `MicHandle::spawn` blocks until the worker reports ready and has no bound
/// of its own, so a wedged Windows audio service would otherwise hang the
/// process before the UI ever exists — a silent no-start with no diagnostic.
/// Five seconds is ample for work that normally takes single-digit
/// milliseconds.
pub const AUDIO_SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The microphone control the app actually has.
///
/// `Unavailable` is not a placeholder for "not wired up yet" — it is the real
/// state of a machine whose audio stack failed at startup. Modelling it as a
/// backend rather than as `Option<ModeMachine<..>>` means every command body
/// stays a straight-line call whose `Err` flows into `last_error` through the
/// same path a transient COM failure takes, instead of every command growing
/// a "do we have a mic?" branch.
pub enum Mic {
    Live(MicHandle),
    Unavailable,
}

impl Mic {
    /// A metering capability, if there is a live worker to meter. `None` when
    /// startup failed — there is nothing to poll.
    pub fn tap(&self) -> Option<MeterTap> {
        match self {
            Mic::Live(handle) => Some(handle.tap()),
            Mic::Unavailable => None,
        }
    }
}

impl MicBackend for Mic {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        match self {
            Mic::Live(h) => h.list_devices(),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
        match self {
            Mic::Live(h) => h.select(id),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
    fn is_muted(&self) -> Result<bool, AudioError> {
        match self {
            Mic::Live(h) => h.is_muted(),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
    fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
        match self {
            Mic::Live(h) => h.set_muted(muted),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
    fn volume(&self) -> Result<f32, AudioError> {
        match self {
            Mic::Live(h) => h.volume(),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
    fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
        match self {
            Mic::Live(h) => h.set_volume(level),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
    fn peak(&self) -> Result<f32, AudioError> {
        match self {
            Mic::Live(h) => h.peak(),
            Mic::Unavailable => Err(AudioError::NoDevice),
        }
    }
}

/// Everything the frontend renders. The frontend stores none of this — it
/// re-renders wholesale on every [`STATE_CHANGED`] event (DESIGN.md §3).
///
/// Task 14 mirrors this field-for-field; `tests::app_state_wire_format_is_pinned`
/// is what notices when the two drift.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppState {
    pub devices: Vec<DeviceInfo>,
    pub selected_device: Option<String>,
    pub mode: Mode,
    pub muted: bool,
    pub volume: f32,
    pub hotkey_display: Option<String>,
    pub hotkey_is_bare_printable: bool,
    pub manual_controls_enabled: bool,
    pub notifications: NotificationPrefs,
    pub autostart: bool,
    pub recording: bool,
    /// The last device failure, or `None` if the last audio call succeeded.
    /// One field, no history, no severity — enough for the UI to say "that
    /// didn't work, and here's why" instead of nothing at all.
    pub last_error: Option<String>,
}

pub struct Core {
    pub machine: ModeMachine<Mic>,
    pub config: Config,
    pub recorder: Recorder,
    pub last_error: Option<String>,
}

impl Core {
    /// Reads live state off the endpoint. Every read degrades to a sensible
    /// default rather than failing the snapshot: a UI that renders "muted:
    /// false, no devices" alongside `last_error` is strictly more useful than
    /// a command that returns nothing at all.
    ///
    /// Takes `&self` deliberately — snapshotting must not clear or set
    /// `last_error`, or a `state-changed` emitted right after a failed command
    /// would wipe the very error it was emitted to deliver.
    pub fn snapshot(&self) -> AppState {
        AppState {
            devices: self.machine.mic().list_devices().unwrap_or_default(),
            selected_device: self.config.device_id.clone(),
            mode: self.machine.mode(),
            muted: self.machine.mic().is_muted().unwrap_or(false),
            volume: self.machine.mic().volume().unwrap_or(1.0),
            hotkey_display: self.config.hotkey.map(|h| h.display()),
            hotkey_is_bare_printable: self
                .config
                .hotkey
                .map(|h| h.is_bare_printable())
                .unwrap_or(false),
            manual_controls_enabled: self.machine.manual_controls_enabled(),
            notifications: self.config.notifications.clone(),
            autostart: self.config.autostart,
            recording: self.recorder.is_active(),
            last_error: self.last_error.clone(),
        }
    }

    /// Folds an audio call's outcome into [`Self::last_error`]: an error is
    /// remembered, a success clears whatever was there.
    ///
    /// Takes the result by reference so callers keep ownership of the value
    /// and there is no `#[must_use]` return to discard.
    pub fn record_outcome<T>(&mut self, result: &Result<T, AudioError>) {
        self.last_error = match result {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
    }

    /// Re-reads mute state purely to find out whether the endpoint is still
    /// answering.
    ///
    /// [`ModeMachine`] swallows device errors by design (DESIGN.md §7 — a
    /// failed Core Audio call must never panic the state machine), so a mute
    /// change that silently failed would otherwise leave the user with no
    /// signal whatsoever. This is the cheapest honest probe: if the worker is
    /// dead or the endpoint is broken, this read fails too.
    pub fn refresh_mic_health(&mut self) {
        let result = self.machine.mic().is_muted();
        self.record_outcome(&result);
    }

    /// Persists config. Failures are logged, never fatal — a read-only
    /// `%APPDATA%` must not take the app down (DESIGN.md §6).
    pub fn persist(&self) {
        if let Err(e) = self.config.save(&crate::config::config_dir()) {
            eprintln!("mugon: failed to save config: {e}");
        }
    }
}

pub type Shared = Mutex<Core>;

/// Locks a mutex, recovering from poisoning instead of propagating the panic.
///
/// A poisoned `Core` must not take down a Tauri command handler: the guarded
/// state is plain data with no invariant a panic mid-write could leave broken,
/// and the alternative — every command panicking forever after one unrelated
/// panic — turns a recoverable fault into a dead app. Same rationale as the
/// lock sites in [`crate::hotkey::hook`].
pub fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Pushes a fresh snapshot to the frontend.
///
/// **Takes the `Core` lock.** Never call it while already holding one — see
/// this module's locking discipline.
pub fn emit_state(app: &AppHandle) {
    // `try_state` rather than `state`: the latter panics when nothing is
    // managed, and there is no useful recovery from a missed event anyway.
    let Some(core) = app.try_state::<Shared>() else {
        return;
    };
    // The guard is a temporary of this statement, so it is released before the
    // emit below — deliberately, not incidentally. `emit` runs Tauri's own
    // listener bookkeeping and (for the meter's 30Hz stream) contends with
    // other threads; holding `Core` across it would put every command handler
    // behind that work for no reason.
    let snapshot = lock_or_recover(&core).snapshot();
    let _ = app.emit(STATE_CHANGED, snapshot);
}

/// Spawns the audio worker, bounded by [`AUDIO_SPAWN_TIMEOUT`].
pub fn spawn_mic() -> Result<MicHandle, AudioError> {
    spawn_with_timeout(AUDIO_SPAWN_TIMEOUT, MicHandle::spawn)
}

/// Runs a blocking constructor on a throwaway thread and gives up on it after
/// `timeout`.
///
/// The thread is deliberately **not** joined on timeout — the whole point is
/// that it may never return. It is not leaked in any meaningful sense: when it
/// finally does finish, its send fails (nobody is listening), the value it was
/// carrying drops, and a `MicHandle`'s own `Drop` shuts its worker down
/// cleanly at that moment.
///
/// Generic over the constructor rather than hardcoding [`MicHandle::spawn`] so
/// the timeout behaviour itself is testable without audio hardware.
fn spawn_with_timeout<T, F>(timeout: Duration, make: F) -> Result<T, AudioError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AudioError> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("mugon-audio-startup".into())
        .spawn(move || {
            let _ = tx.send(make());
        });
    if let Err(e) = spawned {
        return Err(AudioError::Windows(format!(
            "could not spawn the audio startup thread: {e}"
        )));
    }

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(AudioError::Windows(format!(
            "the Windows audio service did not respond within {}s",
            timeout.as_secs()
        ))),
        // The startup thread died without reporting.
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(AudioError::ThreadTerminated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;

    fn unavailable_core() -> Core {
        Core {
            machine: ModeMachine::new(Mic::Unavailable, Mode::MuteToggle),
            config: Config::default(),
            recorder: Recorder::default(),
            last_error: None,
        }
    }

    /// Pins the exact wire shape Task 14's frontend mirrors field-for-field.
    /// A renamed or dropped field should fail here rather than in a browser.
    #[test]
    fn app_state_wire_format_is_pinned() {
        assert_eq!(STATE_CHANGED, "state-changed");

        let value = serde_json::to_value(unavailable_core().snapshot()).unwrap();
        let object = value.as_object().expect("AppState must serialize to an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "autostart",
                "devices",
                "hotkey_display",
                "hotkey_is_bare_printable",
                "last_error",
                "manual_controls_enabled",
                "mode",
                "muted",
                "notifications",
                "recording",
                "selected_device",
                "volume",
            ]
        );
    }

    /// A machine with no working audio stack must still produce a renderable
    /// snapshot rather than erroring the `get_state` command.
    #[test]
    fn snapshot_degrades_gracefully_with_no_microphone() {
        let snapshot = unavailable_core().snapshot();
        assert!(snapshot.devices.is_empty());
        assert!(!snapshot.muted);
        assert_eq!(snapshot.volume, 1.0);
        assert_eq!(snapshot.mode, Mode::MuteToggle);
        assert!(snapshot.manual_controls_enabled);
        assert!(!snapshot.recording);
    }

    #[test]
    fn snapshot_reports_the_recorded_hotkey_and_its_bare_printable_warning() {
        let mut core = unavailable_core();
        core.config.hotkey = Some(Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0x4D });
        let snapshot = core.snapshot();
        assert_eq!(snapshot.hotkey_display.as_deref(), Some("M"));
        assert!(snapshot.hotkey_is_bare_printable, "bare M must carry the warning flag");
    }

    #[test]
    fn record_outcome_sets_on_failure_and_clears_on_the_next_success() {
        let mut core = unavailable_core();

        core.record_outcome(&Err::<(), _>(AudioError::ThreadTerminated));
        assert_eq!(core.last_error.as_deref(), Some("audio thread terminated"));
        assert_eq!(core.snapshot().last_error.as_deref(), Some("audio thread terminated"));

        core.record_outcome(&Ok::<_, AudioError>(()));
        assert_eq!(core.last_error, None);
    }

    /// Snapshotting must not disturb `last_error`, or the `state-changed`
    /// emitted immediately after a failed command would erase the error it
    /// exists to deliver.
    #[test]
    fn snapshot_does_not_clear_a_recorded_error() {
        let mut core = unavailable_core();
        core.record_outcome(&Err::<(), _>(AudioError::NoDevice));
        let _ = core.snapshot();
        assert_eq!(core.last_error.as_deref(), Some("no capture device available"));
    }

    #[test]
    fn an_unavailable_mic_errors_every_call_instead_of_pretending_to_work() {
        let mut mic = Mic::Unavailable;
        assert!(matches!(mic.list_devices(), Err(AudioError::NoDevice)));
        assert!(matches!(mic.is_muted(), Err(AudioError::NoDevice)));
        assert!(matches!(mic.volume(), Err(AudioError::NoDevice)));
        assert!(matches!(mic.peak(), Err(AudioError::NoDevice)));
        assert!(matches!(mic.select(Some("x")), Err(AudioError::NoDevice)));
        assert!(matches!(mic.set_muted(true), Err(AudioError::NoDevice)));
        assert!(matches!(mic.set_volume(0.5), Err(AudioError::NoDevice)));
        assert!(mic.tap().is_none(), "there is no worker to meter");
    }

    /// A poisoned `Core` must stay usable: every Tauri command handler goes
    /// through `lock_or_recover`, and one unrelated panic must not brick them
    /// all.
    #[test]
    fn lock_or_recover_survives_a_poisoned_mutex() {
        let shared: Shared = Mutex::new(unavailable_core());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = lock_or_recover(&shared);
            guard.last_error = Some("set before the panic".into());
            panic!("poison it");
        }));
        assert!(shared.is_poisoned(), "sanity: the panic must have poisoned the lock");
        assert_eq!(lock_or_recover(&shared).last_error.as_deref(), Some("set before the panic"));
    }

    #[test]
    fn spawn_with_timeout_returns_a_fast_constructors_value() {
        let value = spawn_with_timeout(Duration::from_secs(5), || Ok(7u32)).unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn spawn_with_timeout_propagates_a_constructor_error_verbatim() {
        let result = spawn_with_timeout(Duration::from_secs(5), || {
            Err::<u32, _>(AudioError::NoDevice)
        });
        assert!(matches!(result, Err(AudioError::NoDevice)), "got {result:?}");
    }

    /// The property the amendment asked for: a wedged audio service must
    /// produce a diagnostic, not a process that never starts.
    #[test]
    fn spawn_with_timeout_gives_up_on_a_wedged_constructor() {
        let result = spawn_with_timeout(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(2));
            Ok(0u32)
        });
        match result {
            Err(AudioError::Windows(msg)) => {
                assert!(msg.contains("did not respond"), "unhelpful message: {msg}");
            }
            other => panic!("expected a timeout error, got {other:?}"),
        }
    }
}

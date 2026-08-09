//! Application state: the runtime the Tauri commands mutate, and the flat
//! snapshot the frontend renders.
//!
//! # Locking discipline
//!
//! Two mutexes guard mutable app state: [`Shared`] (`Mutex<Core>`) and
//! [`Meter`] (`Mutex<MeterHandle>`). Four rules keep them deadlock-free, and
//! every one of them is load-bearing.
//!
//! **1. Lock order is `Core` before [`Meter`]. Never the reverse.**
//! Today nothing holds both at once — `lib.rs`'s `setup` releases the `Core`
//! guard before taking the meter's. That will not survive Task 10, which wires
//! `MeterHandle::start`/`stop` to window show/hide: getting the [`MeterTap`] to
//! start with requires `Core`, so that code *will* hold both. When it does, it
//! must take them in this order, because there is no ordering to discover from
//! the code once two call sites disagree.
//!
//! **2. The audio worker never takes the `Core` lock.** It owns a `!Send` Core
//! Audio endpoint and services commands over a channel
//! ([`crate::audio::thread`]); it has no `AppHandle` and no way to reach
//! `Core`. So holding `Core` while blocking on an audio reply cannot cycle —
//! the reply does not need `Core` to be produced. Those replies are bounded by
//! [`crate::audio::thread::COMMAND_TIMEOUT`] anyway, so even a wedged worker
//! releases the lock eventually.
//!
//! **3. The 30Hz level meter never takes the `Core` lock either.** It polls
//! through a [`MeterTap`], which is a bare channel sender onto the same worker.
//! Keep it that way: a meter that needed `Core` would be taking the lock 30
//! times a second against every command handler.
//!
//! **4. [`emit_state`] takes the lock to build its snapshot**, so it must never
//! be called while a guard is already held. Command bodies scope their guard in
//! an inner block and call it after; the hook dispatch loop returns a
//! description of its follow-up work instead of doing it inline.
//!
//! One extra caution for Task 10: `MeterHandle::start` blocks on an audio
//! round-trip (it opens the capture stream) and `stop` joins the poll thread.
//! Doing either on the UI thread while holding `Core` compounds every other
//! cost on this list — prefer releasing `Core` first, which is possible because
//! a [`MeterTap`] is cloneable and can be taken out of the guard.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::audio::meter::MeterHandle;
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
///
/// **The device *list* is deliberately not here.** This struct is rebuilt and
/// emitted on every state change, which includes every push-to-talk keypress.
/// Enumerating endpoints costs `GetDefaultAudioEndpoint` +
/// `EnumAudioEndpoints` + a friendly-name property read per device, all on the
/// single-threaded audio worker that is simultaneously serving the 30Hz meter,
/// with the `Core` lock held throughout. The list changes on hotplug, not on
/// keypress, so it belongs behind the separate `list_devices` command
/// (DESIGN.md §3) that the frontend calls on window open and on
/// `devices-changed`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppState {
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
    ///
    /// **Dismissal semantics, because they are not what a reader assumes:**
    /// this clears only when a *subsequent fallible operation succeeds* — an
    /// audio call, or the autostart registry write. Commands that cannot fail
    /// — `set_notification_prefs`, `clear_hotkey`, `begin_hotkey_recording`,
    /// `cancel_hotkey_recording` — leave it exactly as they found it, so an
    /// error stays pinned across any number of them until something actually
    /// talks to the device again. There is no user-dismiss path and no
    /// timestamp. Task 14 should render it as "the last thing that went wrong",
    /// not as "something is wrong right now".
    pub last_error: Option<String>,
}

pub struct Core {
    pub machine: ModeMachine<Mic>,
    pub config: Config,
    /// Where [`Self::persist`] writes. Held rather than re-derived from
    /// `config_dir()` on every save so that the destination is injectable —
    /// which is what lets the tests below exercise the persisting paths
    /// without writing to the developer's real `%APPDATA%`.
    pub config_dir: PathBuf,
    pub recorder: Recorder,
    /// See [`AppState::last_error`] for the dismissal semantics.
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

    /// Re-points the microphone at `id` and re-asserts the mode's resting
    /// state on the device that just arrived.
    ///
    /// The whole of `commands::set_device` minus the Tauri plumbing, so the
    /// sequencing below is unit-testable rather than only reachable through an
    /// IPC call.
    ///
    /// The `reapply_resting_state` is the load-bearing half. A newly selected
    /// endpoint sits at whatever mute state Windows last left it in, while the
    /// old one keeps the state the machine set. Without the reassertion, a
    /// device change in Push to Talk leaves the *old* device muted and the
    /// *new* one live — the user believes they are muted at rest and is hot.
    pub fn select_device(&mut self, id: Option<String>) {
        let result = self.machine.mic_mut().select(id.as_deref());
        if result.is_ok() {
            self.machine.reapply_resting_state();
        }
        self.record_outcome(&result);
        // Persisted even when the selection failed: the user asked for this
        // device, and one that is merely unplugged right now should come back
        // on the next launch rather than being silently forgotten.
        self.config.device_id = id;
        self.persist();
    }

    /// Persists config. Failures are logged, never fatal — a read-only
    /// `%APPDATA%` must not take the app down (DESIGN.md §6).
    pub fn persist(&self) {
        if let Err(e) = self.config.save(&self.config_dir) {
            eprintln!("mugon: failed to save config: {e}");
        }
    }
}

pub type Shared = Mutex<Core>;

/// The level meter's managed state.
///
/// It has a name so the lock ordering in this module's docs has something to
/// refer to: **`Shared` is taken before `Meter`, never the other way round.**
pub type Meter = Mutex<MeterHandle>;

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

/// A `Core` backed by a worker-hosted [`crate::audio::fake::FakeMic`], plus the
/// temporary directory its `persist` writes into.
///
/// Lives outside `mod tests` because `lib.rs`'s tests need it too — the hook
/// dispatch logic is a function of `&mut Core`, and testing it against a real
/// endpoint is impossible while testing it against no endpoint at all would
/// only exercise the error paths.
///
/// The `TempDir` must be kept alive by the caller: dropping it deletes the
/// directory out from under any later `persist`.
#[cfg(test)]
pub(crate) fn fake_core(mode: Mode) -> (Core, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let handle = MicHandle::spawn_with(|| Ok(crate::audio::fake::FakeMic::new()))
        .expect("the fake worker must spawn");
    let core = Core {
        machine: ModeMachine::new(Mic::Live(handle), mode),
        config: Config { mode, ..Config::default() },
        config_dir: dir.path().to_path_buf(),
        recorder: Recorder::default(),
        last_error: None,
    };
    (core, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;

    fn unavailable_core() -> Core {
        Core {
            machine: ModeMachine::new(Mic::Unavailable, Mode::MuteToggle),
            config: Config::default(),
            config_dir: std::env::temp_dir().join("mugon-test-never-written"),
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
        assert!(
            !object.contains_key("devices"),
            "the device list must not ride on the hot snapshot — it costs a full \
             COM enumeration on every push-to-talk keypress. Use `list_devices`."
        );
    }

    /// A machine with no working audio stack must still produce a renderable
    /// snapshot rather than erroring the `get_state` command.
    #[test]
    fn snapshot_degrades_gracefully_with_no_microphone() {
        let snapshot = unavailable_core().snapshot();
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

    /// Fix 1's property, end to end through the same code path
    /// `commands::set_device` uses.
    ///
    /// `FakeMic::select` cannot model a new endpoint carrying its own mute
    /// state, so the live state is forced through the machine first; what is
    /// under test is that `select_device` re-asserts the resting state
    /// afterwards rather than leaving whatever the new device came with.
    #[test]
    fn selecting_a_device_in_ptt_leaves_the_new_device_muted() {
        let (mut core, _dir) = fake_core(Mode::PushToTalk);
        assert!(core.machine.mic().is_muted().unwrap(), "PTT rests muted");

        // Simulate the newly selected endpoint arriving live.
        core.machine.mic_mut().set_muted(false).unwrap();

        core.select_device(Some("device-2".into()));

        assert!(
            core.machine.mic().is_muted().unwrap(),
            "a device change in PTT must not leave the new device hot"
        );
        assert_eq!(core.config.device_id.as_deref(), Some("device-2"));
        assert_eq!(core.last_error, None);
    }

    #[test]
    fn selecting_a_device_in_mute_toggle_leaves_the_new_device_live() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        core.machine.mic_mut().set_muted(true).unwrap();

        core.select_device(None);

        assert!(!core.machine.mic().is_muted().unwrap(), "Mute Toggle rests unmuted");
        assert_eq!(core.config.device_id, None);
    }

    /// The user's choice survives a device that is not currently present, so a
    /// replug restores it rather than silently reverting to the default.
    #[test]
    fn a_failed_selection_still_persists_the_users_choice_and_records_why() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);
        core.machine = ModeMachine::new(Mic::Unavailable, Mode::MuteToggle);

        core.select_device(Some("unplugged-device".into()));

        assert_eq!(core.config.device_id.as_deref(), Some("unplugged-device"));
        assert_eq!(core.last_error.as_deref(), Some("no capture device available"));
        assert_eq!(
            Config::load(dir.path()).device_id.as_deref(),
            Some("unplugged-device"),
            "the choice must survive to the next launch"
        );
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

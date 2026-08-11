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
//! The one place that touches both is `lib.rs`'s `start_metering`, which the
//! window lifecycle calls on every window create: getting the [`MeterTap`] to
//! start with requires `Core`. It takes `Core` first, clones the tap out, and
//! lets that guard die *before* taking the meter lock — so in practice the two
//! are never held simultaneously either. Any future call site must take them in
//! this order, because there is no ordering to discover from the code once two
//! of them disagree.
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
//! One extra caution, and the reason rule 1 is written the way it is:
//! `MeterHandle::start` blocks on an audio round-trip (it opens the capture
//! stream) and `stop` joins the poll thread. Both run on the UI thread from the
//! window handlers, and doing either while holding `Core` would compound every
//! other cost on this list — hence releasing `Core` first, which is possible
//! only because a [`MeterTap`] is cloneable and can be taken out of the guard.

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
use crate::hotkey::{keys, Hotkey};
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

    /// Starts device-hotplug notifications (§4.5).
    ///
    /// Not part of [`MicBackend`]: it is a one-shot startup wiring step, not
    /// an operation on the microphone, and a machine with no working audio
    /// stack has nothing to watch — hence the plain [`AudioError::NoDevice`]
    /// rather than a silent success that would look like it worked.
    pub fn enable_hotplug(&self, app: AppHandle) -> Result<(), AudioError> {
        match self {
            Mic::Live(handle) => handle.enable_hotplug(app),
            Mic::Unavailable => Err(AudioError::NoDevice),
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
    pub hotkey_is_bare_printable: bool,
    /// The binding broken into the parts the picker's controls bind to.
    ///
    /// There is deliberately **no** `hotkey_display` alongside this. Task 17
    /// added these parts expecting the two to coexist, and the formatted label
    /// turned out to have no remaining consumer: the combo the user reads is
    /// composed into [`Self::hotkey_status`] by [`Core::hotkey_status`]. A
    /// second `String` built on every push-to-talk key edge for nobody is the
    /// same cost this struct evicted the device list over.
    pub hotkey: Option<HotkeyParts>,
    /// The `HOTKEY STATUS` line and its kind. See [`Core::hotkey_status`].
    pub hotkey_status: HotkeyStatus,
    pub manual_controls_enabled: bool,
    pub notifications: NotificationPrefs,
    pub autostart: bool,
    /// The last device failure, or `None` if the last audio call succeeded.
    /// One field, no history, no severity — enough for the UI to say "that
    /// didn't work, and here's why" instead of nothing at all.
    ///
    /// **Dismissal semantics, because they are not what a reader assumes:**
    /// this clears only when a *subsequent fallible operation succeeds* — an
    /// audio call, the autostart registry write, or a `set_hotkey` that
    /// validates. Commands that cannot fail — `set_notification_prefs`,
    /// `list_bindable_keys` — leave it exactly as they found it, so an
    /// error stays pinned across any number of them until something actually
    /// succeeds. There is no user-dismiss path and no
    /// timestamp. Task 14 should render it as "the last thing that went wrong",
    /// not as "something is wrong right now".
    pub last_error: Option<String>,
    /// Why the keyboard hook is not running, or `None` while it is.
    ///
    /// **Deliberately not folded into [`Self::last_error`]**, though it is also
    /// an error string. The two have opposite lifetimes: `last_error` is "the
    /// last thing that went wrong" and clears on the next successful audio
    /// call, whereas a hook that failed to install is a *standing condition*
    /// that nothing in the app can recover from. Routed through `last_error`
    /// it would be wiped by the very next mute read — a few hundred
    /// milliseconds later — and the user would be left with a fully populated
    /// settings window, a bound hotkey, and no indication whatsoever that the
    /// hotkey does nothing.
    ///
    /// DESIGN.md §7 calls for a blocking error in the UI here, because without
    /// the hook the app's core function is dead. Tasks 10 and 14 each recorded
    /// that the other would surface it; neither did, and it went unreported
    /// until the whole-branch review.
    pub hook_error: Option<String>,
}

/// Which of the four `HOTKEY STATUS` states applies.
///
/// **Travels alongside the label rather than being derived from it.** The
/// frontend styles off this; without it the only way to colour the line is to
/// prefix-match the label, which turns [`HotkeyStatus::label`] into an
/// undeclared enum — reword the copy and the colouring silently stops applying,
/// with every test on both sides still green. task-14-amendment §2 forbids the
/// frontend parsing Rust-owned display strings, and this is what makes obeying
/// it possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HotkeyStatusKind {
    Inactive,
    NotSet,
    Bound,
    Confirmed,
}

/// The `HOTKEY STATUS` line: what to render, and what it means.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HotkeyStatus {
    pub kind: HotkeyStatusKind,
    pub label: String,
}

/// A binding as the picker's controls see it: four booleans and a key name.
///
/// The key travels by **name**, never as a raw VK, for the same reason the
/// config file does — a VK is meaningless to the frontend, and a name is
/// exactly what `list_bindable_keys` offers and what `set_hotkey` accepts, so
/// the value round-trips through the dropdown unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HotkeyParts {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: String,
}

pub struct Core {
    pub machine: ModeMachine<Mic>,
    pub config: Config,
    /// Where [`Self::persist`] writes. Held rather than re-derived from
    /// `config_dir()` on every save so that the destination is injectable —
    /// which is what lets the tests below exercise the persisting paths
    /// without writing to the developer's real `%APPDATA%`.
    pub config_dir: PathBuf,
    /// See [`AppState::last_error`] for the dismissal semantics.
    pub last_error: Option<String>,
    /// See [`AppState::hook_error`]. Set once, by the hook thread, if
    /// `SetWindowsHookExW` fails; never cleared, because nothing can undo it
    /// short of a restart.
    pub hook_error: Option<String>,
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
            hotkey_is_bare_printable: self
                .config
                .hotkey
                .map(|h| h.is_bare_printable())
                .unwrap_or(false),
            // `vk_to_name` cannot fail here: a `Hotkey` only ever exists via
            // `set_hotkey` or `Deserialize`, and both resolve the name through
            // this same table before constructing one.
            hotkey: self.config.hotkey.map(|h| HotkeyParts {
                ctrl: h.ctrl,
                alt: h.alt,
                shift: h.shift,
                win: h.win,
                key: keys::vk_to_name(h.vk).unwrap_or("?").to_string(),
            }),
            hotkey_status: self.hotkey_status(),
            manual_controls_enabled: self.machine.manual_controls_enabled(),
            notifications: self.config.notifications.clone(),
            autostart: self.config.autostart,
            last_error: self.last_error.clone(),
            hook_error: self.hook_error.clone(),
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

    /// Re-reads mute state to find out whether the endpoint is still
    /// answering, and hands the answer back.
    ///
    /// [`ModeMachine`] swallows device errors by design (DESIGN.md §7 — a
    /// failed Core Audio call must never panic the state machine), so a mute
    /// change that silently failed would otherwise leave the user with no
    /// signal whatsoever. This is the cheapest honest probe: if the worker is
    /// dead or the endpoint is broken, this read fails too.
    ///
    /// The returned `bool` is the same "unreadable counts as unmuted" default
    /// [`Self::snapshot`] uses. It exists so callers that need the new mute
    /// state — the tray, to pick an icon — do not have to ask the worker a
    /// second time for an answer this call already had.
    pub fn refresh_mic_health(&mut self) -> bool {
        let result = self.machine.mic().is_muted();
        self.record_outcome(&result);
        result.unwrap_or(false)
    }

    /// Switches mode, persists the choice, and reports the mute state it left
    /// behind.
    ///
    /// The whole of `commands::set_mode` minus the Tauri plumbing, and the
    /// whole of the tray's Mode submenu minus its menu bookkeeping — which is
    /// the point. Both call sites need the same three things done in the same
    /// order (`ModeMachine::set_mode`, which re-asserts the resting state;
    /// record the choice; persist it) and differ only in what they do
    /// *afterwards*. Two copies of that would drift.
    pub fn apply_mode(&mut self, mode: Mode) -> bool {
        self.machine.set_mode(mode);
        self.config.mode = mode;
        self.persist();
        self.refresh_mic_health()
    }

    /// The manual mute switch (§4.1), reporting the mute state it left behind.
    /// A no-op in Push to Talk — [`ModeMachine::toggle_manual`] enforces that,
    /// not this.
    ///
    /// Same rationale as [`Self::apply_mode`]: the IPC command and the tray
    /// menu item are the same operation with different follow-ups.
    pub fn toggle_mute(&mut self) -> bool {
        self.machine.toggle_manual();
        self.refresh_mic_health()
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
    ///
    /// Returns the mute state it left behind, for the same reason
    /// [`Self::apply_mode`] does: `reapply_resting_state` can flip mute, and the
    /// tray icon has to follow.
    pub fn select_device(&mut self, id: Option<String>) -> bool {
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
        // A bare read, deliberately **not** [`Self::refresh_mic_health`]. That
        // would fold this probe into `last_error` and, on a failed selection
        // where the endpoint still answers, clear the very "that device isn't
        // there" message `record_outcome` just stored — see
        // `a_failed_selection_still_persists_the_users_choice_and_records_why`.
        // The select's own reason is the more useful one, so it stands.
        self.machine.mic().is_muted().unwrap_or(false)
    }

    /// The picker's write path (Task 17), and the **second enforcement point**
    /// for "a binding's key is never a modifier". The first is `Hotkey`'s
    /// `Deserialize`, which guards the config file; this guards IPC. Both
    /// entrances to the same field need the same rule, or the one without it
    /// becomes the way around the other.
    ///
    /// `key: None` clears the binding.
    ///
    /// **Every call resets [`Config::hotkey_confirmed`]**, including one that
    /// merely adds a modifier to an existing key: `Ctrl + F16` is a different
    /// combo from `F16` and has not been observed just because `F16` was.
    ///
    /// Split out from the command for the same reason [`Self::apply_mode`] is —
    /// it makes the sequencing testable rather than reachable only through IPC.
    pub fn set_hotkey(
        &mut self,
        ctrl: bool,
        alt: bool,
        shift: bool,
        win: bool,
        key: Option<&str>,
    ) -> Result<(), String> {
        // Routed through `last_error` as well as the `Result`, exactly as
        // `set_autostart` does: the frontend has no error surface of its own —
        // the banner renders `last_error` — so a rejection returned and nowhere
        // else is as silent as no rejection at all.
        let binding = match self.validate(ctrl, alt, shift, win, key) {
            Ok(binding) => {
                self.last_error = None;
                binding
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                return Err(e);
            }
        };
        self.config.hotkey = binding;
        self.config.hotkey_confirmed = false;
        self.persist();
        Ok(())
    }

    /// Resolves a picker selection into a binding, or says why it cannot.
    ///
    /// Split out to keep [`Self::set_hotkey`] legible now that it also does
    /// `last_error` bookkeeping around the write — the validation rules and the
    /// error routing read as two separate things because they are. Taking
    /// `&self` also makes it structurally unable to touch stored state on the
    /// way to a rejection.
    fn validate(
        &self,
        ctrl: bool,
        alt: bool,
        shift: bool,
        win: bool,
        key: Option<&str>,
    ) -> Result<Option<Hotkey>, String> {
        let Some(name) = key else {
            return Ok(None);
        };
        let vk = keys::name_to_vk(name).ok_or_else(|| format!("{name:?} is not a bindable key"))?;
        if keys::is_modifier(vk) {
            return Err(format!("{name:?} is a modifier and cannot be the bound key"));
        }
        Ok(Some(Hotkey { ctrl, alt, shift, win, vk }))
    }

    /// Records that the hook has actually observed the bound combo firing.
    ///
    /// Returns whether this call was the *transition*. That return is
    /// load-bearing: without it the caller would persist on every matching
    /// keypress, which in Push to Talk is a config write per key edge — two per
    /// press, for the life of the binding.
    pub fn confirm_hotkey(&mut self) -> bool {
        if self.config.hotkey.is_none() || self.config.hotkey_confirmed {
            return false;
        }
        self.config.hotkey_confirmed = true;
        self.persist();
        true
    }

    /// The `HOTKEY STATUS` line, composed here rather than in TypeScript
    /// because task-14-amendment §2 puts display strings on the Rust side: the
    /// frontend renders them and never parses them. [`HotkeyStatus::kind`] is
    /// what makes obeying that possible — it gives the frontend something to
    /// switch on that is not the label's wording.
    ///
    /// **`Inactive` outranks everything.** With no hook there is no path by
    /// which any binding can ever be confirmed, so prompting the user to press
    /// a key would be instructing them to do something that cannot work. It
    /// deliberately overlaps the error banner — the banner is easy to miss, and
    /// this is the row the user is actually looking at.
    pub fn hotkey_status(&self) -> HotkeyStatus {
        let (kind, label) = if self.hook_error.is_some() {
            (HotkeyStatusKind::Inactive, "Inactive — keyboard hook blocked".to_string())
        } else {
            match self.config.hotkey {
                None => (HotkeyStatusKind::NotSet, "Not set".to_string()),
                Some(_) if self.config.hotkey_confirmed => {
                    (HotkeyStatusKind::Confirmed, "Confirmed".to_string())
                }
                Some(hk) => (
                    HotkeyStatusKind::Bound,
                    format!("Bound — press {} to confirm", hk.display()),
                ),
            }
        };
        HotkeyStatus { kind, label }
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
        last_error: None,
        hook_error: None,
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
            last_error: None,
            hook_error: None,
        }
    }

    // ---- Task 17: the hotkey picker ----

    #[test]
    fn set_hotkey_stores_and_persists_a_binding() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);

        core.set_hotkey(true, false, true, false, Some("F16")).unwrap();

        assert_eq!(
            core.config.hotkey.map(|h| h.display()).as_deref(),
            Some("Ctrl + Shift + F16")
        );
        assert_eq!(
            Config::load(dir.path()).hotkey.map(|h| h.display()).as_deref(),
            Some("Ctrl + Shift + F16"),
            "the choice must survive to the next launch"
        );
    }

    #[test]
    fn set_hotkey_with_no_key_clears_the_binding() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);
        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();

        core.set_hotkey(false, false, false, false, None).unwrap();

        assert_eq!(core.config.hotkey, None);
        assert_eq!(Config::load(dir.path()).hotkey, None, "clearing must persist too");
    }

    /// The same invariant `Hotkey`'s `Deserialize` enforces at the config-file
    /// entrance, enforced at the IPC entrance. A binding whose key is a
    /// modifier fires the bound action on every shortcut pressed all day.
    #[test]
    fn set_hotkey_rejects_a_modifier_as_the_bound_key() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);

        assert!(core.set_hotkey(false, false, false, false, Some("LeftCtrl")).is_err());

        assert_eq!(core.config.hotkey, None, "a rejected binding must not be stored");
    }

    #[test]
    fn set_hotkey_rejects_a_key_that_is_not_offered() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);

        assert!(core.set_hotkey(false, false, false, false, Some("Nonsense")).is_err());
        assert!(
            core.set_hotkey(false, false, false, false, Some("Escape")).is_err(),
            "Escape is the universal way out and is not bindable"
        );
    }

    /// A new binding has never been seen. Carrying the old confirmation over
    /// would have the UI claim "Confirmed" about a key nothing has observed.
    #[test]
    fn set_hotkey_resets_the_confirmation() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();
        core.confirm_hotkey();

        core.set_hotkey(false, false, false, false, Some("F17")).unwrap();

        assert!(!core.config.hotkey_confirmed);
    }

    /// Guards the persist. Without the transition check every press of a
    /// confirmed hotkey rewrites `config.json` — in Push to Talk that is a disk
    /// write per keypress, twice.
    #[test]
    fn confirming_is_a_one_time_transition() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);
        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();

        assert!(core.confirm_hotkey(), "the first sighting confirms");
        assert!(!core.confirm_hotkey(), "a second sighting changes nothing");

        assert!(Config::load(dir.path()).hotkey_confirmed, "confirmation must survive a restart");
    }

    #[test]
    fn confirming_without_a_binding_does_nothing() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);

        assert!(!core.confirm_hotkey());
        assert!(!core.config.hotkey_confirmed);
    }

    /// A rejected binding must reach the user. `set_hotkey` returns the reason,
    /// but the frontend has no error surface of its own — the banner renders
    /// `last_error`, so a `Result` nobody routes there is exactly as silent as
    /// no `Result` at all. Same rule `set_autostart` follows.
    #[test]
    fn a_rejected_binding_records_why_in_last_error() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);

        assert!(core.set_hotkey(false, false, false, false, Some("Nonsense")).is_err());

        let message = core.last_error.as_deref().expect("the rejection must be visible");
        assert!(message.contains("Nonsense"), "the message must name the key: {message}");
    }

    #[test]
    fn a_successful_binding_clears_a_previous_rejection() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        let _ = core.set_hotkey(false, false, false, false, Some("Nonsense"));

        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();

        assert_eq!(core.last_error, None);
    }

    #[test]
    fn hotkey_status_reads_not_set_with_no_binding() {
        let (core, _dir) = fake_core(Mode::MuteToggle);
        assert_eq!(core.hotkey_status().label, "Not set");
        assert_eq!(core.hotkey_status().kind, HotkeyStatusKind::NotSet);
    }

    /// The kind is what the frontend styles off. It exists so the accent colour
    /// is not derived by prefix-matching the label — reword the label and the
    /// colouring would silently stop applying, with every test still green.
    #[test]
    fn every_hotkey_status_kind_travels_with_its_label() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        assert_eq!(core.hotkey_status().kind, HotkeyStatusKind::NotSet);

        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();
        assert_eq!(core.hotkey_status().kind, HotkeyStatusKind::Bound);

        core.confirm_hotkey();
        assert_eq!(core.hotkey_status().kind, HotkeyStatusKind::Confirmed);

        core.hook_error = Some("blocked".into());
        assert_eq!(core.hotkey_status().kind, HotkeyStatusKind::Inactive);
    }

    /// The prompt carries the combo so it reads as an instruction rather than a
    /// label — the user has to know *what* to press.
    #[test]
    fn hotkey_status_prompts_for_a_confirming_press() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();

        assert_eq!(core.hotkey_status().label, "Bound — press F16 to confirm");
    }

    #[test]
    fn hotkey_status_reads_confirmed_once_the_hook_has_seen_it() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();

        core.confirm_hotkey();

        assert_eq!(core.hotkey_status().label, "Confirmed");
    }

    /// Precedence, and the reason it exists: with a dead hook, confirmation is
    /// impossible. Telling the user to press F16 to confirm would be
    /// instructing them to do something that cannot work, forever.
    #[test]
    fn a_dead_hook_outranks_every_other_hotkey_status() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        core.hook_error = Some("Windows blocked the keyboard hook".into());

        assert_eq!(core.hotkey_status().label, "Inactive — keyboard hook blocked");

        core.set_hotkey(false, false, false, false, Some("F16")).unwrap();
        assert_eq!(core.hotkey_status().label, "Inactive — keyboard hook blocked");

        core.confirm_hotkey();
        assert_eq!(core.hotkey_status().label, "Inactive — keyboard hook blocked");
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
                "hook_error",
                "hotkey",
                "hotkey_is_bare_printable",
                "hotkey_status",
                "last_error",
                "manual_controls_enabled",
                "mode",
                "muted",
                "notifications",
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

    /// The nested shapes the picker binds to, pinned by **value**, not just by
    /// key.
    ///
    /// `app_state_wire_format_is_pinned` above checks top-level field names
    /// only, which leaves the contract this task actually depends on unguarded:
    /// the frontend switches its styling on `hotkey_status.kind` and its
    /// controls on `hotkey.*`. Add a `#[serde(rename_all)]` or rename a variant
    /// and everything still compiles, every other test stays green, and the
    /// accent colour silently stops applying — the exact failure mode the
    /// `HotkeyStatus { kind, label }` split was introduced to remove, relocated
    /// from the label's wording to the variant's name.
    #[test]
    fn the_hotkey_wire_shapes_the_picker_switches_on_are_pinned() {
        let mut core = unavailable_core();

        let value = serde_json::to_value(core.snapshot()).unwrap();
        assert_eq!(value["hotkey_status"]["kind"], "NotSet");
        assert_eq!(value["hotkey_status"]["label"], "Not set");
        assert_eq!(value["hotkey"], serde_json::Value::Null);

        core.config.hotkey =
            Some(Hotkey { ctrl: true, alt: false, shift: false, win: false, vk: 0x7F });
        let value = serde_json::to_value(core.snapshot()).unwrap();

        assert_eq!(value["hotkey_status"]["kind"], "Bound");
        let hotkey = value["hotkey"].as_object().expect("hotkey must be an object");
        let mut keys: Vec<&str> = hotkey.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["alt", "ctrl", "key", "shift", "win"]);
        assert_eq!(hotkey["key"], "F16");
        assert_eq!(hotkey["ctrl"], true);

        core.config.hotkey_confirmed = true;
        assert_eq!(
            serde_json::to_value(core.snapshot()).unwrap()["hotkey_status"]["kind"],
            "Confirmed"
        );

        core.hook_error = Some("blocked".into());
        assert_eq!(
            serde_json::to_value(core.snapshot()).unwrap()["hotkey_status"]["kind"],
            "Inactive"
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
        assert_eq!(snapshot.hotkey, None);
    }

    /// The picker's controls render off these parts. They exist so the four
    /// chips and the dropdown have raw values to bind to — the alternative is
    /// recovering them by parsing a formatted label in TypeScript, which
    /// task-14-amendment §2 forbids.
    #[test]
    fn snapshot_carries_the_binding_in_parts_for_the_picker() {
        let mut core = unavailable_core();
        core.config.hotkey =
            Some(Hotkey { ctrl: true, alt: false, shift: true, win: false, vk: 0x7F });

        let parts = core.snapshot().hotkey.expect("a bound hotkey must reach the picker");

        assert!(parts.ctrl);
        assert!(!parts.alt);
        assert!(parts.shift);
        assert!(!parts.win);
        assert_eq!(parts.key, "F16");
    }

    #[test]
    fn snapshot_reports_the_bound_hotkey_and_its_bare_printable_warning() {
        let mut core = unavailable_core();
        core.config.hotkey = Some(Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: 0x4D });
        let snapshot = core.snapshot();
        assert_eq!(snapshot.hotkey.map(|h| h.key).as_deref(), Some("M"));
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

        let muted = core.select_device(Some("device-2".into()));

        assert!(muted, "the caller needs the new mute state to pick a tray icon");
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

        let muted = core.select_device(None);

        assert!(!muted, "the reported state must match the device, not the old icon");
        assert!(!core.machine.mic().is_muted().unwrap(), "Mute Toggle rests unmuted");
        assert_eq!(core.config.device_id, None);
    }

    /// The user's choice survives a device that is not currently present, so a
    /// replug restores it rather than silently reverting to the default.
    #[test]
    fn a_failed_selection_still_persists_the_users_choice_and_records_why() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);
        core.machine = ModeMachine::new(Mic::Unavailable, Mode::MuteToggle);

        let _muted = core.select_device(Some("unplugged-device".into()));

        assert_eq!(core.config.device_id.as_deref(), Some("unplugged-device"));
        // The *select's* reason, not the follow-up mute probe's. `select_device`
        // reads mute for the tray icon without recording it precisely so this
        // stays the more specific message.
        assert_eq!(core.last_error.as_deref(), Some("no capture device available"));
        assert_eq!(
            Config::load(dir.path()).device_id.as_deref(),
            Some("unplugged-device"),
            "the choice must survive to the next launch"
        );
    }

    /// `apply_mode` backs both `commands::set_mode` and the tray's Mode
    /// submenu, so all three of its jobs — apply the resting state, report the
    /// mute state back, and persist the choice — are asserted here rather than
    /// at either call site.
    #[test]
    fn apply_mode_mutes_reports_and_persists_on_entering_push_to_talk() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);
        assert!(!core.machine.mic().is_muted().unwrap(), "Mute Toggle rests unmuted");

        let muted = core.apply_mode(Mode::PushToTalk);

        assert!(muted, "the caller needs the new mute state to pick a tray icon");
        assert!(core.machine.mic().is_muted().unwrap());
        assert_eq!(core.config.mode, Mode::PushToTalk);
        assert_eq!(
            Config::load(dir.path()).mode,
            Mode::PushToTalk,
            "the mode must survive to the next launch"
        );
    }

    #[test]
    fn toggle_mute_reports_the_state_it_left_behind() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        assert!(core.toggle_mute(), "the first toggle mutes");
        assert!(!core.toggle_mute(), "the second unmutes");
    }

    #[test]
    fn toggle_mute_is_a_no_op_in_push_to_talk() {
        let (mut core, _dir) = fake_core(Mode::PushToTalk);
        assert!(core.toggle_mute(), "PTT rests muted; the manual switch must not move it");
        assert!(core.machine.mic().is_muted().unwrap());
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

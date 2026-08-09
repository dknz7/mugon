//! Application wiring.
//!
//! # Thread topology
//!
//! | Thread | Owns | Notes |
//! |---|---|---|
//! | UI (`main`) | the Tauri event loop, the webview | tao puts this in an **STA**, which is why no COM audio object may live here |
//! | `mugon-audio` | the `!Send` Core Audio `Endpoint` and the WASAPI capture stream | MTA; serves commands over a channel ([`audio::thread`]) |
//! | `mugon-audio-startup` | nothing after it reports | throwaway; exists only so a wedged audio service becomes a timeout instead of a hang |
//! | `mugon-hook` | the `WH_KEYBOARD_LL` hook and its message pump | [`hotkey::hook::install`] blocks forever by design |
//! | `mugon-hook-dispatch` | routing hook events into the recorder or the mode machine | **must not** be the hook thread: a slow Core Audio call there makes Windows silently uninstall the hook |
//! | `mugon-meter` | the 30Hz `level` poll loop | polls through a `MeterTap`, never touches the `Core` lock |
//!
//! See [`state`]'s module docs for the locking discipline that keeps those
//! last three from deadlocking against each other.

pub mod audio;
pub mod autostart;
pub mod commands;
pub mod config;
pub mod hotkey;
pub mod modes;
pub mod notify;
pub mod state;
pub mod tray;

use std::sync::mpsc;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};

use audio::meter::MeterHandle;
use audio::MicBackend;
use config::{Config, NotificationPrefs};
use hotkey::hook::{self, HookEvent};
use hotkey::recorder::{Recorder, RecorderOutcome};
use modes::{KeyEdge, Mode, ModeMachine};
use state::{emit_state, lock_or_recover, Core, Meter, Mic, Shared};

/// Live combo feedback while the hotkey recorder is running (DESIGN.md §3).
const HOTKEY_RECORDING: &str = "hotkey-recording";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config_dir = config::config_dir();
    let config = Config::load(&config_dir);

    // Startup failure is surfaced, never fatal. A machine with a broken audio
    // stack still gets a running app it can quit cleanly from, with the reason
    // in `AppState.last_error` — a `panic!` here would leave the user with a
    // process that vanished and no idea why.
    let (mut mic, mut last_error) = match state::spawn_mic() {
        Ok(handle) => (Mic::Live(handle), None),
        Err(e) => {
            let message = format!("microphone control unavailable: {e}");
            eprintln!("mugon: {message}");
            (Mic::Unavailable, Some(message))
        }
    };

    // Select before constructing the machine, not after. `ModeMachine::new`
    // applies the mode's resting state immediately (Push to Talk mutes), and
    // doing that first would apply it to the *system default* device before
    // switching to the configured one — muting a device the user never asked
    // about and leaving it that way.
    //
    // A failure here must not be swallowed, or that same bug comes back
    // silently: the endpoint stays on the system default, and the resting mute
    // one line below lands there instead. That fallback is the specified
    // behaviour for an absent device (DESIGN.md §7), so the resting state *is*
    // being applied to the device actually in use — but `config.device_id`
    // still names the one the user picked, so the UI would otherwise show a
    // selected device that nothing is talking to, with no explanation.
    if let Err(e) = mic.select(config.device_id.as_deref()) {
        let message = format!("could not select the saved device, using the system default: {e}");
        eprintln!("mugon: {message}");
        // Only when nothing more informative is already there — a spawn
        // failure explains this one and must not be clobbered by it.
        last_error.get_or_insert(message);
    }

    let meter_tap = mic.tap();
    let machine = ModeMachine::new(mic, config.mode);

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .manage::<Shared>(Mutex::new(Core {
            machine,
            config,
            config_dir,
            recorder: Recorder::default(),
            last_error,
        }))
        .manage::<Meter>(Mutex::new(MeterHandle::new()))
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::list_devices,
            commands::set_device,
            commands::set_mode,
            commands::set_volume,
            commands::toggle_mute,
            commands::begin_hotkey_recording,
            commands::cancel_hotkey_recording,
            commands::clear_hotkey,
            commands::set_notification_prefs,
            commands::set_autostart,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Arm the saved binding before the hook goes live, so a hotkey
            // press during startup is not briefly forwarded to whatever has
            // focus instead of being swallowed.
            //
            // `try_state` rather than `state` throughout `setup`: the latter
            // panics when nothing is managed, and no lookup in this function
            // has a recovery worth panicking over.
            let binding = app
                .try_state::<Shared>()
                .and_then(|core| lock_or_recover(&core).config.hotkey);
            hook::set_binding(binding);

            // Task 10 owns window lifecycle and will move these calls to
            // window show/hide; starting once here is correct for now. The
            // `Core` guard above is already released, so `Shared` and [`Meter`]
            // are never held at the same time here — when Task 10 does hold
            // both, the order is `Shared` first (see `state`'s module docs).
            if let (Some(tap), Some(meter)) = (meter_tap, app.try_state::<Meter>()) {
                lock_or_recover(&meter).start(handle.clone(), tap);
            }

            let (tx, rx) = mpsc::channel::<HookEvent>();

            // The hook needs its own thread with a message pump — `install`
            // blocks forever.
            std::thread::Builder::new().name("mugon-hook".into()).spawn(move || {
                if let Err(e) = hook::install(tx) {
                    // DESIGN.md §7: without the hook the app's core function
                    // is dead. Task 10/14 surface this in the UI; for now it
                    // is a loud log and a still-running app.
                    eprintln!("mugon: FATAL: keyboard hook failed to install: {e}");
                }
            })?;

            let dispatch_app = handle.clone();
            std::thread::Builder::new()
                .name("mugon-hook-dispatch".into())
                .spawn(move || dispatch_hook_events(dispatch_app, rx))?;

            tray::build(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running mugon");
}

/// What [`dispatch_hook_events`] must do *after* releasing the `Core` lock.
///
/// This type is the deadlock guard made structural. Every follow-up action —
/// emitting state, toasting, swapping the tray icon — either re-enters the
/// `Core` lock or calls into code that might; returning a description of the
/// work instead of doing it inline makes "not while holding the lock"
/// impossible to forget rather than merely commented.
#[derive(Debug)]
enum Follow {
    Nothing,
    Emit,
    Recording(Option<String>),
    MuteChanged { mode: Mode, muted: bool, prefs: NotificationPrefs },
}

/// Consumes hook events on a worker thread and routes them to the recorder or
/// the mode machine.
///
/// Runs off both the UI thread and the hook thread. The hook thread in
/// particular must never do this work: a slow Core Audio call inside
/// `hook_proc`'s window makes Windows silently uninstall the hook, and the app
/// then stops responding to the hotkey with no error anywhere.
fn dispatch_hook_events(app: AppHandle, rx: mpsc::Receiver<HookEvent>) {
    // `recv` returning `Err` means the hook thread is gone; nothing further
    // will ever arrive.
    while let Ok(ev) = rx.recv() {
        let Some(core) = app.try_state::<Shared>() else {
            return;
        };

        // Explicit block rather than passing the guard inline: the guard must
        // be visibly dead before the `match` below, which re-enters the lock.
        let follow = {
            let mut c = lock_or_recover(&core);
            handle_hook_event(&mut c, &ev)
        };

        match follow {
            Follow::Nothing => {}
            Follow::Emit => emit_state(&app),
            Follow::Recording(combo) => {
                let _ = app.emit(
                    HOTKEY_RECORDING,
                    serde_json::json!({ "active": true, "combo": combo }),
                );
            }
            Follow::MuteChanged { mode, muted, prefs } => {
                notify::on_mute_change(&app, mode, muted, &prefs);
                tray::update_icon(&app, muted);
                emit_state(&app);
            }
        }
    }
}

/// Everything [`dispatch_hook_events`] does **while holding the `Core` lock**,
/// as a plain function of the state and the event.
///
/// Split out from the loop for two reasons. It is the highest-risk logic in
/// the wiring — recorder-versus-hotkey routing, the mute-change comparison,
/// and which follow-up fires — and inline in a `while let` around an
/// `AppHandle` it would be untestable. And the split is what makes the lock
/// discipline checkable by inspection: everything in here runs under the lock,
/// everything in [`Follow`] runs after it.
///
/// Takes `&mut Core` rather than the guard so tests can call it against a
/// fake-backed `Core` (see [`state::fake_core`]).
fn handle_hook_event(c: &mut Core, ev: &HookEvent) -> Follow {
    if c.recorder.is_active() {
        return match c.recorder.feed(ev) {
            RecorderOutcome::Committed(hk) => {
                c.config.hotkey = Some(hk);
                c.persist();
                hook::set_binding(Some(hk));
                Follow::Emit
            }
            RecorderOutcome::Cancelled => {
                hook::set_binding(c.config.hotkey);
                Follow::Emit
            }
            RecorderOutcome::InProgress(partial) => Follow::Recording(partial.map(|h| h.display())),
            RecorderOutcome::Idle => Follow::Nothing,
        };
    }

    if !c.config.hotkey.is_some_and(|binding| hook::matches(&binding, ev)) {
        return Follow::Nothing;
    }

    let before = c.machine.mic().is_muted();
    c.machine.on_key(if ev.down { KeyEdge::Down } else { KeyEdge::Up });
    let after = c.machine.mic().is_muted();
    c.record_outcome(&after);

    match (before, after) {
        (Ok(before), Ok(after)) if before != after => Follow::MuteChanged {
            mode: c.machine.mode(),
            muted: after,
            prefs: c.config.notifications.clone(),
        },
        (Ok(_), Ok(_)) => Follow::Nothing,
        // Either read failed, so whether the mute actually changed is
        // unknowable. `record_outcome` has already stored the reason; push it
        // to the UI rather than swallowing it.
        _ => Follow::Emit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;
    use crate::state::fake_core;

    const F13: u16 = 0x7C;

    fn binding() -> Hotkey {
        Hotkey { ctrl: false, alt: false, shift: false, win: false, vk: F13 }
    }

    fn ev(vk: u16, down: bool) -> HookEvent {
        HookEvent { vk, down, ctrl: false, alt: false, shift: false, win: false }
    }

    fn bound_core(mode: Mode) -> (Core, tempfile::TempDir) {
        let (mut core, dir) = fake_core(mode);
        core.config.hotkey = Some(binding());
        (core, dir)
    }

    #[test]
    fn ptt_key_down_reports_a_mute_change_to_live() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        assert!(core.machine.mic().is_muted().unwrap(), "PTT rests muted");

        match handle_hook_event(&mut core, &ev(F13, true)) {
            Follow::MuteChanged { mode, muted, .. } => {
                assert_eq!(mode, Mode::PushToTalk);
                assert!(!muted, "holding the key must report going live");
            }
            other => panic!("expected a mute change, got {other:?}"),
        }
        assert!(!core.machine.mic().is_muted().unwrap());
    }

    #[test]
    fn ptt_key_up_reports_a_mute_change_back_to_muted() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));

        match handle_hook_event(&mut core, &ev(F13, false)) {
            Follow::MuteChanged { muted, .. } => assert!(muted, "release must re-mute"),
            other => panic!("expected a mute change, got {other:?}"),
        }
        assert!(core.machine.mic().is_muted().unwrap());
    }

    /// Mute Toggle acts on key-down only (§4.1), so the matching key-up changes
    /// nothing and must not fire a toast or a tray swap.
    #[test]
    fn mute_toggle_key_up_reports_nothing() {
        let (mut core, _dir) = bound_core(Mode::MuteToggle);
        assert!(matches!(
            handle_hook_event(&mut core, &ev(F13, true)),
            Follow::MuteChanged { muted: true, .. }
        ));
        assert!(matches!(handle_hook_event(&mut core, &ev(F13, false)), Follow::Nothing));
        assert!(core.machine.mic().is_muted().unwrap(), "state must survive the key-up");
    }

    #[test]
    fn a_non_matching_key_is_ignored_entirely() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let before = core.machine.mic().is_muted().unwrap();

        assert!(matches!(handle_hook_event(&mut core, &ev(0x4D, true)), Follow::Nothing));
        assert_eq!(core.machine.mic().is_muted().unwrap(), before, "the mic must not move");
    }

    /// A superset of the bound modifiers must not fire — the same rule
    /// `hook::matches` enforces, checked here at the dispatch level so a future
    /// looser comparison in this function is caught too.
    #[test]
    fn the_bound_key_with_an_extra_modifier_is_ignored() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let mut event = ev(F13, true);
        event.ctrl = true;
        assert!(matches!(handle_hook_event(&mut core, &event), Follow::Nothing));
    }

    #[test]
    fn no_binding_at_all_means_no_hotkey_ever_fires() {
        let (mut core, _dir) = fake_core(Mode::PushToTalk);
        assert_eq!(core.config.hotkey, None);
        assert!(matches!(handle_hook_event(&mut core, &ev(F13, true)), Follow::Nothing));
    }

    #[test]
    fn committing_a_recording_stores_and_persists_the_new_binding() {
        let (mut core, dir) = fake_core(Mode::MuteToggle);
        core.recorder.start();

        assert!(matches!(
            handle_hook_event(&mut core, &ev(F13, true)),
            Follow::Recording(Some(_))
        ));
        assert!(matches!(handle_hook_event(&mut core, &ev(F13, false)), Follow::Emit));

        assert_eq!(core.config.hotkey, Some(binding()));
        assert!(!core.recorder.is_active(), "committing must stop the recorder");
        assert_eq!(
            config::Config::load(dir.path()).hotkey,
            Some(binding()),
            "the new binding must survive to the next launch"
        );
    }

    /// While recording, the bound key must reach the recorder rather than
    /// toggling the mic — otherwise re-binding an existing hotkey mutes you.
    #[test]
    fn a_press_during_recording_does_not_reach_the_mode_machine() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let before = core.machine.mic().is_muted().unwrap();
        core.recorder.start();

        let _ = handle_hook_event(&mut core, &ev(F13, true));
        assert_eq!(core.machine.mic().is_muted().unwrap(), before, "the mic must not move");
    }

    #[test]
    fn escape_during_recording_cancels_and_pushes_state() {
        let (mut core, _dir) = bound_core(Mode::MuteToggle);
        core.recorder.start();

        assert!(matches!(handle_hook_event(&mut core, &ev(0x1B, true)), Follow::Emit));
        assert!(!core.recorder.is_active());
        assert_eq!(core.config.hotkey, Some(binding()), "the old binding must survive a cancel");
    }

    /// A dead or absent mic must surface rather than silently doing nothing:
    /// the read fails, so the change is unknowable and the reason goes to the
    /// UI.
    #[test]
    fn a_hotkey_press_with_no_working_mic_records_the_error_and_emits() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        core.machine = ModeMachine::new(Mic::Unavailable, Mode::PushToTalk);

        assert!(matches!(handle_hook_event(&mut core, &ev(F13, true)), Follow::Emit));
        assert_eq!(core.last_error.as_deref(), Some("no capture device available"));
    }
}

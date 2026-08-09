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
//! | `mugon-meter` | the 30Hz `level` poll loop | polls through a `MeterTap`, never touches the `Core` lock; exists only while the settings window does |
//! | `mugon-emergency-unmute` | a throwaway `Endpoint` | panic-hook only ([`emergency_unmute`]); never touches `Core`, because the panicking thread may be holding it |
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

    let machine = ModeMachine::new(mic, config.mode);

    let app = tauri::Builder::default()
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

            // §4.10: metering is a property of the *window*, not of the
            // process. Tauri has already created the configured window by the
            // time `setup` runs, so if one exists it gets its stream now;
            // `tray::show_window` opens one for every window created later.
            // When Task 12's `--minimized` suppresses the window entirely,
            // nothing is opened and Windows' microphone-in-use indicator stays
            // dark — which is the whole point.
            if app.get_webview_window("main").is_some() {
                start_metering(&handle);
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
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // §4.10: destroy the webview rather than hiding it, so the idle
                // tray footprint drops to ~10MB and — the part that is visible
                // to the user — the capture stream is released and Windows'
                // microphone-in-use indicator goes out. Quit is only reachable
                // from the tray.
                api.prevent_close();
                stop_metering(window.app_handle());
                let _ = window.destroy();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building mugon");

    app.run(|app, event| match event {
        // Destroying the last window makes Tauri ask to exit. mugon lives in
        // the tray, so closing the settings panel must not end the process —
        // `code: None` is exactly the "all windows closed" case, as opposed to
        // the `Some` that an explicit `AppHandle::exit` carries.
        tauri::RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        tauri::RunEvent::ExitRequested { .. } => restore_microphone(app),
        // The catch-all for §4.2's "any mode, any means": this fires for tray
        // Quit and for `WM_ENDSESSION` (logoff and shutdown) alike, and runs
        // before Tauri drops the managed state the audio worker lives in.
        tauri::RunEvent::Exit => restore_microphone(app),
        _ => {}
    });
}

/// Restores the microphone on the way out (§4.2).
///
/// Idempotent, and deliberately reachable from every exit path rather than
/// just the tidy one: leaving a user muted system-wide with no app running to
/// undo it is the single worst thing mugon can do.
///
/// Takes the `Core` lock and nothing else, so it is safe from the main thread
/// during shutdown. It does **not** emit state or touch the tray — there is
/// nothing left to render to.
pub fn restore_microphone(app: &AppHandle) {
    let Some(core) = app.try_state::<Shared>() else {
        return;
    };
    lock_or_recover(&core).machine.shutdown();
}

/// How long [`emergency_unmute`] gets before it gives up.
///
/// A process that will not die is worse than one that dies with the mic muted,
/// and this runs inside the panic hook — whatever went wrong may have taken the
/// audio service with it.
const EMERGENCY_UNMUTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Last-resort mic restore for the panic hook.
///
/// Deliberately talks to Core Audio through a brand-new endpoint on a
/// brand-new thread instead of using the running worker or the `Core` lock.
/// Both of those are suspect at this point: the worker may be the thing that
/// panicked, and `Core` may be poisoned *or still held* by the panicking
/// thread, in which case reusing it would turn a crash into a hang.
///
/// The fresh thread is also a hard requirement rather than caution.
/// [`audio::endpoint::Endpoint`] is `!Send` and refuses to construct outside
/// the MTA, and Tauri's main thread is an STA — so there is no thread already
/// running that this could legally borrow.
///
/// Best-effort by definition: every failure is logged and swallowed, because a
/// panic inside a panic hook aborts the process immediately.
pub fn emergency_unmute() {
    use audio::endpoint::Endpoint;
    use audio::MicBackend;

    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("mugon-emergency-unmute".into())
        .spawn(move || {
            let _ = tx.send(Endpoint::new().and_then(|mut ep| ep.set_muted(false)));
        });

    if let Err(e) = spawned {
        eprintln!("mugon: could not spawn the emergency unmute thread: {e}");
        return;
    }

    // The thread is not joined on timeout, for the same reason it is bounded:
    // it may never come back. Leaving it running costs a stranded thread in a
    // process that is already dying.
    match rx.recv_timeout(EMERGENCY_UNMUTE_TIMEOUT) {
        Ok(Ok(())) => eprintln!("mugon: microphone restored after a panic"),
        Ok(Err(e)) => eprintln!("mugon: emergency unmute failed: {e}"),
        Err(_) => eprintln!(
            "mugon: emergency unmute did not complete within {EMERGENCY_UNMUTE_TIMEOUT:?}; \
             the microphone may still be muted"
        ),
    }
}

/// Opens the metering capture stream for a settings window that has just been
/// created (§4.7).
///
/// **Lock order: `Core`, then [`Meter`] — never the reverse.** `Core` is taken
/// only long enough to clone a [`audio::thread::MeterTap`] out of it, and the
/// guard is dead before the meter lock is taken. That is not tidiness:
/// `MeterHandle::start` blocks on an audio round trip to open the stream, and
/// holding `Core` across it would park every command handler and the hook
/// dispatch thread behind a COM call. See `state`'s module docs.
pub(crate) fn start_metering(app: &AppHandle) {
    let Some(core) = app.try_state::<Shared>() else {
        return;
    };
    // A statement of its own so the `Core` guard is a temporary that dies here,
    // rather than living to the end of the `let` below.
    let tap = lock_or_recover(&core).machine.mic().tap();

    // `None` means startup found no microphone at all — there is nothing to
    // meter and nothing to report; `last_error` already says why.
    let (Some(tap), Some(meter)) = (tap, app.try_state::<Meter>()) else {
        return;
    };
    lock_or_recover(&meter).start(app.clone(), tap);
}

/// Closes the metering capture stream when the settings window goes away.
///
/// Blocks until the poll thread has actually exited, which is what makes the
/// microphone-in-use indicator go out *before* the window disappears rather
/// than some time afterwards. Takes only the [`Meter`] lock.
pub(crate) fn stop_metering(app: &AppHandle) {
    let Some(meter) = app.try_state::<Meter>() else {
        return;
    };
    lock_or_recover(&meter).stop();
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

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
use state::{emit_state, lock_or_recover, Core, Mic, Shared};

/// Live combo feedback while the hotkey recorder is running (DESIGN.md §3).
const HOTKEY_RECORDING: &str = "hotkey-recording";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = Config::load(&config::config_dir());

    // Startup failure is surfaced, never fatal. A machine with a broken audio
    // stack still gets a running app it can quit cleanly from, with the reason
    // in `AppState.last_error` — a `panic!` here would leave the user with a
    // process that vanished and no idea why.
    let (mut mic, last_error) = match state::spawn_mic() {
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
    let _ = mic.select(config.device_id.as_deref());

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
            recorder: Recorder::default(),
            last_error,
        }))
        .manage(Mutex::new(MeterHandle::new()))
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
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
            // `Core` guard above is already released, so `Core` and
            // `Mutex<MeterHandle>` are never held at the same time.
            if let (Some(tap), Some(meter)) = (meter_tap, app.try_state::<Mutex<MeterHandle>>()) {
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

        let follow = {
            let mut c = lock_or_recover(&core);
            if c.recorder.is_active() {
                match c.recorder.feed(&ev) {
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
                    RecorderOutcome::InProgress(partial) => {
                        Follow::Recording(partial.map(|h| h.display()))
                    }
                    RecorderOutcome::Idle => Follow::Nothing,
                }
            } else if c.config.hotkey.is_some_and(|binding| hook::matches(&binding, &ev)) {
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
                    // Either read failed, so whether the mute actually changed
                    // is unknowable. `record_outcome` has already stored the
                    // reason; push it to the UI rather than swallowing it.
                    _ => Follow::Emit,
                }
            } else {
                Follow::Nothing
            }
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

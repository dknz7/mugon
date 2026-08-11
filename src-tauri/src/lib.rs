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
//! | `mugon-hook-dispatch` | routing hook events into the mode machine | **must not** be the hook thread: a slow Core Audio call there makes Windows silently uninstall the hook |
//! | `mugon-meter` | the 30Hz `level` poll loop | polls through a `MeterTap`, never touches the `Core` lock; exists only while the settings window does |
//! | `mugon-emergency-unmute` | a throwaway `Endpoint` | panic-hook only ([`emergency_unmute`]); never touches `Core`, because the panicking thread may be holding it |
//! | *(Windows audio service)* | nothing | not ours: the `IMMNotificationClient` callback ([`audio::hotplug`]) runs on an arbitrary COM thread. It holds only a channel sender and an `AppHandle`, never blocks, and never touches an `Endpoint` or the `Core` lock |
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

use tauri::{AppHandle, Manager};

use audio::meter::MeterHandle;
use audio::MicBackend;
use config::{Config, NotificationPrefs};
use hotkey::hook::{self, HookEvent};
use modes::{KeyEdge, KeyObservation, Mode, ModeMachine};
use state::{emit_state, lock_or_recover, Core, Meter, Mic, Shared};

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
            Some(vec![autostart::LAUNCH_ARG]),
        ))
        .manage::<Shared>(Mutex::new(Core {
            machine,
            config,
            config_dir,
            last_error,
            // Set only if the hook thread below fails to install.
            hook_error: None,
        }))
        .manage::<Meter>(Mutex::new(MeterHandle::new()))
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::list_devices,
            commands::set_device,
            commands::set_mode,
            commands::set_volume,
            commands::toggle_mute,
            commands::list_bindable_keys,
            commands::set_hotkey,
            commands::set_notification_prefs,
            commands::set_autostart,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Task 9b (§4.5): start watching for capture devices arriving and
            // leaving. This lives here rather than in `spawn_mic` above
            // because the notification callback emits a Tauri event, and no
            // `AppHandle` exists until now.
            //
            // Non-fatal, like every other startup step: without it the app
            // simply stops noticing hotplug, which is exactly where it stood
            // before this task. Logged rather than pushed into `last_error` —
            // that field is "the last thing that went wrong with the device",
            // and a missing notification client is not something the user can
            // act on from the settings panel.
            if let Some(core) = handle.try_state::<Shared>() {
                let started =
                    lock_or_recover(&core).machine.mic().enable_hotplug(handle.clone());
                if let Err(e) = started {
                    eprintln!("mugon: device hotplug notifications unavailable: {e}");
                }
            }

            // §4.10: metering is a property of the *window*, not of the
            // process. `tauri.conf.json` declares no window
            // (`"windows": []`), so nothing exists yet — `tray::show_window`
            // is the single window-creation path (the same one every later
            // "show settings" click uses) and starts metering itself once the
            // window it just built exists. Task 12: `autostart::launched_minimized`
            // reports whether this process carries the autostart argument
            // registered above; when it does, skip this entirely — no window
            // is opened and Windows' microphone-in-use indicator stays dark,
            // which is the whole point.
            if !autostart::launched_minimized() {
                tray::show_window(&handle);
            }

            let (tx, rx) = mpsc::channel::<HookEvent>();

            // The hook needs its own thread with a message pump — `install`
            // blocks forever, so this closure only returns if it failed.
            let hook_app = handle.clone();
            std::thread::Builder::new().name("mugon-hook".into()).spawn(move || {
                if let Err(e) = hook::install(tx) {
                    // DESIGN.md §7: without the hook the app's core function
                    // is dead, so this has to reach the user. It used to be an
                    // `eprintln!` and nothing else — which in a release build
                    // goes nowhere at all, because `main.rs` sets
                    // `windows_subsystem = "windows"` and there is no console
                    // to print to. The app then showed a fully populated
                    // settings window with a bound hotkey that silently did
                    // nothing.
                    eprintln!("mugon: FATAL: keyboard hook failed to install: {e}");
                    report_hook_failure(&hook_app, &e);
                }
            })?;

            let dispatch_app = handle.clone();
            std::thread::Builder::new()
                .name("mugon-hook-dispatch".into())
                .spawn(move || dispatch_hook_events(dispatch_app, rx))?;

            // §4.3. The hook can miss a key-up; nothing else in the app would
            // ever notice, and in Push to Talk that leaves the microphone live.
            let watchdog_app = handle.clone();
            std::thread::Builder::new()
                .name("mugon-watchdog".into())
                .spawn(move || run_stuck_key_watchdog(watchdog_app))?;

            tray::build(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // §4.10: destroy the webview rather than hiding it. That
                // retires all six `msedgewebview2.exe` processes — measured at
                // Task 15, the tree drops from ~365MB to ~28MB working set
                // (~7MB private) — and, the part that is visible to the user,
                // releases the capture stream so Windows' microphone-in-use
                // indicator goes out. Quit is only reachable from the tray.
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

/// The banner text for a keyboard hook that would not install.
///
/// Split out from [`report_hook_failure`] so the wording — the part the user
/// actually has to act on — is testable without an `AppHandle`.
///
/// Written front-loaded on purpose. The banner truncates to roughly 65
/// characters (the frontend puts the full string in a tooltip), so the two
/// things the user needs — *the hotkey does not work* and *antivirus is the
/// likely reason* — have to survive being cut off. The underlying Win32 error
/// trails at the end where losing it costs nothing.
fn hook_failure_message(reason: &str) -> String {
    format!(
        "Hotkey inactive — Windows blocked the keyboard hook. Antivirus software is the \
         usual cause; mugon cannot mute or unmute from the keyboard until it is allowed \
         through. Everything else still works. ({reason})"
    )
}

/// Records a hook-install failure where the user can see it (§7).
///
/// Takes the `Core` lock only to store the message, then emits **after**
/// releasing it — `emit_state` takes the same lock.
fn report_hook_failure(app: &AppHandle, reason: &str) {
    let Some(core) = app.try_state::<Shared>() else {
        return;
    };
    // Scoped so the guard is dead before `emit_state` re-enters the lock.
    {
        lock_or_recover(&core).hook_error = Some(hook_failure_message(reason));
    }
    emit_state(app);
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
            let result = Endpoint::new().and_then(|mut ep| {
                // Re-point at the *saved* device before unmuting. A fresh
                // `Endpoint` always opens the system default
                // (`GetDefaultAudioEndpoint`), and on any machine with virtual
                // audio devices that is routinely not the physical microphone
                // the user configured — so unmuting it would restore the wrong
                // endpoint and leave the muted one exactly as dead as before.
                //
                // Reading the config here does not break this function's one
                // rule. `Config::load` is a filesystem read that cannot fail
                // (it falls back to defaults), touches no lock and no `Core`,
                // and rides inside the same [`EMERGENCY_UNMUTE_TIMEOUT`] as
                // everything else here.
                //
                // A failed `select` is logged and ignored on purpose: unmuting
                // whichever endpoint we do have beats giving up entirely.
                if let Some(id) = Config::load(&config::config_dir()).device_id.as_deref() {
                    if let Err(e) = ep.select(Some(id)) {
                        eprintln!("mugon: emergency unmute could not select {id}: {e}");
                    }
                }
                ep.set_muted(false)
            });
            let _ = tx.send(result);
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
    MuteChanged { mode: Mode, muted: bool, prefs: NotificationPrefs },
}

/// Consumes hook events on a worker thread and routes them into the mode
/// machine.
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
        // be visibly dead before `run_follow` below, which re-enters the lock.
        let follow = {
            let mut c = lock_or_recover(&core);
            handle_hook_event(&mut c, &ev)
        };

        run_follow(&app, follow);
    }
}

/// Performs a [`Follow`]. **Must be called with no lock held** — every arm
/// either re-enters the `Core` lock or marshals onto the UI thread.
fn run_follow(app: &AppHandle, follow: Follow) {
    match follow {
        Follow::Nothing => {}
        Follow::Emit => emit_state(app),
        Follow::MuteChanged { mode, muted, prefs } => {
            notify::on_mute_change(app, mode, muted, &prefs);
            tray::update_icon(app, muted);
            emit_state(app);
        }
    }
}

/// §4.3's poll interval. Fast enough that a latched-open microphone is measured
/// in fractions of a second, slow enough to be free: a poll that finds nothing
/// wrong is a `GetAsyncKeyState` and two field reads under the `Core` lock, and
/// never talks to the audio worker.
const WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// The stuck-key watchdog (§4.3).
///
/// Runs for the life of the process on its own thread. It is the only thing
/// standing between a missed key-up and a microphone that stays live
/// indefinitely — a UAC prompt, `Win+L`, or an elevated window taking the
/// foreground all stop the hook receiving events, and in Push to Talk the hold
/// then never ends.
///
/// Deliberately **not** on the hook thread (a Core Audio call there makes
/// Windows silently uninstall the hook) and not on the UI thread (it would
/// block the event loop on an audio round trip). Same lock discipline as the
/// hook dispatch loop: decide under the lock, act after it.
fn run_stuck_key_watchdog(app: AppHandle) {
    loop {
        std::thread::sleep(WATCHDOG_INTERVAL);

        // Gone means the app is tearing down; there is nothing left to guard.
        let Some(core) = app.try_state::<Shared>() else {
            return;
        };

        let (forced, follow) = {
            let mut c = lock_or_recover(&core);
            watchdog_poll(&mut c, hook::is_physically_down)
        };

        // Logged out here rather than inside the poll: everything this module
        // does under the `Core` lock is deliberate, and a line of I/O that only
        // exists for diagnostics has no business being the one exception.
        if forced {
            eprintln!("mugon: stuck-key watchdog forced the push-to-talk release path (§4.3)");
        }
        run_follow(&app, follow);
    }
}

/// One watchdog poll, **under the `Core` lock**, as a function of the state and
/// a key-state probe.
///
/// Returns whether it forced a release, and the follow-up work. The flag is
/// separate from the [`Follow`] because it cannot be recovered from it: a
/// forced release normally reports `MuteChanged`, but degrades to `Emit` when
/// the endpoint stops answering and to `Nothing` if the mic was somehow already
/// muted. The caller logs off the flag, outside the lock.
///
/// The probe is a parameter rather than a direct `GetAsyncKeyState` call so the
/// whole poll — decision *and* its effect on the microphone — is testable
/// against a fake-backed `Core`, which is the point: a watchdog that only fires
/// in situations nobody can reproduce is a watchdog nobody can trust.
fn watchdog_poll(c: &mut Core, physically_down: impl Fn(u16) -> bool) -> (bool, Follow) {
    let observation = KeyObservation {
        mode: c.machine.mode(),
        held: c.machine.is_held(),
        physically_down: c.config.hotkey.map(|binding| physically_down(binding.vk)),
    };
    if !observation.release_required() {
        return (false, Follow::Nothing);
    }

    (true, apply_edge(c, KeyEdge::Up))
}

/// Everything [`dispatch_hook_events`] does **while holding the `Core` lock**,
/// as a plain function of the state and the event.
///
/// Split out from the loop for two reasons. It is the highest-risk logic in
/// the wiring — binding match, confirmation, the mute-change comparison,
/// and which follow-up fires — and inline in a `while let` around an
/// `AppHandle` it would be untestable. And the split is what makes the lock
/// discipline checkable by inspection: everything in here runs under the lock,
/// everything in [`Follow`] runs after it.
///
/// Takes `&mut Core` rather than the guard so tests can call it against a
/// fake-backed `Core` (see [`state::fake_core`]).
fn handle_hook_event(c: &mut Core, ev: &HookEvent) -> Follow {
    if !c.config.hotkey.is_some_and(|binding| hook::matches(&binding, ev)) {
        return Follow::Nothing;
    }

    // Task 17: the hook has now seen the bound combo actually fire, which is
    // what moves `HOTKEY STATUS` off `Bound`. Key-down only — `hook::matches`
    // compares the modifier set exactly, and users routinely release Ctrl before
    // the key, so the key-up event often carries no modifiers and matches
    // nothing.
    //
    // Nothing is done with the return here because every reachable `apply_edge`
    // result below already emits: a working endpoint reports `MuteChanged`, and
    // one that has stopped answering reports `Emit`. If that ever stops being
    // true, a first sighting would silently leave the status line reading
    // "press … to confirm" until something unrelated pushed state.
    if ev.down {
        c.confirm_hotkey();
    }

    apply_edge(c, if ev.down { KeyEdge::Down } else { KeyEdge::Up })
}

/// Drives one key edge through the mode machine and reports what changed.
///
/// Shared by the hook dispatch path and the stuck-key watchdog, which must
/// produce identical follow-ups: a forced release has to swap the tray icon,
/// beep and push state exactly as a real key-up would, or the user is left
/// looking at a "live" tray icon over a muted microphone.
fn apply_edge(c: &mut Core, edge: KeyEdge) -> Follow {
    let before = c.machine.mic().is_muted();
    c.machine.on_key(edge);
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

    /// Task 17. Observing the bound combo fire is the only proof mugon has that
    /// the key actually reaches it — the one thing recording provided that a
    /// dropdown cannot. It drives `HOTKEY STATUS` from `Bound` to `Confirmed`.
    #[test]
    fn a_matching_press_confirms_the_binding() {
        let (mut core, _dir) = bound_core(Mode::MuteToggle);
        assert!(!core.config.hotkey_confirmed, "a fresh binding starts unconfirmed");

        let _ = handle_hook_event(&mut core, &ev(F13, true));

        assert!(core.config.hotkey_confirmed);
    }

    #[test]
    fn a_non_matching_press_never_confirms() {
        let (mut core, _dir) = bound_core(Mode::MuteToggle);

        let _ = handle_hook_event(&mut core, &ev(0x4D, true));

        assert!(!core.config.hotkey_confirmed, "only the bound combo is proof");
    }

    /// Confirmation hangs off **key-down** deliberately. `hook::matches`
    /// compares the modifier set exactly, and on the release of `Ctrl + F16` the
    /// user has very often let Ctrl go first — so the key-up event carries
    /// `ctrl: false` and matches nothing. Key-down is the edge that reliably
    /// carries the whole combo.
    #[test]
    fn a_release_whose_modifiers_already_lifted_does_not_confirm() {
        let (mut core, _dir) = fake_core(Mode::MuteToggle);
        core.config.hotkey =
            Some(Hotkey { ctrl: true, alt: false, shift: false, win: false, vk: F13 });

        let _ = handle_hook_event(&mut core, &ev(F13, false));

        assert!(!core.config.hotkey_confirmed);
    }

    // --- Hook health (§7) ---

    /// The banner is the only signal a user gets that their hotkey is dead, so
    /// it has to say both what broke and what to do — and say them early
    /// enough to survive the frontend's ~65-character truncation.
    #[test]
    fn the_hook_failure_banner_names_the_symptom_and_the_likely_cause() {
        let message = hook_failure_message("SetWindowsHookExW failed: Access is denied. (0x80070005)");

        let head = &message[..65.min(message.len())];
        assert!(head.contains("Hotkey"), "the symptom must survive truncation: {head}");
        assert!(
            head.to_lowercase().contains("antivirus") || head.contains("blocked"),
            "the cause must survive truncation: {head}"
        );
        assert!(
            message.contains("0x80070005"),
            "the underlying Win32 error must still be recoverable from the full string"
        );
    }

    /// The whole reason this is not `last_error`: that field clears on the next
    /// successful audio call, which in practice is milliseconds away. A dead
    /// hook is a standing condition and must outlive every one of them.
    #[test]
    fn a_hook_failure_survives_later_successful_audio_calls() {
        let (mut core, _dir) = bound_core(Mode::MuteToggle);
        core.hook_error = Some(hook_failure_message("nope"));
        core.last_error = Some("something transient".into());

        // Any successful fallible operation clears `last_error` by design, and
        // this is the cheapest one. Called purely for that side effect: the
        // mute state it returns is irrelevant here, and asserting it would tie
        // a hook-error test to the resting state of an unrelated mode. The
        // returned value is therefore ignored deliberately, and the assertion
        // below — which fails if the clear did not happen — is the real check.
        let _ = core.refresh_mic_health();
        assert_eq!(core.last_error, None, "the transient error must have cleared");

        let snapshot = core.snapshot();
        assert!(
            snapshot.hook_error.is_some(),
            "the hook failure must still be reported to the UI"
        );
        assert_eq!(snapshot.last_error, None);
    }

    /// A healthy hook must not put anything in the banner.
    #[test]
    fn a_working_hook_reports_no_error() {
        let (core, _dir) = bound_core(Mode::MuteToggle);
        assert_eq!(core.snapshot().hook_error, None);
    }

    // --- Stuck-key watchdog (§4.3) ---

    /// Probes that stand in for `GetAsyncKeyState`.
    const KEY_IS_UP: fn(u16) -> bool = |_| false;
    const KEY_IS_DOWN: fn(u16) -> bool = |_| true;

    /// One poll's follow-up, dropping the "did it force a release" flag that
    /// only the caller's logging needs. `forced_a_release_is_reported`
    /// covers the flag itself.
    fn poll(core: &mut Core, probe: impl Fn(u16) -> bool) -> Follow {
        watchdog_poll(core, probe).1
    }

    /// The flag exists so the watchdog thread can log outside the `Core` lock,
    /// and it must not be inferrable from the [`Follow`] — hence its own test.
    #[test]
    fn a_forced_release_is_reported_separately_from_its_follow_up() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        assert!(!watchdog_poll(&mut core, KEY_IS_UP).0, "nothing held: nothing forced");

        let _ = handle_hook_event(&mut core, &ev(F13, true));
        assert!(!watchdog_poll(&mut core, KEY_IS_DOWN).0, "a real hold forces nothing");
        assert!(watchdog_poll(&mut core, KEY_IS_UP).0, "a stuck key must report the release");
        assert!(!watchdog_poll(&mut core, KEY_IS_UP).0, "and only once");
    }

    /// The failure this whole feature exists for: PTT held, the key-up lost to
    /// a UAC prompt, and the microphone live with nothing to close it.
    #[test]
    fn the_watchdog_remutes_a_ptt_hold_whose_key_up_never_arrived() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));
        assert!(!core.machine.mic().is_muted().unwrap(), "sanity: the hold went live");

        match poll(&mut core, KEY_IS_UP) {
            Follow::MuteChanged { mode, muted, .. } => {
                assert_eq!(mode, Mode::PushToTalk);
                assert!(muted, "the forced release must report the mic going quiet");
            }
            other => panic!("expected a mute change, got {other:?}"),
        }
        assert!(core.machine.mic().is_muted().unwrap());
        assert!(!core.machine.is_held(), "the hold must be cleared, not just the mute");
    }

    /// The case that runs four times a second for as long as anyone speaks.
    /// Getting this wrong cuts the user off mid-sentence.
    #[test]
    fn the_watchdog_leaves_a_genuine_hold_alone() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));

        for _ in 0..8 {
            assert!(matches!(poll(&mut core, KEY_IS_DOWN), Follow::Nothing));
        }
        assert!(!core.machine.mic().is_muted().unwrap(), "a real hold must stay live");
        assert!(core.machine.is_held());
    }

    #[test]
    fn the_watchdog_is_silent_when_nothing_is_held() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        assert!(matches!(poll(&mut core, KEY_IS_UP), Follow::Nothing));
        assert!(core.machine.mic().is_muted().unwrap(), "PTT rests muted, untouched");
    }

    /// Mute Toggle's `held` flag only suppresses auto-repeat. Forcing an edge
    /// through it would be a mute change the user did not ask for.
    #[test]
    fn the_watchdog_never_touches_mute_toggle() {
        let (mut core, _dir) = bound_core(Mode::MuteToggle);
        let _ = handle_hook_event(&mut core, &ev(F13, true));
        let muted = core.machine.mic().is_muted().unwrap();

        assert!(matches!(poll(&mut core, KEY_IS_UP), Follow::Nothing));
        assert_eq!(core.machine.mic().is_muted().unwrap(), muted, "the mic must not move");
    }

    /// Clearing the hotkey mid-hold removes the only thing that could have
    /// matched the key-up, so nothing would ever end the hold.
    #[test]
    fn the_watchdog_releases_a_hold_whose_binding_was_cleared() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));
        core.config.hotkey = None;

        assert!(matches!(poll(&mut core, KEY_IS_DOWN), Follow::MuteChanged { muted: true, .. }));
        assert!(core.machine.mic().is_muted().unwrap());
    }

    /// A watchdog that re-fires forever would beep and re-emit four times a
    /// second for the rest of the session.
    #[test]
    fn the_watchdog_fires_once_and_then_settles() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));

        assert!(matches!(poll(&mut core, KEY_IS_UP), Follow::MuteChanged { .. }));
        for _ in 0..4 {
            assert!(matches!(poll(&mut core, KEY_IS_UP), Follow::Nothing));
        }
    }

    /// The hardware key-up routinely arrives after the watchdog has already
    /// acted — the UAC prompt is dismissed, the screen is unlocked. It must not
    /// produce a second mute change.
    #[test]
    fn a_late_real_key_up_after_a_forced_release_is_a_noop() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));
        let _ = poll(&mut core, KEY_IS_UP);

        assert!(matches!(handle_hook_event(&mut core, &ev(F13, false)), Follow::Nothing));
        assert!(core.machine.mic().is_muted().unwrap());
    }

    /// The binding is what the watchdog polls, so it must poll the *bound* key
    /// rather than whatever it happens to have to hand.
    #[test]
    fn the_watchdog_polls_the_bound_virtual_key() {
        let (mut core, _dir) = bound_core(Mode::PushToTalk);
        let _ = handle_hook_event(&mut core, &ev(F13, true));

        let seen = std::cell::Cell::new(None);
        let _ = poll(&mut core, |vk| {
            seen.set(Some(vk));
            true
        });
        assert_eq!(seen.get(), Some(F13));
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

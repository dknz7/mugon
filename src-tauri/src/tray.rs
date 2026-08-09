//! Tray icon and context menu (DESIGN.md §4.9).
//!
//! The tray is the app's only permanent surface: the settings window is
//! destroyed on close (§4.10), so for most of mugon's life this menu is the
//! entire UI — and the only way to quit cleanly with the microphone restored.
//!
//! Every handler in here runs on the **main thread** (menu and tray events are
//! delivered from the event loop), which is what makes it safe to build windows
//! and mutate menu items directly. It also means every handler is holding up
//! the UI while it runs, so each takes the `Core` lock in an inner scope and
//! does its follow-up work — icon swap, menu sync, `emit_state` — after
//! releasing it. See `state`'s module docs for why that ordering is not
//! optional.

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::audio::MicBackend;
use crate::modes::Mode;
use crate::state::{emit_state, lock_or_recover, Core, Shared};

const TRAY_ID: &str = "mugon-tray";

const LIVE_ICON: &[u8] = include_bytes!("../icons/tray-live.png");
const MUTED_ICON: &[u8] = include_bytes!("../icons/tray-muted.png");

/// The menu items whose *appearance* has to track app state: the mode radio
/// pair and the manual toggle, which is disabled in Push to Talk (§4.1).
///
/// Managed rather than re-fetched because [`tauri::tray::TrayIcon`] exposes no
/// getter for the menu it was built with. Holding the items directly also keeps
/// [`sync_menu`] free of id lookups that could silently start matching nothing.
struct TrayMenu {
    toggle: MenuItem<Wry>,
    mode_toggle: CheckMenuItem<Wry>,
    mode_ptt: CheckMenuItem<Wry>,
}

/// Builds the tray icon and its context menu, and wires window show/create.
///
/// Called from `setup`. Must succeed even when the microphone failed to
/// initialise — the tray is the only way to quit the app cleanly, so it has to
/// exist before anything else is allowed to go wrong. That is why the initial
/// state below degrades rather than propagating: `Mode::default()` and "not
/// muted" are wrong-but-harmless for one paint, and [`sync_menu`] corrects them
/// the moment anything changes.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let (mode, muted) = app
        .try_state::<Shared>()
        .map(|core| {
            let c = lock_or_recover(&core);
            (c.machine.mode(), c.machine.mic().is_muted().unwrap_or(false))
        })
        .unwrap_or((Mode::default(), false));

    let toggle = MenuItem::with_id(app, "toggle", "Toggle Mute", true, None::<&str>)?;
    let mode_toggle =
        CheckMenuItem::with_id(app, "mode_toggle", "Mute Toggle", true, false, None::<&str>)?;
    let mode_ptt =
        CheckMenuItem::with_id(app, "mode_ptt", "Push to Talk", true, false, None::<&str>)?;
    let modes = Submenu::with_id_and_items(app, "modes", "Mode", true, &[&mode_toggle, &mode_ptt])?;
    let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &modes,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    app.manage(TrayMenu { toggle, mode_toggle, mode_ptt });

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(muted)?)
        .tooltip("mugon")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            // `Up` only: a click delivers both a Down and an Up, and reacting
            // to both would build the window twice.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => {
                // No-ops in Push to Talk, where the item is disabled anyway
                // (§4.1) — `ModeMachine::toggle_manual` enforces it regardless.
                if let Some(muted) = with_core(app, |c| c.toggle_mute()) {
                    update_icon(app, muted);
                }
                emit_state(app);
            }
            "mode_toggle" => set_mode(app, Mode::MuteToggle),
            "mode_ptt" => set_mode(app, Mode::PushToTalk),
            "settings" => show_window(app),
            "quit" => {
                // §4.2: the mic MUST be restored before we go, and it has to
                // happen here rather than on the way out, while the audio
                // worker is unquestionably still alive. `RunEvent::Exit` also
                // calls this — it is idempotent — because tray Quit is not the
                // only way this process ends.
                crate::restore_microphone(app);
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    sync_menu(app, mode);
    Ok(())
}

/// Runs `change` under the `Core` lock and hands back its result, or `None` if
/// there is no managed state to change.
///
/// Exists so the guard is scoped to this function and callers cannot
/// accidentally hold it across [`emit_state`] or [`update_icon`], both of which
/// re-enter the lock or marshal onto the main thread.
fn with_core<T>(app: &AppHandle, change: impl FnOnce(&mut Core) -> T) -> Option<T> {
    let core = app.try_state::<Shared>()?;
    let mut c = lock_or_recover(&core);
    Some(change(&mut c))
}

fn set_mode(app: &AppHandle, mode: Mode) {
    // Shared with `commands::set_mode` — see [`Core::apply_mode`]. The two
    // differ only in the follow-up: the tray additionally has check marks to
    // move, which is exactly the kind of difference that used to justify a
    // second copy of the mode-change logic and would have let it drift.
    let muted = with_core(app, |c| c.apply_mode(mode));
    sync_menu(app, mode);
    if let Some(muted) = muted {
        update_icon(app, muted);
    }
    emit_state(app);
}

/// Points the mode check marks at `mode` and enables the manual toggle only
/// where it does anything (§4.1).
///
/// `pub(crate)` because the mode can also change from the IPC surface, and a
/// tray menu still showing the old mode is a lie the user acts on. Nothing
/// calls it from there yet — the frontend that would is Tasks 13–14.
pub(crate) fn sync_menu(app: &AppHandle, mode: Mode) {
    let Some(menu) = app.try_state::<TrayMenu>() else {
        return;
    };
    let _ = menu.mode_toggle.set_checked(mode == Mode::MuteToggle);
    let _ = menu.mode_ptt.set_checked(mode == Mode::PushToTalk);
    let _ = menu.toggle.set_enabled(mode == Mode::MuteToggle);
}

fn icon(muted: bool) -> tauri::Result<tauri::image::Image<'static>> {
    tauri::image::Image::from_bytes(if muted { MUTED_ICON } else { LIVE_ICON })
}

/// Swaps the tray icon between the live and struck-through microphone.
///
/// Callable from any thread — Tauri marshals `set_icon` onto the main thread —
/// but **not** while holding the `Core` lock: off the main thread this blocks
/// until the event loop services it, and the event loop may itself be inside a
/// command that wants that lock.
pub fn update_icon(app: &AppHandle, muted: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match icon(muted) {
        Ok(image) => {
            let _ = tray.set_icon(Some(image));
        }
        Err(e) => eprintln!("mugon: could not decode the tray icon: {e}"),
    }
}

/// Shows the settings window, recreating it if a previous close destroyed it
/// (§4.10).
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    let built = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
        .title("mugon")
        .inner_size(460.0, 700.0)
        .resizable(false)
        .maximizable(false)
        // Explicit rather than relying on Tauri's default (which is `true`
        // and does match this) — Task 12's fix round 1 asked for this to
        // stop being a question anyone has to go verify against the source.
        .decorations(true)
        .build();

    match built {
        Ok(window) => {
            let _ = window.set_focus();
            // §4.7/§4.10: the capture stream exists for the meter, and the
            // meter exists for this window. Opening it here — rather than at
            // startup — is what keeps Windows' microphone-in-use indicator dark
            // while mugon is only sitting in the tray.
            crate::start_metering(app);
            emit_state(app);
        }
        Err(e) => eprintln!("mugon: could not create the settings window: {e}"),
    }
}

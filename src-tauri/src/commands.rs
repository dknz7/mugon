//! The IPC surface (DESIGN.md §3).
//!
//! Every command follows the same shape: take the `Core` lock in an inner
//! block, mutate, drop the guard, *then* [`emit_state`]. `emit_state` takes the
//! lock itself to build its snapshot, so calling it inside the block would
//! deadlock instantly — the block is not stylistic.

use tauri::{AppHandle, State};

use crate::audio::{DeviceInfo, MicBackend};
use crate::modes::Mode;
use crate::state::{emit_state, lock_or_recover, AppState, Shared};

#[tauri::command]
pub fn get_state(core: State<Shared>) -> AppState {
    lock_or_recover(&core).snapshot()
}

/// Enumerates capture endpoints (DESIGN.md §3).
///
/// Separate from `get_state` because it is expensive — a full COM enumeration
/// plus a friendly-name property read per device — and because the answer
/// changes on hotplug, not on every mute. Call it on window open and on
/// `devices-changed`, not on every render. See [`AppState`]'s docs.
///
/// Returns an empty list rather than an error on failure; the reason lands in
/// `last_error` and rides out on the `state-changed` this emits.
#[tauri::command]
pub fn list_devices(app: AppHandle, core: State<Shared>) -> Vec<DeviceInfo> {
    let devices = {
        let mut c = lock_or_recover(&core);
        let result = c.machine.mic().list_devices();
        c.record_outcome(&result);
        result.unwrap_or_default()
    };
    emit_state(&app);
    devices
}

#[tauri::command]
pub fn set_device(app: AppHandle, core: State<Shared>, id: Option<String>) {
    // The select, the resting-state reassertion and the persist are one unit
    // in `Core::select_device` so they can be tested together — getting the
    // reassertion wrong leaves a push-to-talk user hot. See its docs.
    lock_or_recover(&core).select_device(id);
    emit_state(&app);
}

#[tauri::command]
pub fn set_mode(app: AppHandle, core: State<Shared>, mode: Mode) {
    // Shared with the tray's Mode submenu — see `Core::apply_mode`.
    //
    // The guard is a temporary of this statement, so it is dead before the
    // three calls below, every one of which either re-enters the lock or
    // marshals onto the main thread.
    let muted = lock_or_recover(&core).apply_mode(mode);
    // The tray is the app's permanent surface and has no other way to learn
    // that the settings window changed the mode: without these, a mode change
    // from the UI leaves the tray showing the old check mark, the old icon, and
    // a Toggle Mute item enabled in a mode where it does nothing.
    crate::tray::sync_menu(&app, mode);
    crate::tray::update_icon(&app, muted);
    emit_state(&app);
}

#[tauri::command]
pub fn set_volume(app: AppHandle, core: State<Shared>, level: f32) {
    {
        let mut c = lock_or_recover(&core);
        let result = c.machine.mic_mut().set_volume(level);
        c.record_outcome(&result);
        // Deliberately NOT persisted — Windows owns this value (DESIGN.md §4.6).
    }
    emit_state(&app);
}

#[tauri::command]
pub fn toggle_mute(app: AppHandle, core: State<Shared>) {
    // Shared with the tray's Toggle Mute item — see `Core::toggle_mute`.
    let muted = lock_or_recover(&core).toggle_mute();
    crate::tray::update_icon(&app, muted);
    emit_state(&app);
}

#[tauri::command]
pub fn begin_hotkey_recording(app: AppHandle, core: State<Shared>) {
    {
        let mut c = lock_or_recover(&core);
        c.recorder.start();
        // Stop swallowing while recording, so the user can bind a key that is
        // currently bound without the old binding eating the press.
        crate::hotkey::hook::set_binding(None);
    }
    emit_state(&app);
}

#[tauri::command]
pub fn cancel_hotkey_recording(app: AppHandle, core: State<Shared>) {
    {
        let mut c = lock_or_recover(&core);
        c.recorder.cancel();
        crate::hotkey::hook::set_binding(c.config.hotkey);
    }
    emit_state(&app);
}

#[tauri::command]
pub fn clear_hotkey(app: AppHandle, core: State<Shared>) {
    {
        let mut c = lock_or_recover(&core);
        c.config.hotkey = None;
        c.persist();
        crate::hotkey::hook::set_binding(None);
    }
    emit_state(&app);
}

#[tauri::command]
pub fn set_notification_prefs(app: AppHandle, core: State<Shared>, toast: bool, sound: bool) {
    {
        let mut c = lock_or_recover(&core);
        c.config.notifications.toast = toast;
        c.config.notifications.sound = sound;
        c.persist();
    }
    emit_state(&app);
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, core: State<Shared>, enabled: bool) -> Result<(), String> {
    // Deliberately outside the lock: writing the Run key is registry I/O with
    // no need for `Core`, and the config must only record what actually took
    // effect.
    let result = crate::autostart::set(&app, enabled);
    {
        let mut c = lock_or_recover(&core);
        // Routed through `last_error` as well as the `Result`, so Task 14 has
        // exactly one place to look for "the last thing that failed" rather
        // than one field plus a per-command rejection to remember.
        c.last_error = result.clone().err();
        if result.is_ok() {
            c.config.autostart = enabled;
            c.persist();
        }
    }
    emit_state(&app);
    result
}

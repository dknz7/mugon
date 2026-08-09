//! The IPC surface (DESIGN.md §3).
//!
//! Every command follows the same shape: take the `Core` lock in an inner
//! block, mutate, drop the guard, *then* [`emit_state`]. `emit_state` takes the
//! lock itself to build its snapshot, so calling it inside the block would
//! deadlock instantly — the block is not stylistic.

use tauri::{AppHandle, State};

use crate::audio::MicBackend;
use crate::modes::Mode;
use crate::state::{emit_state, lock_or_recover, AppState, Shared};

#[tauri::command]
pub fn get_state(core: State<Shared>) -> AppState {
    lock_or_recover(&core).snapshot()
}

#[tauri::command]
pub fn set_device(app: AppHandle, core: State<Shared>, id: Option<String>) {
    {
        let mut c = lock_or_recover(&core);
        let result = c.machine.mic_mut().select(id.as_deref());
        c.record_outcome(&result);
        // Persisted even when the selection failed: the user asked for this
        // device, and a device that is merely unplugged right now should come
        // back on the next launch rather than being silently forgotten.
        c.config.device_id = id;
        c.persist();
    }
    emit_state(&app);
}

#[tauri::command]
pub fn set_mode(app: AppHandle, core: State<Shared>, mode: Mode) {
    {
        let mut c = lock_or_recover(&core);
        c.machine.set_mode(mode);
        c.refresh_mic_health();
        c.config.mode = mode;
        c.persist();
    }
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
    {
        let mut c = lock_or_recover(&core);
        c.machine.toggle_manual();
        c.refresh_mic_health();
    }
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
    crate::autostart::set(&app, enabled)?;
    {
        let mut c = lock_or_recover(&core);
        c.config.autostart = enabled;
        c.persist();
    }
    emit_state(&app);
    Ok(())
}

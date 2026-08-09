//! Run-at-login registration (DESIGN.md §4.11).
//!
//! Backed by `tauri-plugin-autostart`
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, no elevation). The
//! plugin is initialised once in `lib.rs::run` with `--minimized` as the
//! launch argument it writes into the registry value, so an autostart launch
//! reaches `main` with that flag present and never creates a window (see
//! `lib.rs`'s `setup`).

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// Enables or disables launching mugon at login.
///
/// Returns `Err` with a displayable message when the registry write fails, so
/// the command can surface it instead of persisting a preference that did not
/// take effect.
pub fn set(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())
    } else {
        manager.disable().map_err(|e| e.to_string())
    }
}

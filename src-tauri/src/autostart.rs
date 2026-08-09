//! Run-at-login registration (DESIGN.md §4.11).
//!
//! Backed by `tauri-plugin-autostart`
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, no elevation). The
//! plugin is initialised once in `lib.rs::run` with [`LAUNCH_ARG`] as the
//! launch argument it writes into the registry value, so an autostart launch
//! reaches `main` with that flag present in `std::env::args()` and — via
//! [`launched_minimized`] — never creates a window (see `lib.rs`'s `setup`).
//! This module owns that flag string; nothing else should hardcode it.

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

/// The argument an autostart launch carries, and the one thing in the process
/// that suppresses window creation for it. Defined once here so `lib.rs`'s
/// plugin registration (which writes it into the registry) and
/// [`launched_minimized`] (which reads it back from `argv`) can never drift
/// apart into two different strings.
pub const LAUNCH_ARG: &str = "--minimized";

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

/// Whether this process was launched with [`LAUNCH_ARG`] — i.e. by Windows at
/// login rather than by a user double-click or `Start Menu` entry.
///
/// `setup()` uses this to decide whether to create the settings window at
/// all (DESIGN.md §4.10/§4.11): a launch that skips this check would open a
/// window — and start metering — on every autostart boot, defeating the
/// point of the flag.
pub fn launched_minimized() -> bool {
    std::env::args().any(|arg| arg == LAUNCH_ARG)
}

//! Run-at-login registration (DESIGN.md §4.11).
//!
//! **Stub — Task 12 implements this** over `tauri-plugin-autostart`
//! (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, no elevation). The
//! signature below is the contract `commands::set_autostart` already calls
//! against; the body is deliberately a no-op success.

use tauri::AppHandle;

/// Enables or disables launching mugon at login.
///
/// Returns `Err` with a displayable message when the registry write fails, so
/// the command can surface it instead of persisting a preference that did not
/// take effect.
pub fn set(_app: &AppHandle, _enabled: bool) -> Result<(), String> {
    Ok(())
}

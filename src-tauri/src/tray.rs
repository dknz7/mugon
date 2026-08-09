//! Tray icon and context menu (DESIGN.md §4.9).
//!
//! **Stub — Task 10 implements this.** The signatures below are the contract
//! `lib.rs` already calls against; the bodies are deliberately empty so the
//! wiring task ships something reviewable rather than half a tray.

use tauri::AppHandle;

/// Builds the tray icon and its context menu, and wires window show/create.
///
/// Called from `setup`. Must succeed even when the microphone failed to
/// initialise — the tray is the only way to quit the app cleanly, so it has to
/// exist before anything else is allowed to go wrong.
pub fn build(_app: &AppHandle) -> tauri::Result<()> {
    Ok(())
}

/// Swaps the tray icon between the live and struck-through microphone.
pub fn update_icon(_app: &AppHandle, _muted: bool) {}

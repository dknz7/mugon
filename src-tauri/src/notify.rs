//! Toasts and the optional beep (DESIGN.md §4.8).
//!
//! **Stub — Task 11 implements this.** The signature below is the contract
//! `lib.rs` already calls against; the body is deliberately empty.

use tauri::AppHandle;

use crate::config::NotificationPrefs;
use crate::modes::Mode;

/// Fires the configured notifications for a mute state change.
///
/// Takes `mode` because toasts fire in Mute Toggle mode only — push-to-talk
/// would toast on every utterance (DESIGN.md §4.8). The beep, when enabled,
/// fires in both modes.
///
/// Called from the hook dispatch thread with **no locks held**.
pub fn on_mute_change(
    _app: &AppHandle,
    _mode: Mode,
    _muted: bool,
    _prefs: &NotificationPrefs,
) {
}

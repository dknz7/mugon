pub mod endpoint;
#[cfg(test)]
pub mod fake;
pub mod hotplug;
pub mod meter;
pub mod thread;

use serde::Serialize;

/// dBFS floor. Anything at or below this reads as silence (§4.7).
pub const DBFS_FLOOR: f32 = -60.0;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no capture device available")]
    NoDevice,
    #[error("device {0} not found")]
    DeviceNotFound(String),
    /// The audio worker thread ([`thread::MicHandle`]'s) is gone: it panicked,
    /// exited, or never started. Distinct from [`AudioError::Windows`] because
    /// the recovery policy differs — a failed COM call is transient and the
    /// next call may well succeed, whereas a dead worker means *every*
    /// subsequent call will fail until the app restarts. Task 9 surfaces this
    /// through `AppState.last_error` and deliberately does not respawn.
    #[error("audio thread terminated")]
    ThreadTerminated,
    /// The audio worker accepted a command and never replied within
    /// [`thread::COMMAND_TIMEOUT`] — it is alive but wedged, almost certainly
    /// inside a COM call that is not coming back.
    ///
    /// Distinct from [`AudioError::ThreadTerminated`] because a dead worker
    /// disconnects its channels and unblocks its callers for free, whereas a
    /// hung one blocks them forever unless something bounds the wait. Tauri
    /// runs sync commands on the main thread, so an unbounded wait here would
    /// freeze the UI *while holding the `Core` lock* and take the tray's Quit
    /// item down with it.
    #[error("audio thread did not respond within {0:?}")]
    Timeout(std::time::Duration),
    #[error("windows audio error: {0}")]
    Windows(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// The only interface `modes.rs` is allowed to depend on, so the mode state
/// machine can be tested against `fake::FakeMic` without Core Audio (§3).
///
/// `MicControl` is [`MicBackend`] **plus the promise that the implementor can
/// cross a thread boundary**, and nothing else — it declares no methods of its
/// own. The blanket impl below means every `Send` backend is automatically a
/// `MicControl`, so each backend writes its seven method bodies exactly once and
/// there is no forwarding boilerplate anywhere in the tree.
///
/// The invariant this whole layer exists to enforce falls straight out of the
/// bound: `Endpoint` is a `MicBackend` but is not `Send`, so it can never be a
/// `MicControl`, and the only way to reach one from a `MicControl` caller is
/// through [`thread::MicHandle`]. See the `compile_fail` doctest on `Endpoint`.
pub trait MicControl: MicBackend + Send {}

impl<T: MicBackend + Send + ?Sized> MicControl for T {}

/// What the audio worker thread drives. Deliberately **not** `Send`:
/// implementors may be apartment-bound COM objects that are only ever legal to
/// touch from the one thread that created them.
///
/// [`MicControl`] is the `Send` public face; `MicBackend` is the confined inner
/// one. [`thread::MicHandle`] is the bridge — it owns the worker thread that
/// owns the backend, and forwards `MicControl` calls to it over a channel.
///
/// The method signatures are identical to `MicControl`'s minus the `Send`
/// bound, so the forwarding is pure delegation. Keep them in lockstep.
pub trait MicBackend {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError>;
    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError>;
    fn is_muted(&self) -> Result<bool, AudioError>;
    fn set_muted(&mut self, muted: bool) -> Result<(), AudioError>;
    fn volume(&self) -> Result<f32, AudioError>;
    fn set_volume(&mut self, level: f32) -> Result<(), AudioError>;
    fn peak(&self) -> Result<f32, AudioError>;
}

/// Converts a 0.0-1.0 peak amplitude to dBFS, clamped to [DBFS_FLOOR, 0.0].
/// A peak of zero or below maps to exactly DBFS_FLOOR rather than -inf.
pub fn peak_to_dbfs(peak: f32) -> f32 {
    if peak <= 0.0 {
        return DBFS_FLOOR;
    }
    (20.0 * peak.log10()).clamp(DBFS_FLOOR, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the shape Task 6's `modes.rs` will have: a function generic
    /// over `MicControl` calling backend methods through the supertrait.
    /// `modes.rs` holds only the `Mode` enum today, so nothing in the tree
    /// carries that bound yet — this makes sure the bound *works* before Task 6
    /// depends on it, and would stop compiling if the blanket impl ever stopped
    /// reaching a plain `Send` backend.
    fn toggle_through_mic_control<M: MicControl>(mic: &mut M) -> Result<bool, AudioError> {
        let before = mic.is_muted()?;
        mic.set_muted(!before)?;
        mic.is_muted()
    }

    #[test]
    fn a_mic_control_generic_resolves_for_a_send_backend() {
        let mut mic = crate::audio::fake::FakeMic::new();
        assert!(toggle_through_mic_control(&mut mic).unwrap());
        assert_eq!(mic.mute_calls, vec![true], "the call reached the backend");
    }

    #[test]
    fn silence_maps_to_floor() {
        assert_eq!(peak_to_dbfs(0.0), -60.0);
        assert_eq!(peak_to_dbfs(-0.5), -60.0, "negative peak must clamp, not NaN");
    }

    #[test]
    fn full_scale_maps_to_zero() {
        assert!((peak_to_dbfs(1.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn half_amplitude_is_about_minus_six_db() {
        assert!((peak_to_dbfs(0.5) + 6.0206).abs() < 1e-3);
    }

    #[test]
    fn very_quiet_signal_clamps_to_floor() {
        assert_eq!(peak_to_dbfs(0.000_001), -60.0);
    }

    #[test]
    fn above_full_scale_clamps_to_zero() {
        assert_eq!(peak_to_dbfs(2.0), 0.0);
    }
}

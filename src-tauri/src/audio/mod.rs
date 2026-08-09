pub mod endpoint;
#[cfg(test)]
pub mod fake;

use serde::Serialize;

/// dBFS floor. Anything at or below this reads as silence (§4.7).
pub const DBFS_FLOOR: f32 = -60.0;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no capture device available")]
    NoDevice,
    #[error("device {0} not found")]
    DeviceNotFound(String),
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
pub trait MicControl: Send {
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

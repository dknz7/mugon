//! Test double for `MicControl`. Records every mute call so tests can assert on
//! the exact sequence a mode transition produced.

use super::{AudioError, DeviceInfo, MicControl};

#[derive(Default)]
pub struct FakeMic {
    pub muted: bool,
    pub volume: f32,
    pub peak: f32,
    pub selected: Option<String>,
    /// Every value passed to `set_muted`, in order.
    pub mute_calls: Vec<bool>,
    pub fail_next: bool,
}

impl FakeMic {
    pub fn new() -> Self {
        Self { volume: 1.0, ..Default::default() }
    }
}

impl MicControl for FakeMic {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![DeviceInfo {
            id: "fake-1".into(),
            name: "Fake Microphone".into(),
            is_default: true,
        }])
    }
    fn select(&mut self, id: Option<&str>) -> Result<(), AudioError> {
        self.selected = id.map(str::to_owned);
        Ok(())
    }
    fn is_muted(&self) -> Result<bool, AudioError> { Ok(self.muted) }
    fn set_muted(&mut self, muted: bool) -> Result<(), AudioError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(AudioError::NoDevice);
        }
        self.mute_calls.push(muted);
        self.muted = muted;
        Ok(())
    }
    fn volume(&self) -> Result<f32, AudioError> { Ok(self.volume) }
    fn set_volume(&mut self, level: f32) -> Result<(), AudioError> {
        self.volume = level.clamp(0.0, 1.0);
        Ok(())
    }
    fn peak(&self) -> Result<f32, AudioError> { Ok(self.peak) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_calls_records_every_call_in_order_with_duplicates() {
        let mut mic = FakeMic::new();
        mic.set_muted(true).unwrap();
        mic.set_muted(true).unwrap();
        mic.set_muted(false).unwrap();
        assert_eq!(mic.mute_calls, vec![true, true, false]);
    }

    #[test]
    fn fail_next_errors_once_then_resets() {
        let mut mic = FakeMic::new();
        mic.fail_next = true;

        let result = mic.set_muted(true);
        assert!(matches!(result, Err(AudioError::NoDevice)));
        assert!(!mic.fail_next, "fail_next must reset to false after firing");

        // Next call succeeds now that fail_next has reset.
        assert!(mic.set_muted(true).is_ok());
    }

    #[test]
    fn failed_call_is_not_recorded_in_mute_calls() {
        let mut mic = FakeMic::new();
        mic.fail_next = true;
        let _ = mic.set_muted(true);
        assert!(mic.mute_calls.is_empty());
    }

    #[test]
    fn set_volume_clamps_to_unit_range() {
        let mut mic = FakeMic::new();
        mic.set_volume(-0.5).unwrap();
        assert_eq!(mic.volume().unwrap(), 0.0);

        mic.set_volume(1.5).unwrap();
        assert_eq!(mic.volume().unwrap(), 1.0);
    }

    #[test]
    fn select_stores_id_and_none_clears_it() {
        let mut mic = FakeMic::new();
        mic.select(Some("device-2")).unwrap();
        assert_eq!(mic.selected, Some("device-2".to_string()));

        mic.select(None).unwrap();
        assert_eq!(mic.selected, None);
    }

    #[test]
    fn is_muted_reflects_last_successful_set_muted() {
        let mut mic = FakeMic::new();
        mic.set_muted(true).unwrap();
        assert!(mic.is_muted().unwrap());

        mic.set_muted(false).unwrap();
        assert!(!mic.is_muted().unwrap());
    }
}

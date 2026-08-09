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

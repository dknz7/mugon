use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    MuteToggle,
    PushToTalk,
}

impl Default for Mode {
    fn default() -> Self { Mode::MuteToggle }
}

mod cuda;
mod diagnostics;
pub mod engine;
pub mod isolation;
pub mod latency;
pub mod playback;
pub mod preferences;
pub mod wasapi;
pub mod wav;

pub const RATE: usize = 16_000;
pub const HOP: usize = 128;
pub const PROFILE_SAMPLES: usize = RATE * 3;

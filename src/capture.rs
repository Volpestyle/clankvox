use tokio::time;

#[derive(Clone, Debug)]
pub(crate) struct SpeakingState {
    pub(crate) last_packet_at: Option<time::Instant>,
    pub(crate) is_speaking: bool,
}

pub(crate) const SPEAKING_TIMEOUT_MS: u64 = 100;

#[derive(Clone, Debug)]
pub(crate) struct UserCaptureState {
    pub(crate) sample_rate: u32,
    pub(crate) silence_duration_ms: u32,
    pub(crate) stream_active: bool,
    pub(crate) last_audio_at: Option<time::Instant>,
}

impl UserCaptureState {
    pub(crate) fn new(sample_rate: u32, silence_duration_ms: u32) -> Self {
        Self {
            sample_rate: normalize_sample_rate(sample_rate),
            silence_duration_ms: normalize_silence_duration_ms(silence_duration_ms),
            stream_active: false,
            last_audio_at: None,
        }
    }

    pub(crate) fn touch_audio(&mut self, now: time::Instant) {
        self.stream_active = true;
        self.last_audio_at = Some(now);
    }
}

pub(crate) fn normalize_sample_rate(sample_rate: u32) -> u32 {
    sample_rate.clamp(8_000, 48_000)
}

pub(crate) fn normalize_silence_duration_ms(duration_ms: u32) -> u32 {
    duration_ms.clamp(100, 5_000)
}

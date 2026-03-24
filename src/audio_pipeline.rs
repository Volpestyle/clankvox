use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use audiopus::coder::Encoder as OpusEncoder;
use audiopus::{Application, Channels, SampleRate};
use parking_lot::Mutex;
use tracing::{error, info, warn};

use crate::ipc::{send_msg, OutMsg};

struct GainEnvelope {
    current: f32,
    target: f32,
    step_per_sample: f32,
}

impl GainEnvelope {
    fn new(current: f32, target: f32, fade_ms: u32) -> Self {
        let total_samples = 48_000.0 * fade_ms as f64 / 1000.0;
        let step = if total_samples > 0.0 {
            (target as f64 - current as f64) / total_samples
        } else {
            0.0
        };
        Self {
            current,
            target,
            step_per_sample: step as f32,
        }
    }

    fn apply_sample(&mut self, sample: i16) -> i16 {
        let out = (sample as f32 * self.current).clamp(-32768.0, 32767.0) as i16;
        self.advance();
        out
    }

    fn advance(&mut self) {
        if (self.step_per_sample > 0.0 && self.current < self.target)
            || (self.step_per_sample < 0.0 && self.current > self.target)
        {
            self.current += self.step_per_sample;
            if (self.step_per_sample > 0.0 && self.current > self.target)
                || (self.step_per_sample < 0.0 && self.current < self.target)
            {
                self.current = self.target;
            }
        }
    }

    fn is_complete(&self) -> bool {
        (self.current - self.target).abs() < 0.0001
    }
}

const MAX_TRAILING_SILENCE: u32 = 5; // 100ms of trailing silence
pub(crate) const AUDIO_FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz mono
const MAX_PCM_BUFFER_SAMPLES: usize = 720_000; // 15 seconds @ 48kHz mono
pub(crate) const MAX_MUSIC_BUFFER_SAMPLES: usize = 96_000; // 2 seconds @ 48kHz mono
const PARTIAL_TTS_FLUSH_TICKS: u32 = 2; // Flush an underfilled tail after 40ms of no growth

/// Minimum TTS samples to accumulate before starting playback after the buffer
/// was empty.  This absorbs burst-latency gaps from streaming TTS providers
/// (e.g. ElevenLabs) at the cost of a small initial delay (~200ms).  Once the
/// pre-buffer is satisfied, playback runs continuously until the buffer drains.
const TTS_PREBUFFER_SAMPLES: usize = 9_600; // 200ms @ 48kHz mono

/// Maximum ticks to wait for the pre-buffer to fill before releasing the gate.
/// If no new samples arrive for this many consecutive ticks, the utterance is
/// shorter than TTS_PREBUFFER_SAMPLES and we should play it immediately rather
/// than holding it forever.  3 ticks = 60ms of no growth.
const TTS_PREBUFFER_STALL_TICKS: u32 = 3;

pub(crate) struct AudioSendState {
    pcm_buffer: VecDeque<i16>,   // TTS audio
    music_buffer: VecDeque<i16>, // Music audio (separate for mixing)
    music_gain: GainEnvelope,    // Gain envelope applied to music
    music_gain_notified: f32,    // Last gain value we sent MusicGainReached for
    music_output_suppressed: bool,
    encoder: OpusEncoder,
    speaking: bool,
    trailing_silence_frames: u32,
    partial_tts_stall_ticks: u32,
    /// When true, TTS playback has started and the pre-buffer threshold no
    /// longer applies until the buffer fully drains back to zero.
    tts_prebuffer_satisfied: bool,
    /// Tracks how many consecutive ticks the TTS buffer has been below
    /// TTS_PREBUFFER_SAMPLES without growing.  When this reaches
    /// TTS_PREBUFFER_STALL_TICKS the gate releases — the utterance is
    /// shorter than the threshold and no more data is coming.
    tts_prebuffer_stall_ticks: u32,
    /// Snapshot of the TTS buffer size on the previous prebuffer-gate tick,
    /// used to detect whether the buffer is still growing.
    tts_prebuffer_last_len: usize,
}

impl AudioSendState {
    pub(crate) fn new() -> Result<Self> {
        let encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| anyhow::anyhow!("Opus encoder init: {e:?}"))?;
        // Pre-allocate for ~1 second of audio at 48kHz mono. This avoids
        // repeated small allocations during the initial buffering phase.
        Ok(Self {
            pcm_buffer: VecDeque::with_capacity(48_000),
            music_buffer: VecDeque::with_capacity(48_000),
            music_gain: GainEnvelope::new(1.0, 1.0, 0),
            music_gain_notified: 1.0,
            music_output_suppressed: false,
            encoder,
            speaking: false,
            trailing_silence_frames: 0,
            partial_tts_stall_ticks: 0,
            tts_prebuffer_satisfied: false,
            tts_prebuffer_stall_ticks: 0,
            tts_prebuffer_last_len: 0,
        })
    }

    pub(crate) fn push_pcm(&mut self, samples: Vec<i16>) {
        self.pcm_buffer.extend(samples);
        // Drop newest samples to keep the buffer bounded without skipping ahead
        // through speech that is already queued for playback.
        if self.pcm_buffer.len() > MAX_PCM_BUFFER_SAMPLES {
            let overflow = self.pcm_buffer.len() - MAX_PCM_BUFFER_SAMPLES;
            warn!(
                "TTS PCM buffer overflow: dropping {} newest samples ({:.1}ms), buffer was {} samples ({:.1}ms)",
                overflow,
                overflow as f64 / 48.0,
                self.pcm_buffer.len(),
                self.pcm_buffer.len() as f64 / 48.0
            );
            self.pcm_buffer.truncate(MAX_PCM_BUFFER_SAMPLES);
        }
        self.trailing_silence_frames = 0;
        self.partial_tts_stall_ticks = 0;
    }

    pub(crate) fn push_music_pcm(&mut self, samples: Vec<i16>) {
        self.music_buffer.extend(samples);
        self.trailing_silence_frames = 0;
    }

    pub(crate) fn can_accept_music_chunk(&self) -> bool {
        self.music_buffer.len().saturating_add(AUDIO_FRAME_SAMPLES) <= MAX_MUSIC_BUFFER_SAMPLES
    }

    pub(crate) fn suppress_music_output(&mut self) {
        self.music_output_suppressed = true;
    }

    pub(crate) fn resume_music_output(&mut self) {
        self.music_output_suppressed = false;
        self.trailing_silence_frames = 0;
    }

    pub(crate) fn set_music_gain(&mut self, target: f32, fade_ms: u32) -> Option<f32> {
        let current = if fade_ms == 0 {
            target
        } else {
            self.music_gain.current
        };
        self.music_gain = GainEnvelope::new(current, target, fade_ms);
        if fade_ms == 0 {
            self.music_gain_notified = target;
            Some(target)
        } else {
            None
        }
    }

    pub(crate) fn begin_music_fade_in(&mut self, fade_ms: u32) {
        self.music_gain = GainEnvelope::new(0.0, 1.0, fade_ms);
        self.music_gain_notified = 0.0;
    }

    pub(crate) fn maybe_take_music_gain_reached(&mut self) -> Option<f32> {
        if self.music_gain.is_complete()
            && (self.music_gain_notified - self.music_gain.target).abs() > 0.0001
        {
            let reached = self.music_gain.target;
            self.music_gain_notified = reached;
            Some(reached)
        } else {
            None
        }
    }

    pub(crate) fn is_music_fade_out_complete(&self) -> bool {
        self.music_gain.is_complete() && self.music_gain.target < 0.001
    }

    pub(crate) fn is_music_ducked(&self) -> bool {
        self.music_gain.target < 1.0
    }

    pub(crate) fn tts_is_empty(&self) -> bool {
        self.pcm_buffer.is_empty()
    }

    pub(crate) fn tts_buffer_samples(&self) -> usize {
        self.pcm_buffer.len()
    }

    pub(crate) fn music_buffer_samples(&self) -> usize {
        self.music_buffer.len()
    }

    #[cfg(test)]
    pub(crate) fn is_music_output_suppressed(&self) -> bool {
        self.music_output_suppressed
    }

    fn clear(&mut self) {
        self.pcm_buffer.clear();
        self.music_buffer.clear();
        self.music_output_suppressed = false;
        self.trailing_silence_frames = MAX_TRAILING_SILENCE;
        self.partial_tts_stall_ticks = 0;
        self.tts_prebuffer_satisfied = false;
        self.tts_prebuffer_stall_ticks = 0;
        self.tts_prebuffer_last_len = 0;
    }

    fn clear_tts(&mut self) {
        self.pcm_buffer.clear();
        self.partial_tts_stall_ticks = 0;
        self.tts_prebuffer_satisfied = false;
        self.tts_prebuffer_stall_ticks = 0;
        self.tts_prebuffer_last_len = 0;
    }

    fn clear_music(&mut self) {
        self.music_buffer.clear();
        self.music_output_suppressed = false;
        self.trailing_silence_frames = MAX_TRAILING_SILENCE;
    }

    /// Returns true when the TTS buffer has just transitioned from non-empty to
    /// empty (the trailing silence frames have all been sent).  The caller
    /// should emit an immediate drain notification to the TS side so the
    /// output-lock state converges without waiting for the periodic report.
    pub(crate) fn tts_just_drained(&self) -> bool {
        // The buffer is empty, we were speaking, and we have exhausted all
        // trailing silence frames — meaning this tick is the first fully-idle
        // tick after playback finished.
        !self.speaking
            && self.pcm_buffer.is_empty()
            && self.trailing_silence_frames >= MAX_TRAILING_SILENCE
            && !self.tts_prebuffer_satisfied
    }

    /// Encode the next 20ms frame, mixing TTS and music buffers.
    /// Music samples have the gain envelope applied unless music output is
    /// temporarily suppressed for a wake-word pause. Returns None if idle.
    pub(crate) fn next_opus_frame(&mut self) -> Option<Vec<u8>> {
        let available_tts = self.pcm_buffer.len();
        let available_music = self.music_buffer.len();
        let has_music = !self.music_output_suppressed && available_music >= AUDIO_FRAME_SAMPLES;

        // Pre-buffer gate: when transitioning from empty to having TTS data,
        // wait until the buffer reaches TTS_PREBUFFER_SAMPLES before starting
        // playback.  This absorbs TTS streaming burst gaps so the output is
        // continuous.  Once the threshold is met, play normally until the
        // buffer fully drains back to zero.
        //
        // Safety valve: if the buffer stops growing for TTS_PREBUFFER_STALL_TICKS
        // (60ms) without reaching the threshold, the utterance is shorter than
        // the pre-buffer target — release the gate and play what we have.
        if !self.tts_prebuffer_satisfied
            && available_tts > 0
            && available_tts < TTS_PREBUFFER_SAMPLES
        {
            if available_tts > self.tts_prebuffer_last_len {
                // Buffer is still growing — reset the stall counter.
                self.tts_prebuffer_stall_ticks = 0;
            } else {
                self.tts_prebuffer_stall_ticks = self.tts_prebuffer_stall_ticks.saturating_add(1);
            }
            self.tts_prebuffer_last_len = available_tts;

            if self.tts_prebuffer_stall_ticks < TTS_PREBUFFER_STALL_TICKS && !has_music {
                // Still accumulating — don't produce TTS output yet.
                self.partial_tts_stall_ticks = 0;
                return None;
            }
            // Stall timeout reached — short utterance, release the gate.
            self.tts_prebuffer_satisfied = true;
            info!(
                buffered_samples = available_tts,
                buffered_ms = available_tts as f64 / 48.0,
                stall_ticks = self.tts_prebuffer_stall_ticks,
                "clankvox_tts_prebuffer_satisfied_short_utterance"
            );
        }
        if available_tts >= TTS_PREBUFFER_SAMPLES && !self.tts_prebuffer_satisfied {
            self.tts_prebuffer_satisfied = true;
            self.tts_prebuffer_stall_ticks = 0;
            self.tts_prebuffer_last_len = 0;
            info!(
                buffered_samples = available_tts,
                buffered_ms = available_tts as f64 / 48.0,
                "clankvox_tts_prebuffer_satisfied"
            );
        }

        let has_full_tts = available_tts >= AUDIO_FRAME_SAMPLES;
        let has_partial_tts = available_tts > 0 && available_tts < AUDIO_FRAME_SAMPLES;

        if has_full_tts {
            self.partial_tts_stall_ticks = 0;
        } else if has_partial_tts {
            self.partial_tts_stall_ticks = self.partial_tts_stall_ticks.saturating_add(1);
        } else {
            self.partial_tts_stall_ticks = 0;
        }

        let flush_partial_tts = has_partial_tts
            && (has_music || self.partial_tts_stall_ticks >= PARTIAL_TTS_FLUSH_TICKS);
        let tts_samples_to_take = if has_full_tts {
            AUDIO_FRAME_SAMPLES
        } else if flush_partial_tts {
            available_tts
        } else {
            0
        };
        let has_tts = tts_samples_to_take > 0;

        if has_tts || has_music {
            let mut mixed = [0i32; AUDIO_FRAME_SAMPLES];

            if has_music {
                for (i, s) in self.music_buffer.drain(..AUDIO_FRAME_SAMPLES).enumerate() {
                    mixed[i] += self.music_gain.apply_sample(s) as i32;
                }
            }
            if has_tts {
                if flush_partial_tts && tts_samples_to_take < AUDIO_FRAME_SAMPLES {
                    info!(
                        queued_samples = available_tts,
                        stall_ticks = self.partial_tts_stall_ticks,
                        "clankvox_tts_partial_frame_flushed"
                    );
                }
                for (i, s) in self.pcm_buffer.drain(..tts_samples_to_take).enumerate() {
                    mixed[i] += s as i32; // TTS at full volume always
                }
            }

            let pcm: Vec<i16> = mixed
                .iter()
                .map(|&s| s.clamp(-32768, 32767) as i16)
                .collect();

            let mut opus_buf = vec![0u8; 4000];
            match self.encoder.encode(&pcm, &mut opus_buf) {
                Ok(len) => {
                    self.speaking = true;
                    self.trailing_silence_frames = 0;
                    self.partial_tts_stall_ticks = 0;
                    return Some(opus_buf[..len].to_vec());
                }
                Err(e) => {
                    error!("Opus encode error: {:?}", e);
                    return None;
                }
            }
        }

        if has_partial_tts {
            // Hold a short underfilled tail briefly so adjacent deltas can coalesce
            // into a full 20ms frame; otherwise we would pad too aggressively and
            // create choppy playback. If the tail does not grow, a later tick will
            // flush it as a padded final frame.
            return None;
        }

        // Buffer empty — send trailing silence to avoid abrupt cutoff
        if self.trailing_silence_frames < MAX_TRAILING_SILENCE {
            self.trailing_silence_frames += 1;
            // Opus silence frame (RFC 6716 comfort noise)
            return Some(vec![0xF8, 0xFF, 0xFE]);
        }

        if self.speaking {
            self.speaking = false;
            // Buffer is fully drained and trailing silence is done — reset the
            // pre-buffer gate so the next batch of TTS audio gets the full
            // pre-buffer treatment.
            self.tts_prebuffer_satisfied = false;
            self.tts_prebuffer_stall_ticks = 0;
            self.tts_prebuffer_last_len = 0;
        }
        None
    }
}

/// Windowed-sinc low-pass FIR filter + polyphase resampling.
///
/// For integer-ratio downsampling (e.g. 48 kHz → 24 kHz) this applies a
/// proper anti-aliasing filter before decimation, preventing high-frequency
/// content from folding back into the output band.
///
/// For non-integer ratios the filter kernel is interpolated at fractional
/// positions (polyphase decomposition), which is equivalent to a high-quality
/// sinc resampler.
///
/// Filter parameters:
///   - Kernel half-length: 16 taps per lobe (32-tap symmetric FIR)
///   - Window: Blackman (excellent stopband attenuation ≈ −74 dB)
///   - Cutoff: 0.45 × min(in_rate, out_rate) to leave transition room
pub(crate) fn resample_mono_i16(input: &[i16], in_rate: u32, out_rate: u32) -> Vec<i16> {
    if in_rate == out_rate || input.len() <= 1 {
        return input.to_vec();
    }
    let ratio = in_rate as f64 / out_rate as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    if out_len == 0 {
        return vec![];
    }

    // Number of zero-crossings on each side of the sinc kernel.
    const SINC_HALF_LEN: usize = 16;

    // Cutoff relative to the lower of the two rates, with margin for the
    // transition band.
    let cutoff = 0.45 / ratio.max(1.0);

    let mut output = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let center = i as f64 * ratio;
        let i_center = center.floor() as isize;

        let mut sum = 0.0f64;
        let mut weight_sum = 0.0f64;

        let start = (i_center - SINC_HALF_LEN as isize).max(0);
        let end = (i_center + SINC_HALF_LEN as isize + 1).min(input.len() as isize);

        for j in start..end {
            let x = (j as f64 - center) * cutoff * 2.0;
            // sinc(x) = sin(πx)/(πx), with sinc(0) = 1
            let sinc = if x.abs() < 1e-10 {
                1.0
            } else {
                let px = std::f64::consts::PI * x;
                px.sin() / px
            };
            // Blackman window
            let n = j as f64 - center;
            let win_pos = (n / SINC_HALF_LEN as f64 + 1.0) * 0.5; // 0..1
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * win_pos).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * win_pos).cos();

            let kernel = sinc * w;
            sum += input[j as usize] as f64 * kernel;
            weight_sum += kernel;
        }

        let sample = if weight_sum.abs() > 1e-10 {
            sum / weight_sum
        } else {
            0.0
        };
        output.push(sample.round().clamp(-32768.0, 32767.0) as i16);
    }
    output
}

/// Convert LLM output (mono i16 LE at `in_rate`) to 48kHz mono i16 for Opus encoding.
pub(crate) fn convert_llm_to_48k_mono(pcm: &[u8], in_rate: u32) -> Vec<i16> {
    let sample_count = pcm.len() / 2;
    if sample_count == 0 {
        return vec![];
    }
    let mut mono = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        mono.push(i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]));
    }
    resample_mono_i16(&mono, in_rate, 48000)
}

/// Convert decoded stereo i16 48kHz to LLM input (mono i16 LE at `out_rate`).
/// Returns (LLM input, `signal_peak_abs`, `signal_active_sample_count`, `signal_sample_count`)
pub(crate) fn convert_decoded_to_llm(
    stereo_i16: &[i16],
    out_rate: u32,
) -> (Vec<u8>, u16, usize, usize) {
    let frame_count = stereo_i16.len() / 2;
    if frame_count == 0 {
        return (vec![], 0, 0, 0);
    }
    // Stereo-to-mono downmix via averaging.  The /2 inherently attenuates by
    // 6 dB — this is standard for mono downmix and is accounted for by the
    // downstream signal thresholds.  Using (L+R+1)>>1 instead of (L+R)/2
    // rounds to nearest rather than truncating toward zero, which eliminates
    // a small DC bias on low-amplitude signals.
    let mut mono = Vec::with_capacity(frame_count);
    for i in 0..frame_count {
        let l = stereo_i16[i * 2] as i32;
        let r = stereo_i16[i * 2 + 1] as i32;
        mono.push(((l + r + 1) >> 1).clamp(-32768, 32767) as i16);
    }
    let resampled = resample_mono_i16(&mono, 48000, out_rate);
    let mut buf = Vec::with_capacity(resampled.len() * 2);
    let mut max_amp: u16 = 0;
    let mut active_samples = 0;
    for &s in &resampled {
        buf.extend_from_slice(&s.to_le_bytes());
        let abs_val = s.unsigned_abs();
        if abs_val > max_amp {
            max_amp = abs_val;
        }
        if abs_val > 500 {
            active_samples += 1;
        }
    }
    (buf, max_amp, active_samples, resampled.len())
}

pub(crate) fn clear_audio_send_buffer(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.clear();
    }
}

pub(crate) fn clear_tts_send_buffer(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.clear_tts();
    }
}

pub(crate) fn clear_music_send_buffer(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.clear_music();
    }
}

pub(crate) fn suppress_music_output(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.suppress_music_output();
    }
}

pub(crate) fn resume_music_output(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.resume_music_output();
    }
}

pub(crate) fn has_buffered_music_output(
    audio_send_state: &Arc<Mutex<Option<AudioSendState>>>,
) -> bool {
    let guard = audio_send_state.lock();
    guard
        .as_ref()
        .is_some_and(|state| state.music_buffer_samples() > 0)
}

pub(crate) fn emit_playback_armed(
    reason: &str,
    audio_send_state: &Arc<Mutex<Option<AudioSendState>>>,
) {
    if audio_send_state.lock().is_some() {
        send_msg(&OutMsg::PlaybackArmed {
            reason: reason.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioSendState, MAX_PCM_BUFFER_SAMPLES};

    #[test]
    fn tts_partial_tail_flushes_after_short_stall() {
        let mut state = AudioSendState::new().expect("audio state");
        // Bypass prebuffer gate — this test is about partial-tail flush behavior
        state.tts_prebuffer_satisfied = true;
        state.push_pcm(vec![123; 480]);

        assert_eq!(state.tts_buffer_samples(), 480);
        assert!(state.next_opus_frame().is_none());
        assert_eq!(state.tts_buffer_samples(), 480);

        let frame = state.next_opus_frame().expect("partial tail should flush");
        assert!(!frame.is_empty());
        assert_eq!(state.tts_buffer_samples(), 0);
    }

    #[test]
    fn tts_partial_tail_coalesces_before_flush_threshold() {
        let mut state = AudioSendState::new().expect("audio state");
        // Bypass prebuffer gate — this test is about tail coalescing behavior
        state.tts_prebuffer_satisfied = true;
        state.push_pcm(vec![123; 480]);

        assert!(state.next_opus_frame().is_none());
        state.push_pcm(vec![123; 480]);

        let frame = state
            .next_opus_frame()
            .expect("full frame should encode once tail grows");
        assert!(!frame.is_empty());
        assert_eq!(state.tts_buffer_samples(), 0);
    }

    #[test]
    fn tts_prebuffer_gate_holds_output_until_threshold() {
        let mut state = AudioSendState::new().expect("audio state");
        // Push less than TTS_PREBUFFER_SAMPLES — should be held
        state.push_pcm(vec![123; 960]);
        assert!(!state.tts_prebuffer_satisfied);
        assert!(
            state.next_opus_frame().is_none(),
            "prebuffer should hold output"
        );
        assert_eq!(
            state.tts_buffer_samples(),
            960,
            "samples should remain in buffer"
        );

        // Push enough to cross the threshold
        state.push_pcm(vec![123; super::TTS_PREBUFFER_SAMPLES]);
        assert!(!state.tts_prebuffer_satisfied);
        let frame = state
            .next_opus_frame()
            .expect("prebuffer satisfied, should produce frame");
        assert!(!frame.is_empty());
        assert!(state.tts_prebuffer_satisfied);
    }

    #[test]
    fn tts_prebuffer_releases_short_utterance_after_stall() {
        let mut state = AudioSendState::new().expect("audio state");
        // Push a short utterance (under TTS_PREBUFFER_SAMPLES)
        state.push_pcm(vec![123; 960]);
        assert!(!state.tts_prebuffer_satisfied);

        // Tick 1: first tick sees growth (0 -> 960), resets stall counter
        assert!(state.next_opus_frame().is_none());
        // Tick 2: no growth, stall_ticks -> 1
        assert!(state.next_opus_frame().is_none());
        // Tick 3: no growth, stall_ticks -> 2
        assert!(state.next_opus_frame().is_none());
        // Tick 4: stall_ticks reaches TTS_PREBUFFER_STALL_TICKS (3) -> gate releases
        let frame = state
            .next_opus_frame()
            .expect("short utterance should play after stall timeout");
        assert!(!frame.is_empty());
        assert!(state.tts_prebuffer_satisfied);
    }

    #[test]
    fn suppressed_music_output_preserves_buffer_until_resumed() {
        let mut state = AudioSendState::new().expect("audio state");
        state.push_music_pcm(vec![123; 960]);
        state.suppress_music_output();

        for _ in 0..3 {
            let _ = state.next_opus_frame();
        }

        assert_eq!(state.music_buffer_samples(), 960);
        assert!(state.is_music_output_suppressed());

        state.resume_music_output();
        let frame = state
            .next_opus_frame()
            .expect("music frame should encode after resume");
        assert!(!frame.is_empty());
        assert_eq!(state.music_buffer_samples(), 0);
        assert!(!state.is_music_output_suppressed());
    }

    #[test]
    fn clear_music_preserves_tts_buffer() {
        let mut state = AudioSendState::new().expect("audio state");
        state.push_pcm(vec![123; 480]);
        state.push_music_pcm(vec![456; 960]);
        state.suppress_music_output();

        state.clear_music();

        assert_eq!(state.tts_buffer_samples(), 480);
        assert_eq!(state.music_buffer_samples(), 0);
        assert!(!state.is_music_output_suppressed());
    }

    #[test]
    fn tts_overflow_drops_newest_tail_instead_of_skipping_buffered_speech() {
        let mut state = AudioSendState::new().expect("audio state");
        state.push_pcm(vec![111; MAX_PCM_BUFFER_SAMPLES]);
        state.push_pcm(vec![222; 960]);

        assert_eq!(state.tts_buffer_samples(), MAX_PCM_BUFFER_SAMPLES);
        assert_eq!(state.pcm_buffer.front().copied(), Some(111));
        assert_eq!(state.pcm_buffer.back().copied(), Some(111));
    }
}

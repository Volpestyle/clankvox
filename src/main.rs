#![allow(dead_code)]

mod dave;
mod voice_conn;

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use audiopus::coder::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use audiopus::packet::Packet as OpusPacket;
use audiopus::{Application, Channels, MutSignals, SampleRate};
use base64::Engine as _;
use crossbeam_channel as crossbeam;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::dave::DaveManager;
use crate::voice_conn::{VoiceConnection, VoiceEvent};

// ---------------------------------------------------------------------------
// Global IPC output channel — all threads send OutMsg through this channel,
// a dedicated writer thread serializes and flushes them to stdout.
// ---------------------------------------------------------------------------

static IPC_TX: std::sync::OnceLock<crossbeam::Sender<OutMsg>> = std::sync::OnceLock::new();

// ---------------------------------------------------------------------------
// IPC message types — must match clankvoxClient.ts exactly
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum InMsg {
    Join {
        #[serde(rename = "guildId")]
        guild_id: String,
        #[serde(rename = "channelId")]
        channel_id: String,
        #[serde(rename = "selfDeaf", default)]
        self_deaf: bool,
        #[serde(rename = "selfMute", default)]
        self_mute: bool,
    },
    VoiceServer {
        data: Value,
    },
    VoiceState {
        data: Value,
    },
    Audio {
        #[serde(rename = "pcmBase64")]
        pcm_base64: String,
        #[serde(rename = "sampleRate", default = "default_sample_rate")]
        sample_rate: u32,
    },
    StopPlayback,
    StopTtsPlayback,
    SubscribeUser {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(rename = "silenceDurationMs", default = "default_silence_duration")]
        silence_duration_ms: u32,
        #[serde(rename = "sampleRate", default = "default_sample_rate")]
        sample_rate: u32,
    },
    UnsubscribeUser {
        #[serde(rename = "userId")]
        user_id: String,
    },
    MusicPlay {
        url: String,
        #[serde(rename = "resolvedDirectUrl", default)]
        resolved_direct_url: bool,
    },
    MusicStop,
    MusicPause,
    MusicResume,
    MusicSetGain {
        target: f32,
        #[serde(rename = "fadeMs", default)]
        fade_ms: u32,
    },
    ConnectAsr {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(rename = "apiKey")]
        api_key: String,
        model: String,
        language: Option<String>,
        prompt: Option<String>,
    },
    DisconnectAsr {
        #[serde(rename = "userId")]
        user_id: String,
    },
    CommitAsr {
        #[serde(rename = "userId")]
        user_id: String,
    },
    ClearAsr {
        #[serde(rename = "userId")]
        user_id: String,
    },
    Destroy,
}

fn default_sample_rate() -> u32 {
    24000
}
fn default_silence_duration() -> u32 {
    700
}

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
enum OutMsg {
    Ready,
    AdapterSend {
        payload: Value,
    },
    ConnectionState {
        status: String,
    },
    PlayerState {
        status: String,
    },
    PlaybackArmed {
        reason: String,
    },
    TtsPlaybackState {
        status: String,
    },
    SpeakingStart {
        #[serde(rename = "userId")]
        user_id: String,
    },
    SpeakingEnd {
        #[serde(rename = "userId")]
        user_id: String,
    },
    UserAudio {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(skip)]
        pcm: Vec<u8>,
        #[serde(skip)]
        signal_peak_abs: u16,
        #[serde(skip)]
        signal_active_sample_count: usize,
        #[serde(skip)]
        signal_sample_count: usize,
    },
    UserAudioEnd {
        #[serde(rename = "userId")]
        user_id: String,
    },
    ClientDisconnect {
        #[serde(rename = "userId")]
        user_id: String,
    },
    MusicIdle,
    MusicError {
        message: String,
    },
    MusicGainReached {
        gain: f32,
    },
    AsrTranscript {
        #[serde(rename = "userId")]
        user_id: String,
        text: String,
        #[serde(rename = "isFinal")]
        is_final: bool,
    },
    AsrDisconnected {
        #[serde(rename = "userId")]
        user_id: String,
        reason: String,
    },
    Error {
        message: String,
    },
    BufferDepth {
        #[serde(rename = "ttsSamples")]
        tts_samples: usize,
        #[serde(rename = "musicSamples")]
        music_samples: usize,
    },
}

fn send_msg(msg: &OutMsg) {
    if let Some(tx) = IPC_TX.get() {
        let _ = tx.try_send(msg.clone());
    }
}

fn send_error(message: &str) {
    send_msg(&OutMsg::Error {
        message: message.to_string(),
    });
}

fn send_tts_playback_state(status: &str) {
    send_msg(&OutMsg::TtsPlaybackState {
        status: status.to_string(),
    });
}

fn spawn_ipc_writer() -> crossbeam::Sender<OutMsg> {
    // Bounded channel: at 250 audio msgs/sec (5 users × 50 fps) this gives ~2s of headroom.
    // If the parent process stalls on reading stdout, audio frames are dropped rather than OOMing.
    let (tx, rx) = crossbeam::bounded::<OutMsg>(512);
    std::thread::spawn(move || {
        let mut out = io::stdout().lock();
        for msg in rx {
            match msg {
                OutMsg::UserAudio {
                    user_id,
                    pcm,
                    signal_peak_abs,
                    signal_active_sample_count,
                    signal_sample_count,
                } => {
                    let uid = user_id.parse::<u64>().unwrap_or(0);
                    let mut payload = Vec::with_capacity(8 + 2 + 4 + 4 + pcm.len());
                    payload.extend_from_slice(&uid.to_le_bytes());
                    payload.extend_from_slice(&signal_peak_abs.to_le_bytes());
                    payload.extend_from_slice(&(signal_active_sample_count as u32).to_le_bytes());
                    payload.extend_from_slice(&(signal_sample_count as u32).to_le_bytes());
                    payload.extend_from_slice(&pcm);

                    let len = payload.len() as u32;
                    let _ = out.write_all(&[1]); // Format 1 = Binary Audio
                    let _ = out.write_all(&len.to_le_bytes());
                    let _ = out.write_all(&payload);
                    let _ = out.flush();
                }
                other => {
                    if let Ok(json) = serde_json::to_string(&other) {
                        let payload = json.as_bytes();
                        let len = payload.len() as u32;
                        let _ = out.write_all(&[0]); // Format 0 = JSON
                        let _ = out.write_all(&len.to_le_bytes());
                        let _ = out.write_all(payload);
                        let _ = out.flush();
                    }
                }
            }
        }
    });
    tx
}

// ---------------------------------------------------------------------------
// ASR WebSocket Client
// ---------------------------------------------------------------------------

use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;

pub enum AsrCommand {
    Audio(Vec<u8>),
    Commit,
    Clear,
}

async fn run_asr_client(
    user_id: String,
    api_key: String,
    model: String,
    language: Option<String>,
    prompt: Option<String>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<AsrCommand>,
) -> anyhow::Result<()> {
    let url = "wss://api.openai.com/v1/realtime?intent=transcription";
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {}", api_key).parse()?);
    request
        .headers_mut()
        .insert("OpenAI-Beta", "realtime=v1".parse()?);

    let (ws_stream, _) = connect_async(request).await?;
    let (mut write, mut read) = ws_stream.split();

    // Send session setup
    let mut transcription_cfg = json!({
        "model": model,
    });
    if let Some(l) = language {
        if !l.is_empty() {
            transcription_cfg
                .as_object_mut()
                .unwrap()
                .insert("language".into(), json!(l));
        }
    }
    if let Some(p) = prompt {
        if !p.is_empty() {
            transcription_cfg
                .as_object_mut()
                .unwrap()
                .insert("prompt".into(), json!(p));
        }
    }

    let setup_msg = json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": {
                        "type": "audio/pcm",
                        "rate": 24000
                    },
                    "noise_reduction": {
                        "type": "near_field"
                    },
                    "turn_detection": serde_json::Value::Null,
                    "transcription": transcription_cfg
                }
            }
        },
        "include": ["item.input_audio_transcription.logprobs"]
    });

    write.send(Message::Text(setup_msg.to_string())).await?;

    loop {
        tokio::select! {
            Some(cmd) = rx.recv() => {
                match cmd {
                    AsrCommand::Audio(pcm) => {
                        let b64 = STANDARD.encode(&pcm);
                        let append_msg = json!({
                            "type": "input_audio_buffer.append",
                            "audio": b64
                        });
                        if write.send(Message::Text(append_msg.to_string())).await.is_err() {
                            break;
                        }
                    }
                    AsrCommand::Commit => {
                        let commit_msg = json!({
                            "type": "input_audio_buffer.commit"
                        });
                        let _ = write.send(Message::Text(commit_msg.to_string())).await;
                    }
                    AsrCommand::Clear => {
                        let clear_msg = json!({
                            "type": "input_audio_buffer.clear"
                        });
                        let _ = write.send(Message::Text(clear_msg.to_string())).await;
                    }
                }
            }
            Some(msg_res) = read.next() => {
                let msg = match msg_res {
                    Ok(m) => m,
                    Err(_) => break, // WebSocket closed or errored
                };
                if let Message::Text(text) = msg {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if msg_type == "conversation.item.input_audio_transcription.delta" {
                            if let Some(delta) = value.get("delta").and_then(|d| d.as_str()) {
                                send_msg(&OutMsg::AsrTranscript {
                                    user_id: user_id.clone(),
                                    text: delta.to_string(),
                                    is_final: false,
                                });
                            }
                        } else if msg_type == "conversation.item.input_audio_transcription.completed" {
                            if let Some(transcript) = value.get("transcript").and_then(|t| t.as_str()) {
                                send_msg(&OutMsg::AsrTranscript {
                                    user_id: user_id.clone(),
                                    text: transcript.to_string(),
                                    is_final: true,
                                });
                            }
                        } else if msg_type == "error" {
                            if let Some(err) = value.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
                                error!("OpenAI ASR error for user {}: {}", user_id, err);
                            }
                        }
                    }
                }
            }
            else => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_output_not_drained_while_pcm_queue_has_chunks() {
        let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));

        music_pcm_tx.send(vec![0; 960]).expect("queue chunk");

        assert!(!is_music_output_drained(&music_pcm_rx, &audio_send_state));
    }

    #[test]
    fn music_output_not_drained_while_mixer_buffer_has_music() {
        let (_music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));
        {
            let mut guard = audio_send_state.lock();
            let state = guard.as_mut().expect("state");
            state.push_music_pcm(vec![0; 960]);
        }

        assert!(!is_music_output_drained(&music_pcm_rx, &audio_send_state));
    }

    #[test]
    fn music_output_drained_when_queue_and_mixer_are_empty() {
        let (_music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));

        assert!(is_music_output_drained(&music_pcm_rx, &audio_send_state));
    }

    #[test]
    fn direct_music_pipeline_command_skips_ytdlp() {
        let command = build_music_pipeline_command("https://cdn.example.com/audio.m4a", true);
        assert!(command.starts_with("ffmpeg "));
        assert!(!command.contains("yt-dlp"));
    }

    #[test]
    fn unresolved_music_pipeline_command_uses_ytdlp() {
        let command = build_music_pipeline_command("https://www.youtube.com/watch?v=abc123", false);
        assert!(command.contains("yt-dlp"));
        assert!(command.contains("| ffmpeg "));
    }
}

// ---------------------------------------------------------------------------
// PCM resampling
// ---------------------------------------------------------------------------

fn resample_mono_i16(input: &[i16], in_rate: u32, out_rate: u32) -> Vec<i16> {
    if in_rate == out_rate || input.len() <= 1 {
        return input.to_vec();
    }
    let ratio = in_rate as f64 / out_rate as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    if out_len == 0 {
        return vec![];
    }
    let mut output = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let next = (idx + 1).min(input.len() - 1);
        let frac = src_pos - idx as f64;
        let sample = input[idx] as f64 + frac * (input[next] as f64 - input[idx] as f64);
        output.push(sample.round().clamp(-32768.0, 32767.0) as i16);
    }
    output
}

/// Convert LLM output (mono i16 LE at `in_rate`) to 48kHz mono i16 for Opus encoding.
fn convert_llm_to_48k_mono(pcm: &[u8], in_rate: u32) -> Vec<i16> {
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
/// Returns (LLM input, signal_peak_abs, signal_active_sample_count, signal_sample_count)
fn convert_decoded_to_llm(stereo_i16: &[i16], out_rate: u32) -> (Vec<u8>, u16, usize, usize) {
    let frame_count = stereo_i16.len() / 2;
    if frame_count == 0 {
        return (vec![], 0, 0, 0);
    }
    let mut mono = Vec::with_capacity(frame_count);
    for i in 0..frame_count {
        let l = stereo_i16[i * 2] as i32;
        let r = stereo_i16[i * 2 + 1] as i32;
        mono.push(((l + r) / 2).clamp(-32768, 32767) as i16);
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

// ---------------------------------------------------------------------------
// Partial voice connection info (accumulated from voice_server + voice_state)
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct PendingConnection {
    endpoint: Option<String>,
    token: Option<String>,
    session_id: Option<String>,
    user_id: Option<u64>,
}

impl PendingConnection {
    fn is_complete(&self) -> bool {
        self.endpoint.is_some()
            && self.token.is_some()
            && self.session_id.is_some()
            && self.user_id.is_some()
    }
}

// ---------------------------------------------------------------------------
// Gain envelope for smooth volume transitions
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Audio send state (outbound TTS + music pipeline with mixing)
// ---------------------------------------------------------------------------

struct AudioSendState {
    pcm_buffer: VecDeque<i16>,   // TTS audio
    music_buffer: VecDeque<i16>, // Music audio (separate for mixing)
    music_gain: GainEnvelope,    // Gain envelope applied to music
    music_gain_notified: f32,    // Last gain value we sent MusicGainReached for
    encoder: OpusEncoder,
    speaking: bool,
    trailing_silence_frames: u32,
}

const MAX_TRAILING_SILENCE: u32 = 5; // 100ms of trailing silence
const MAX_PCM_BUFFER_SAMPLES: usize = 720_000; // 15 seconds @ 48kHz mono

impl AudioSendState {
    fn new() -> Result<Self> {
        let encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| anyhow::anyhow!("Opus encoder init: {:?}", e))?;
        Ok(Self {
            pcm_buffer: VecDeque::new(),
            music_buffer: VecDeque::new(),
            music_gain: GainEnvelope::new(1.0, 1.0, 0),
            music_gain_notified: 1.0,
            encoder,
            speaking: false,
            trailing_silence_frames: 0,
        })
    }

    fn push_pcm(&mut self, samples: Vec<i16>) {
        self.pcm_buffer.extend(samples);
        // Drop oldest samples to keep the buffer bounded (prevents runaway latency)
        if self.pcm_buffer.len() > MAX_PCM_BUFFER_SAMPLES {
            let overflow = self.pcm_buffer.len() - MAX_PCM_BUFFER_SAMPLES;
            warn!(
                "TTS PCM buffer overflow: dropping {} oldest samples ({:.1}ms), buffer was {} samples ({:.1}ms)",
                overflow,
                overflow as f64 / 48.0,
                self.pcm_buffer.len(),
                self.pcm_buffer.len() as f64 / 48.0
            );
            self.pcm_buffer.drain(..overflow);
        }
        self.trailing_silence_frames = 0;
    }

    fn push_music_pcm(&mut self, samples: Vec<i16>) {
        self.music_buffer.extend(samples);
        self.trailing_silence_frames = 0;
    }

    fn set_music_gain(&mut self, target: f32, fade_ms: u32) {
        self.music_gain = GainEnvelope::new(self.music_gain.current, target, fade_ms);
    }

    fn tts_buffer_samples(&self) -> usize {
        self.pcm_buffer.len()
    }

    fn music_buffer_samples(&self) -> usize {
        self.music_buffer.len()
    }

    fn clear(&mut self) {
        self.pcm_buffer.clear();
        self.music_buffer.clear();
        self.trailing_silence_frames = MAX_TRAILING_SILENCE;
    }

    fn clear_tts(&mut self) {
        self.pcm_buffer.clear();
    }

    /// Encode the next 20ms frame, mixing TTS and music buffers.
    /// Music samples have the gain envelope applied. Returns None if idle.
    fn next_opus_frame(&mut self) -> Option<Vec<u8>> {
        const FRAME_SIZE: usize = 960; // 20ms @ 48kHz mono

        let has_tts = self.pcm_buffer.len() >= FRAME_SIZE;
        let has_music = self.music_buffer.len() >= FRAME_SIZE;

        if has_tts || has_music {
            let mut mixed = [0i32; FRAME_SIZE];

            if has_music {
                for (i, s) in self.music_buffer.drain(..FRAME_SIZE).enumerate() {
                    mixed[i] += self.music_gain.apply_sample(s) as i32;
                }
            }
            if has_tts {
                for (i, s) in self.pcm_buffer.drain(..FRAME_SIZE).enumerate() {
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
                    return Some(opus_buf[..len].to_vec());
                }
                Err(e) => {
                    error!("Opus encode error: {:?}", e);
                    return None;
                }
            }
        }

        // Buffer empty — send trailing silence to avoid abrupt cutoff
        if self.trailing_silence_frames < MAX_TRAILING_SILENCE {
            self.trailing_silence_frames += 1;
            // Opus silence frame (RFC 6716 comfort noise)
            return Some(vec![0xF8, 0xFF, 0xFE]);
        }

        if self.speaking {
            self.speaking = false;
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Music player (yt-dlp + ffmpeg → raw PCM)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum MusicEvent {
    Idle,
    Error(String),
    FirstPcm {
        startup_ms: u64,
        resolved_direct_url: bool,
    },
}

/// Tracks per-user speaking state driven by actual UDP audio packet arrival.
#[derive(Clone, Debug)]
struct SpeakingState {
    last_packet_at: Option<time::Instant>,
    is_speaking: bool,
}

const SPEAKING_TIMEOUT_MS: u64 = 100;

#[derive(Clone, Debug)]
struct UserCaptureState {
    sample_rate: u32,
    silence_duration_ms: u32,
    stream_active: bool,
    last_audio_at: Option<time::Instant>,
}

impl UserCaptureState {
    fn new(sample_rate: u32, silence_duration_ms: u32) -> Self {
        Self {
            sample_rate: normalize_sample_rate(sample_rate),
            silence_duration_ms: normalize_silence_duration_ms(silence_duration_ms),
            stream_active: false,
            last_audio_at: None,
        }
    }

    fn touch_audio(&mut self, now: time::Instant) {
        self.stream_active = true;
        self.last_audio_at = Some(now);
    }
}

fn normalize_sample_rate(sample_rate: u32) -> u32 {
    sample_rate.clamp(8_000, 48_000)
}

fn normalize_silence_duration_ms(duration_ms: u32) -> u32 {
    duration_ms.clamp(100, 5_000)
}

fn clear_audio_send_buffer(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.clear();
    }
}

fn clear_tts_send_buffer(audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    let mut guard = audio_send_state.lock();
    if let Some(ref mut state) = *guard {
        state.clear_tts();
    }
}

fn drain_music_pcm_queue(music_pcm_rx: &crossbeam::Receiver<Vec<i16>>) {
    while music_pcm_rx.try_recv().is_ok() {}
}

fn is_music_output_drained(
    music_pcm_rx: &crossbeam::Receiver<Vec<i16>>,
    audio_send_state: &Arc<Mutex<Option<AudioSendState>>>,
) -> bool {
    if !music_pcm_rx.is_empty() {
        return false;
    }

    let guard = audio_send_state.lock();
    guard
        .as_ref()
        .map_or(true, |state| state.music_buffer.is_empty())
}

fn emit_playback_armed(reason: &str, audio_send_state: &Arc<Mutex<Option<AudioSendState>>>) {
    if audio_send_state.lock().is_some() {
        send_msg(&OutMsg::PlaybackArmed {
            reason: reason.to_string(),
        });
    }
}

fn start_music_pipeline(
    url: &str,
    music_player: &mut Option<MusicPlayer>,
    music_pcm_rx: &crossbeam::Receiver<Vec<i16>>,
    music_pcm_tx: &crossbeam::Sender<Vec<i16>>,
    music_event_tx: &mpsc::Sender<MusicEvent>,
    audio_send_state: &Arc<Mutex<Option<AudioSendState>>>,
    resolved_direct_url: bool,
    clear_output_buffers: bool,
) {
    if let Some(ref mut player) = music_player {
        player.stop();
    }
    *music_player = None;
    drain_music_pcm_queue(music_pcm_rx);
    if clear_output_buffers {
        clear_audio_send_buffer(audio_send_state);
    }
    *music_player = Some(MusicPlayer::start(
        url,
        music_pcm_tx.clone(),
        music_event_tx.clone(),
        resolved_direct_url,
    ));
}

const MUSIC_PIPELINE_STDERR_TAIL_LINES: usize = 24;

struct MusicPlayer {
    stop: Arc<AtomicBool>,
    child_pid: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MusicPlayer {
    fn start(
        url: &str,
        pcm_tx: crossbeam::Sender<Vec<i16>>,
        music_event_tx: mpsc::Sender<MusicEvent>,
        resolved_direct_url: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let child_pid = Arc::new(AtomicU32::new(0));
        let child_pid_thread = child_pid.clone();
        let url = url.to_string();

        let thread = std::thread::spawn(move || {
            let pipeline_command = build_music_pipeline_command(&url, resolved_direct_url);
            let pipeline_started_at = time::Instant::now();
            let child = std::process::Command::new("sh")
                .args(["-c", &pipeline_command])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = music_event_tx.blocking_send(MusicEvent::Error(format!(
                        "yt-dlp/ffmpeg spawn failed: {}",
                        e
                    )));
                    return;
                }
            };
            child_pid_thread.store(child.id(), Ordering::SeqCst);

            let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
            let mut stderr_thread = child.stderr.take().map(|stderr| {
                let stderr_tail = stderr_tail.clone();
                std::thread::spawn(move || {
                    let reader = io::BufReader::new(stderr);
                    for line_result in reader.lines() {
                        let line = match line_result {
                            Ok(value) => value.trim().to_string(),
                            Err(_) => break,
                        };
                        if line.is_empty() {
                            continue;
                        }
                        let mut tail = stderr_tail.lock();
                        if tail.len() >= MUSIC_PIPELINE_STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })
            });

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    let _ = music_event_tx.blocking_send(MusicEvent::Error(
                        "music pipeline missing stdout".to_string(),
                    ));
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(handle) = stderr_thread.take() {
                        let _ = handle.join();
                    }
                    child_pid_thread.store(0, Ordering::SeqCst);
                    return;
                }
            };

            let mut reader = io::BufReader::with_capacity(48000 * 2, stdout); // 0.5s buffer
            let mut chunk = vec![0u8; 960 * 2]; // 20ms of mono i16
            let mut first_pcm_reported = false;

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match io::Read::read_exact(&mut reader, &mut chunk) {
                    Ok(()) => {
                        if !first_pcm_reported {
                            first_pcm_reported = true;
                            let startup_ms = pipeline_started_at.elapsed().as_millis() as u64;
                            info!(
                                "music pipeline first pcm startup_ms={} direct={}",
                                startup_ms, resolved_direct_url
                            );
                            let _ = music_event_tx.blocking_send(MusicEvent::FirstPcm {
                                startup_ms,
                                resolved_direct_url,
                            });
                        }
                        let mut samples = Vec::with_capacity(960);
                        for i in 0..960 {
                            samples.push(i16::from_le_bytes([chunk[i * 2], chunk[i * 2 + 1]]));
                        }
                        if pcm_tx.send(samples).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            let _ = child.kill();
            let wait_result = child.wait();
            if let Some(handle) = stderr_thread.take() {
                let _ = handle.join();
            }
            child_pid_thread.store(0, Ordering::SeqCst);

            let stderr_summary = {
                let tail = stderr_tail.lock();
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(
                        " | stderr tail: {}",
                        tail.iter().cloned().collect::<Vec<_>>().join(" || ")
                    )
                }
            };

            if !stop_clone.load(Ordering::Relaxed) {
                match wait_result {
                    Ok(status) if status.success() => {
                        let _ = music_event_tx.blocking_send(MusicEvent::Idle);
                    }
                    Ok(status) => {
                        let _ = music_event_tx.blocking_send(MusicEvent::Error(format!(
                            "music pipeline exited with status {}{}",
                            status, stderr_summary
                        )));
                    }
                    Err(error) => {
                        let _ = music_event_tx.blocking_send(MusicEvent::Error(format!(
                            "music pipeline wait failed: {}{}",
                            error, stderr_summary
                        )));
                    }
                }
            }
        });

        MusicPlayer {
            stop,
            child_pid,
            thread: Some(thread),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let pid = self.child_pid.load(Ordering::SeqCst);
        if pid > 0 {
            // Direct syscall instead of spawning a process
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        self.child_pid.store(0, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                // Avoid blocking the async runtime thread on a stuck join.
                std::thread::spawn(move || {
                    let _ = thread.join();
                });
            }
        }
    }
}

impl Drop for MusicPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_music_pipeline_command(url: &str, resolved_direct_url: bool) -> String {
    let quoted_url = url.replace('\'', "'\\''");
    if resolved_direct_url {
        format!(
            "ffmpeg -nostdin -loglevel error -i '{}' -f s16le -ar 48000 -ac 1 pipe:1",
            quoted_url
        )
    } else {
        format!(
            "yt-dlp --no-warnings --quiet --no-playlist --extractor-args 'youtube:player_client=android' -f bestaudio/best -o - '{}' | ffmpeg -nostdin -loglevel error -i pipe:0 -f s16le -ar 48000 -ac 1 pipe:1",
            quoted_url
        )
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,davey=warn,davey::cryptor::frame_processors=off",
                )
            }),
        )
        .with_writer(io::stderr)
        .init();

    let ipc_tx = spawn_ipc_writer();
    IPC_TX.set(ipc_tx).expect("IPC_TX already initialized");

    // Spawn stdin reader (blocking, off the tokio runtime)
    let (ipc_msg_tx, mut ipc_msg_rx) = mpsc::channel::<InMsg>(256);
    {
        let audio_debug = std::env::var("AUDIO_DEBUG").is_ok();
        std::thread::spawn(move || {
            let stdin = io::stdin();
            let mut handle = stdin.lock();
            // Cap line length to 1MB to prevent unbounded allocation on malformed input.
            const MAX_LINE_BYTES: usize = 1_024 * 1_024;
            let mut line_buf = String::new();
            loop {
                line_buf.clear();
                match handle.read_line(&mut line_buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if n > MAX_LINE_BYTES {
                            if audio_debug {
                                eprintln!(
                                    "[rust-subprocess] Dropping oversized stdin line ({} bytes)",
                                    n
                                );
                            }
                            continue;
                        }
                        let trimmed = line_buf.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<InMsg>(trimmed) {
                            Ok(msg) => {
                                if ipc_msg_tx.blocking_send(msg).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if audio_debug {
                                    eprintln!(
                                        "[rust-subprocess] JSON parse error: {} for line: {}",
                                        e,
                                        &trimmed[..trimmed.len().min(200)]
                                    );
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            // Stdin closed (pipe broken, likely parent died)
            let _ = ipc_msg_tx.blocking_send(InMsg::Destroy);
        });
    }

    info!("Voice subprocess started, waiting for IPC messages");

    // ---- State ----
    let mut pending_conn = PendingConnection::default();
    let mut guild_id: Option<u64> = None;
    let mut channel_id: Option<u64> = None;
    let mut _self_mute = false;

    let dave: Arc<Mutex<Option<DaveManager>>> = Arc::new(Mutex::new(None));
    let mut voice_conn: Option<VoiceConnection> = None;
    let (voice_event_tx, mut voice_event_rx) = mpsc::channel::<VoiceEvent>(256);

    // Audio send pipeline: IPC audio → PCM buffer → 20ms Opus encode → DAVE encrypt → RTP
    let audio_send_state = Arc::new(Mutex::new(None::<AudioSendState>));
    let mut send_interval = time::interval(Duration::from_millis(20));
    send_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    send_interval.tick().await; // consume first immediate tick

    // Music → PCM channel (feeds into audio_send_state)
    let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(500);
    let (music_event_tx, mut music_event_rx) = mpsc::channel::<MusicEvent>(32);
    let mut music_player: Option<MusicPlayer> = None;
    let mut music_active = false;
    let mut music_paused = false;
    let mut active_music_url: Option<String> = None;
    let mut active_music_resolved_direct_url = false;
    let mut pending_music_url: Option<String> = None;
    let mut pending_music_received_at: Option<time::Instant> = None;
    let mut pending_music_audio_seen = false;
    let mut pending_music_last_audio_at: Option<time::Instant> = None;
    let mut pending_music_waiting_for_drain = false;
    let mut pending_music_drain_started_at: Option<time::Instant> = None;
    let mut pending_music_first_pcm_at: Option<time::Instant> = None;
    let mut pending_music_resolved_direct_url = false;
    let mut pending_music_stop = false; // Set when fade-out starts before actual stop
    let mut music_finishing = false;

    // Opus decoder for inbound audio (stereo 48kHz)
    let mut opus_decoder =
        OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo).expect("Opus decoder init");

    // SSRC → user_id mapping (mirrored from voice_conn's Speaking events)
    let mut ssrc_map: HashMap<u32, u64> = HashMap::new();
    let mut self_user_id: Option<u64> = None;
    let mut default_recv_sample_rate = default_sample_rate();
    let mut default_silence_duration_ms = default_silence_duration();
    let mut user_capture_states: HashMap<u64, UserCaptureState> = HashMap::new();
    let mut asr_txs: HashMap<u64, mpsc::UnboundedSender<AsrCommand>> = HashMap::new();
    let (asr_exit_tx, mut asr_exit_rx) = mpsc::channel::<(u64, String)>(32);
    let mut speaking_states: HashMap<u64, SpeakingState> = HashMap::new();

    // Buffer depth reporting: emit every 25 ticks (500ms) while buffered, and
    // emit one final zero-depth update when playback drains so the main process
    // can clear its backlog state.
    let mut buffer_depth_tick_counter: u32 = 0;
    let mut buffer_depth_was_nonempty = false;
    let mut tts_playback_buffered = false;
    const BUFFER_DEPTH_REPORT_INTERVAL: u32 = 25; // 25 × 20ms = 500ms

    loop {
        tokio::select! {
            // ---- IPC message from main process ----
            Some(msg) = ipc_msg_rx.recv() => {
                match msg {
                    InMsg::Join { guild_id: gid, channel_id: cid, self_deaf: _, self_mute: sm } => {
                        let g: u64 = match gid.parse() {
                            Ok(v) => v,
                            Err(_) => { send_error("Invalid guild ID"); continue; }
                        };
                        let c: u64 = match cid.parse() {
                            Ok(v) => v,
                            Err(_) => { send_error("Invalid channel ID"); continue; }
                        };
                        guild_id = Some(g);
                        channel_id = Some(c);
                        _self_mute = sm;

                        // Send OP4 via the gateway adapter proxy
                        send_msg(&OutMsg::AdapterSend {
                            payload: serde_json::json!({
                                "op": 4,
                                "d": {
                                    "guild_id": gid,
                                    "channel_id": cid,
                                    "self_mute": sm,
                                    "self_deaf": false,
                                }
                            }),
                        });
                        info!("Join requested guild={} channel={}, sent OP4", g, c);
                    }

                    InMsg::VoiceServer { data } => {
                        let ep = data.get("endpoint").and_then(|v| v.as_str()).map(String::from);
                        let has_token = data.get("token").and_then(|v| v.as_str()).is_some();
                        info!(
                            "IPC voice_server: endpoint={:?} token={} connected={}",
                            ep, if has_token { "present" } else { "missing" }, voice_conn.is_some()
                        );
                        if let Some(ref e) = ep {
                            pending_conn.endpoint = Some(e.clone());
                        }
                        if let Some(tk) = data.get("token").and_then(|v| v.as_str()) {
                            pending_conn.token = Some(tk.to_string());
                        }
                        try_connect(
                            &pending_conn, guild_id, channel_id, self_user_id,
                            &voice_event_tx, &dave, &mut voice_conn, &audio_send_state,
                        ).await;
                    }

                    InMsg::VoiceState { data } => {
                        let new_sid = data.get("session_id").and_then(|v| v.as_str()).map(String::from);
                        let old_sid = pending_conn.session_id.clone();
                        let new_uid = data.get("user_id").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok());
                        let new_channel = data.get("channel_id").and_then(|v| v.as_str()).map(String::from);
                        info!(
                            "IPC voice_state: session_id={:?} prev_session_id={:?} channel_id={:?} user_id={:?} connected={}",
                            new_sid, old_sid, new_channel, new_uid, voice_conn.is_some()
                        );

                        if let Some(ref sid) = new_sid {
                            // Detect session credential refresh while already connected
                            if voice_conn.is_some() && old_sid.as_deref() != Some(sid.as_str()) {
                                warn!(
                                    "Session ID changed while connected: {:?} -> {:?}, tearing down for reconnect",
                                    old_sid, new_sid
                                );
                                if let Some(ref conn) = voice_conn {
                                    conn.shutdown();
                                }
                                voice_conn = None;
                                *audio_send_state.lock() = None;
                            }
                            pending_conn.session_id = Some(sid.clone());
                        }
                        if let Some(uid) = new_uid {
                            pending_conn.user_id = Some(uid);
                            self_user_id = Some(uid);
                        }
                        try_connect(
                            &pending_conn, guild_id, channel_id, self_user_id,
                            &voice_event_tx, &dave, &mut voice_conn, &audio_send_state,
                        ).await;
                    }

                    InMsg::Audio { pcm_base64, sample_rate } => {
                        let now = time::Instant::now();
                        if pending_music_url.is_some() {
                            pending_music_audio_seen = true;
                            pending_music_last_audio_at = Some(now);
                        }

                        // When music is playing at full volume (not ducked), drop TTS.
                        // When music is ducked (gain < 1.0), allow TTS through for mixing.
                        if music_active && !music_paused {
                            let is_ducked = {
                                let guard = audio_send_state.lock();
                                guard.as_ref().map_or(false, |s| s.music_gain.target < 1.0)
                            };
                            if !is_ducked {
                                continue;
                            }
                        }

                        let engine = base64::engine::general_purpose::STANDARD;
                        if let Ok(raw) = engine.decode(&pcm_base64) {
                            let samples = convert_llm_to_48k_mono(&raw, sample_rate);
                            if !samples.is_empty() {
                                let mut emit_tts_buffered = false;
                                {
                                    let mut guard = audio_send_state.lock();
                                    if let Some(ref mut state) = *guard {
                                        state.push_pcm(samples);
                                        if state.tts_buffer_samples() > 0 && !tts_playback_buffered {
                                            tts_playback_buffered = true;
                                            emit_tts_buffered = true;
                                        }
                                    }
                                }
                                if emit_tts_buffered {
                                    send_tts_playback_state("buffered");
                                }
                            }
                        }
                    }

                    InMsg::StopPlayback => {
                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        music_player = None;
                        music_active = false;
                        music_paused = false;
                        active_music_url = None;
                        active_music_resolved_direct_url = false;
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        drain_music_pcm_queue(&music_pcm_rx);
                        clear_audio_send_buffer(&audio_send_state);
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle");
                        }
                        send_msg(&OutMsg::PlayerState {
                            status: "idle".into(),
                        });
                        emit_playback_armed("stop_playback", &audio_send_state);
                    }

                    InMsg::StopTtsPlayback => {
                        clear_tts_send_buffer(&audio_send_state);
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle");
                        }
                    }

                    InMsg::SubscribeUser {
                        user_id,
                        silence_duration_ms,
                        sample_rate,
                    } => {
                        default_recv_sample_rate = normalize_sample_rate(sample_rate);
                        default_silence_duration_ms = normalize_silence_duration_ms(silence_duration_ms);
                        if let Ok(uid) = user_id.parse::<u64>() {
                            let state = user_capture_states.entry(uid).or_insert_with(|| {
                                UserCaptureState::new(
                                    default_recv_sample_rate,
                                    default_silence_duration_ms,
                                )
                            });
                            state.sample_rate = default_recv_sample_rate;
                            state.silence_duration_ms = default_silence_duration_ms;
                        }
                    }

                    InMsg::UnsubscribeUser { user_id } => {
                        if let Ok(uid) = user_id.parse::<u64>() {
                            if let Some(state) = user_capture_states.remove(&uid) {
                                if state.stream_active {
                                    send_msg(&OutMsg::UserAudioEnd {
                                        user_id: uid.to_string(),
                                    });
                                }
                            }
                        }
                    }

                    InMsg::MusicPlay { url, resolved_direct_url } => {
                        let normalized_url = url.trim().to_string();
                        if normalized_url.is_empty() {
                            send_msg(&OutMsg::MusicError {
                                message: "music_play missing url".to_string(),
                            });
                            continue;
                        }

                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        music_player = None;
                        music_active = false;
                        music_paused = false;
                        active_music_url = Some(normalized_url.clone());
                        active_music_resolved_direct_url = resolved_direct_url;
                        pending_music_url = Some(normalized_url.clone());
                        pending_music_received_at = Some(time::Instant::now());
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = resolved_direct_url;
                        music_finishing = false;
                        start_music_pipeline(
                            &normalized_url,
                            &mut music_player,
                            &music_pcm_rx,
                            &music_pcm_tx,
                            &music_event_tx,
                            &audio_send_state,
                            resolved_direct_url,
                            false,
                        );
                        info!(
                            "music_play queued pending start url={} direct={} (waiting for announcement drain)",
                            normalized_url,
                            resolved_direct_url
                        );
                    }

                    InMsg::MusicStop => {
                        if music_player.is_some() && music_active {
                            // Start a quick fade-out; actual stop happens in the
                            // 20ms tick when the fade completes.
                            let mut guard = audio_send_state.lock();
                            if let Some(ref mut state) = *guard {
                                state.set_music_gain(0.0, 300);
                            }
                            pending_music_stop = true;
                        } else {
                            // No active music, stop immediately
                            if let Some(ref mut mp) = music_player {
                                mp.stop();
                            }
                            music_player = None;
                            music_active = false;
                            music_paused = false;
                            active_music_url = None;
                            active_music_resolved_direct_url = false;
                            pending_music_url = None;
                            pending_music_received_at = None;
                            pending_music_audio_seen = false;
                            pending_music_last_audio_at = None;
                            pending_music_waiting_for_drain = false;
                            pending_music_drain_started_at = None;
                            pending_music_first_pcm_at = None;
                            pending_music_resolved_direct_url = false;
                            music_finishing = false;
                            drain_music_pcm_queue(&music_pcm_rx);
                            clear_audio_send_buffer(&audio_send_state);
                            if tts_playback_buffered {
                                tts_playback_buffered = false;
                                send_tts_playback_state("idle");
                            }
                            send_msg(&OutMsg::PlayerState {
                                status: "idle".into(),
                            });
                            send_msg(&OutMsg::MusicIdle);
                            emit_playback_armed("music_stop", &audio_send_state);
                        }
                    }

                    InMsg::MusicPause => {
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        let was_finishing = music_finishing;
                        music_finishing = false;
                        if music_player.is_some() || music_active || was_finishing {
                            music_paused = true;
                            music_active = false;
                            clear_audio_send_buffer(&audio_send_state);
                            send_msg(&OutMsg::PlayerState {
                                status: "paused".into(),
                            });
                            emit_playback_armed("music_pause", &audio_send_state);
                        }
                    }

                    InMsg::MusicResume => {
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        music_finishing = false;

                        if music_player.is_some() {
                            music_paused = false;
                            music_active = true;
                            send_msg(&OutMsg::PlayerState {
                                status: "playing".into(),
                            });
                        } else if let Some(url) = active_music_url.clone() {
                            start_music_pipeline(
                                &url,
                                &mut music_player,
                                &music_pcm_rx,
                                &music_pcm_tx,
                                &music_event_tx,
                                &audio_send_state,
                                active_music_resolved_direct_url,
                                true,
                            );
                            music_paused = false;
                            music_active = true;
                            send_msg(&OutMsg::PlayerState {
                                status: "playing".into(),
                            });
                        }
                    }

                    InMsg::MusicSetGain { target, fade_ms } => {
                        let clamped = target.clamp(0.0, 1.0);
                        let mut guard = audio_send_state.lock();
                        if let Some(ref mut state) = *guard {
                            state.set_music_gain(clamped, fade_ms);
                            if fade_ms == 0 {
                                state.music_gain_notified = clamped;
                                drop(guard);
                                send_msg(&OutMsg::MusicGainReached { gain: clamped });
                            }
                        }
                    }

                    InMsg::ConnectAsr { user_id, api_key, model, language, prompt } => {
                        let uid = user_id.parse::<u64>().unwrap_or(0);
                        if uid != 0 {
                            // Drop any existing ASR channel for this user before creating a new one
                            asr_txs.remove(&uid);

                            let (asr_tx, asr_rx) = tokio::sync::mpsc::unbounded_channel();
                            asr_txs.insert(uid, asr_tx);

                            let exit_tx = asr_exit_tx.clone();
                            tokio::spawn(async move {
                                let reason = match run_asr_client(user_id.clone(), api_key, model, language, prompt, asr_rx).await {
                                    Ok(()) => "closed".to_string(),
                                    Err(e) => {
                                        error!("ASR client {} exited: {}", user_id, e);
                                        format!("{}", e)
                                    }
                                };
                                let _ = exit_tx.send((uid, reason)).await;
                            });
                        }
                    }

                    InMsg::DisconnectAsr { user_id } => {
                        let uid = user_id.parse::<u64>().unwrap_or(0);
                        asr_txs.remove(&uid);
                    }

                    InMsg::CommitAsr { user_id } => {
                        let uid = user_id.parse::<u64>().unwrap_or(0);
                        if let Some(tx) = asr_txs.get(&uid) {
                            let _ = tx.send(AsrCommand::Commit);
                        }
                    }

                    InMsg::ClearAsr { user_id } => {
                        let uid = user_id.parse::<u64>().unwrap_or(0);
                        if let Some(tx) = asr_txs.get(&uid) {
                            let _ = tx.send(AsrCommand::Clear);
                        }
                    }

                    InMsg::Destroy => {
                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        if let Some(ref conn) = voice_conn {
                            conn.shutdown();
                        }
                        break;
                    }
                }
            }

            // ---- Voice connection events ----
            Some(event) = voice_event_rx.recv() => {
                match event {
                    VoiceEvent::Ready { ssrc } => {
                        info!("Voice connection ready, ssrc={}", ssrc);
                        send_msg(&OutMsg::ConnectionState { status: "ready".into() });
                        send_msg(&OutMsg::Ready);

                        // Initialize the audio send pipeline
                        match AudioSendState::new() {
                            Ok(state) => {
                                *audio_send_state.lock() = Some(state);
                                send_msg(&OutMsg::PlaybackArmed { reason: "connection_ready".into() });
                            }
                            Err(e) => error!("Failed to init audio send state: {}", e),
                        }
                    }

                    VoiceEvent::SsrcUpdate { ssrc, user_id } => {
                        ssrc_map.insert(ssrc, user_id);
                    }

                    VoiceEvent::ClientDisconnect { user_id } => {
                        if self_user_id == Some(user_id) {
                            continue;
                        }
                        // Remove from SSRC map
                        ssrc_map.retain(|_, v| *v != user_id);

                        let uid_str = user_id.to_string();

                        // End speaking if active
                        if let Some(ss) = speaking_states.remove(&user_id) {
                            if ss.is_speaking {
                                send_msg(&OutMsg::SpeakingEnd { user_id: uid_str.clone() });
                            }
                        }

                        // End user audio capture if active
                        if let Some(state) = user_capture_states.remove(&user_id) {
                            if state.stream_active {
                                send_msg(&OutMsg::UserAudioEnd { user_id: uid_str.clone() });
                            }
                        }

                        // Close ASR
                        asr_txs.remove(&user_id);

                        send_msg(&OutMsg::ClientDisconnect { user_id: uid_str });
                    }

                    VoiceEvent::OpusReceived { ssrc, opus_frame } => {
                        let uid = match ssrc_map.get(&ssrc) {
                            Some(&uid) => uid,
                            None => {
                                debug!("Dropped Opus frame from unknown ssrc: {}", ssrc);
                                continue;
                            }
                        };
                        if self_user_id == Some(uid) {
                            continue; // skip bot's own audio
                        }

                        let target_sample_rate = user_capture_states
                            .entry(uid)
                            .or_insert_with(|| {
                                UserCaptureState::new(
                                    default_recv_sample_rate,
                                    default_silence_duration_ms,
                                )
                            })
                            .sample_rate;

                        // Opus decode → stereo i16 48kHz
                        let mut pcm_stereo = vec![0i16; 5760]; // max Opus frame
                        let decode_result = {
                            let packet = OpusPacket::try_from(opus_frame.as_slice()).ok();
                            let signals = MutSignals::try_from(pcm_stereo.as_mut_slice())
                                .expect("non-empty signal buffer");
                            opus_decoder.decode(packet, signals, false)
                        };
                        match decode_result {
                            Ok(samples_per_channel) => {
                                let total_samples = samples_per_channel * 2; // stereo
                                let decoded = &pcm_stereo[..total_samples];

                                // Audio-driven speaking detection — only after
                                // successful Opus decode to avoid phantom starts
                                // from DAVE decrypt artifacts / invalid frames.
                                let now = time::Instant::now();
                                let ss = speaking_states.entry(uid).or_insert(SpeakingState {
                                    last_packet_at: None,
                                    is_speaking: false,
                                });
                                ss.last_packet_at = Some(now);
                                if !ss.is_speaking {
                                    ss.is_speaking = true;
                                    send_msg(&OutMsg::SpeakingStart { user_id: uid.to_string() });
                                }

                                let (llm_pcm, peak, active, total) = convert_decoded_to_llm(decoded, target_sample_rate);
                                if !llm_pcm.is_empty() {
                                    // Send to ASR first (clone only when ASR is active)
                                    if let Some(asr_tx) = asr_txs.get(&uid) {
                                        let _ = asr_tx.send(AsrCommand::Audio(llm_pcm.clone()));
                                    }

                                    send_msg(&OutMsg::UserAudio {
                                        user_id: uid.to_string(),
                                        pcm: llm_pcm,
                                        signal_peak_abs: peak,
                                        signal_active_sample_count: active,
                                        signal_sample_count: total,
                                    });

                                    if let Some(state) = user_capture_states.get_mut(&uid) {
                                        state.touch_audio(time::Instant::now());
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("Opus decode error for ssrc={}: {:?}", ssrc, e);
                            }
                        }
                    }

                    VoiceEvent::DaveReady => {
                        info!("DAVE E2EE session is ready");
                    }

                    VoiceEvent::Disconnected { reason } => {
                        warn!("Voice disconnected: {}", reason);
                        send_msg(&OutMsg::ConnectionState { status: "disconnected".into() });
                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        music_player = None;
                        music_active = false;
                        music_paused = false;
                        active_music_url = None;
                        active_music_resolved_direct_url = false;
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        drain_music_pcm_queue(&music_pcm_rx);
                        voice_conn = None;
                        *audio_send_state.lock() = None;
                        speaking_states.clear();
                    }

                    VoiceEvent::Error { message } => {
                        error!("Voice connection error: {}", message);
                        send_msg(&OutMsg::Error { message });
                    }
                }
            }

            // ---- Music pipeline events (from worker thread) ----
            Some(event) = music_event_rx.recv() => {
                match event {
                    MusicEvent::Idle => {
                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        music_player = None;
                        music_paused = false;
                        music_finishing = music_active;
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        if !music_finishing {
                            active_music_url = None;
                            active_music_resolved_direct_url = false;
                            send_msg(&OutMsg::PlayerState {
                                status: "idle".into(),
                            });
                            send_msg(&OutMsg::MusicIdle);
                            emit_playback_armed("music_idle", &audio_send_state);
                        }
                    }
                    MusicEvent::Error(message) => {
                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        music_player = None;
                        music_active = false;
                        music_paused = false;
                        music_finishing = false;
                        active_music_url = None;
                        active_music_resolved_direct_url = false;
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        drain_music_pcm_queue(&music_pcm_rx);
                        send_msg(&OutMsg::MusicError { message });
                        send_msg(&OutMsg::PlayerState {
                            status: "idle".into(),
                        });
                        emit_playback_armed("music_error", &audio_send_state);
                    }
                    MusicEvent::FirstPcm {
                        startup_ms,
                        resolved_direct_url,
                    } => {
                        let now = time::Instant::now();
                        pending_music_first_pcm_at = Some(now);
                        if let Some(received_at) = pending_music_received_at {
                            info!(
                                "music_play prepared url={} direct={} startupMs={} requestToFirstPcmMs={}",
                                pending_music_url.as_deref().unwrap_or("unknown"),
                                resolved_direct_url,
                                startup_ms,
                                now.duration_since(received_at).as_millis() as u64
                            );
                        } else {
                            info!(
                                "music_play prepared url={} direct={} startupMs={}",
                                pending_music_url.as_deref().unwrap_or("unknown"),
                                resolved_direct_url,
                                startup_ms
                            );
                        }
                    }
                }
            }

            // ---- ASR task exit notification ----
            Some((uid, reason)) = asr_exit_rx.recv() => {
                // Clean up stale sender — only if it's still the same sender
                // (a new ConnectAsr may have already replaced it)
                if let Some(tx) = asr_txs.get(&uid) {
                    if tx.is_closed() {
                        asr_txs.remove(&uid);
                    }
                }
                send_msg(&OutMsg::AsrDisconnected {
                    user_id: uid.to_string(),
                    reason,
                });
            }

            // ---- 20ms audio send tick ----
            _ = send_interval.tick() => {
                let now = time::Instant::now();

                // Audio-driven speaking timeout: emit SpeakingEnd after SPEAKING_TIMEOUT_MS
                // of no UDP audio packets from a user.
                let mut speaking_ended_users: Vec<u64> = Vec::new();
                for (&user_id, ss) in speaking_states.iter_mut() {
                    if !ss.is_speaking {
                        continue;
                    }
                    if let Some(last_at) = ss.last_packet_at {
                        let silent_ms = now.duration_since(last_at).as_millis() as u64;
                        if silent_ms >= SPEAKING_TIMEOUT_MS {
                            ss.is_speaking = false;
                            speaking_ended_users.push(user_id);
                        }
                    }
                }
                for user_id in speaking_ended_users {
                    send_msg(&OutMsg::SpeakingEnd { user_id: user_id.to_string() });
                }

                // Emit user_audio_end when we've had enough silence after
                // speaking/audio updates. This mirrors Node AfterSilence-driven
                // stream end semantics used by the main-process turn logic.
                for (user_id, state) in user_capture_states.iter_mut() {
                    if !state.stream_active {
                        continue;
                    }
                    let Some(last_audio_at) = state.last_audio_at else {
                        state.last_audio_at = Some(now);
                        continue;
                    };
                    let silent_for_ms =
                        now.duration_since(last_audio_at).as_millis() as u64;
                    if silent_for_ms >= u64::from(state.silence_duration_ms) {
                        state.stream_active = false;
                        state.last_audio_at = None;
                        send_msg(&OutMsg::UserAudioEnd {
                            user_id: user_id.to_string(),
                        });
                    }
                }

                // Deferred music start: allow announcement audio to complete
                // before we commit the music pipeline.
                if let Some(url) = pending_music_url.clone() {
                    let mut start_music = false;
                    let mut reason = "pending_unknown";
                    if let Some(received_at) = pending_music_received_at {
                        let elapsed_ms = now.duration_since(received_at).as_millis() as u64;
                        if elapsed_ms > 15_000 {
                            start_music = true;
                            reason = "pending_safety_timeout";
                        } else if !pending_music_audio_seen {
                            if elapsed_ms > 5_000 {
                                start_music = true;
                                reason = "pending_no_announcement_audio";
                            }
                        } else {
                            let last_audio_at = pending_music_last_audio_at.unwrap_or(received_at);
                            let gap_ms = now.duration_since(last_audio_at).as_millis() as u64;
                            if !pending_music_waiting_for_drain && gap_ms > 500 {
                                pending_music_waiting_for_drain = true;
                                pending_music_drain_started_at = Some(now);
                            }
                            if pending_music_waiting_for_drain {
                                let audio_buffer_empty = {
                                    let guard = audio_send_state.lock();
                                    guard
                                        .as_ref()
                                        .map_or(true, |state| state.pcm_buffer.is_empty())
                                };
                                let drain_elapsed_ms = pending_music_drain_started_at
                                    .map(|started| now.duration_since(started).as_millis() as u64)
                                    .unwrap_or(0);
                                if audio_buffer_empty {
                                    start_music = true;
                                    reason = "pending_announcement_drain_complete";
                                } else if drain_elapsed_ms > 5_000 {
                                    start_music = true;
                                    reason = "pending_drain_timeout";
                                }
                            }
                        }
                    } else {
                        start_music = true;
                        reason = "pending_missing_timestamp";
                    }

                    if start_music {
                        let total_wait_ms = pending_music_received_at
                            .map(|received_at| now.duration_since(received_at).as_millis() as u64)
                            .unwrap_or(0);
                        let prepared_lead_ms = pending_music_first_pcm_at
                            .map(|first_pcm_at| now.duration_since(first_pcm_at).as_millis() as u64)
                            .unwrap_or(0);
                        let committed_direct_url = pending_music_resolved_direct_url;
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        music_finishing = false;
                        active_music_url = Some(url.clone());
                        active_music_resolved_direct_url = committed_direct_url;
                        music_active = true;
                        music_paused = false;

                        // Fade in over 1.5s
                        {
                            let mut guard = audio_send_state.lock();
                            if let Some(ref mut state) = *guard {
                                state.music_gain = GainEnvelope::new(0.0, 1.0, 1500);
                                state.music_gain_notified = 0.0;
                            }
                        }

                        send_msg(&OutMsg::PlayerState {
                            status: "playing".into(),
                        });
                        info!(
                            "music_play committed url={} reason={} totalWaitMs={} preparedLeadMs={} direct={}",
                            url,
                            reason,
                            total_wait_ms,
                            prepared_lead_ms,
                            committed_direct_url
                        );
                    }
                }

                // Drain music PCM into the dedicated music buffer only while active.
                if music_active && !music_paused {
                    while let Ok(chunk) = music_pcm_rx.try_recv() {
                        let mut guard = audio_send_state.lock();
                        if let Some(ref mut state) = *guard {
                            state.push_music_pcm(chunk);
                        }
                    }
                }

                if music_finishing && is_music_output_drained(&music_pcm_rx, &audio_send_state) {
                    music_finishing = false;
                    music_active = false;
                    music_paused = false;
                    active_music_url = None;
                    send_msg(&OutMsg::PlayerState {
                        status: "idle".into(),
                    });
                    send_msg(&OutMsg::MusicIdle);
                    emit_playback_armed("music_idle", &audio_send_state);
                }

                // Check if a gain fade completed; emit event and handle pending stop.
                {
                    let mut guard = audio_send_state.lock();
                    if let Some(ref mut state) = *guard {
                        if state.music_gain.is_complete()
                            && (state.music_gain_notified - state.music_gain.target).abs() > 0.0001
                        {
                            let reached = state.music_gain.target;
                            state.music_gain_notified = reached;
                            drop(guard);
                            send_msg(&OutMsg::MusicGainReached { gain: reached });
                        }
                    }
                }

                // Deferred music stop: execute after fade-out completes.
                if pending_music_stop {
                    let fade_done = {
                        let guard = audio_send_state.lock();
                        guard.as_ref().map_or(true, |s| {
                            s.music_gain.is_complete() && s.music_gain.target < 0.001
                        })
                    };
                    if fade_done {
                        pending_music_stop = false;
                        if let Some(ref mut mp) = music_player {
                            mp.stop();
                        }
                        music_player = None;
                        music_active = false;
                        music_paused = false;
                        music_finishing = false;
                        active_music_url = None;
                        active_music_resolved_direct_url = false;
                        pending_music_url = None;
                        pending_music_received_at = None;
                        pending_music_audio_seen = false;
                        pending_music_last_audio_at = None;
                        pending_music_waiting_for_drain = false;
                        pending_music_drain_started_at = None;
                        pending_music_first_pcm_at = None;
                        pending_music_resolved_direct_url = false;
                        drain_music_pcm_queue(&music_pcm_rx);
                        clear_audio_send_buffer(&audio_send_state);
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle");
                        }
                        send_msg(&OutMsg::PlayerState {
                            status: "idle".into(),
                        });
                        send_msg(&OutMsg::MusicIdle);
                        emit_playback_armed("music_stop", &audio_send_state);
                    }
                }

                // Report buffer depth periodically (every 500ms while buffered),
                // plus a single zero-depth event on the transition to empty.
                buffer_depth_tick_counter += 1;
                if buffer_depth_tick_counter >= BUFFER_DEPTH_REPORT_INTERVAL {
                    buffer_depth_tick_counter = 0;
                    let guard = audio_send_state.lock();
                    if let Some(ref state) = *guard {
                        let tts = state.tts_buffer_samples();
                        let music = state.music_buffer_samples();
                        if tts > 0 || music > 0 {
                            buffer_depth_was_nonempty = true;
                            drop(guard);
                            send_msg(&OutMsg::BufferDepth {
                                tts_samples: tts,
                                music_samples: music,
                            });
                            if tts > 0 && !tts_playback_buffered {
                                tts_playback_buffered = true;
                                send_tts_playback_state("buffered");
                            }
                        } else if buffer_depth_was_nonempty {
                            buffer_depth_was_nonempty = false;
                            drop(guard);
                            send_msg(&OutMsg::BufferDepth {
                                tts_samples: 0,
                                music_samples: 0,
                            });
                            if tts_playback_buffered {
                                tts_playback_buffered = false;
                                send_tts_playback_state("idle");
                            }
                        }
                    } else if buffer_depth_was_nonempty {
                        buffer_depth_was_nonempty = false;
                        drop(guard);
                        send_msg(&OutMsg::BufferDepth {
                            tts_samples: 0,
                            music_samples: 0,
                        });
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle");
                        }
                    }
                }

                // Encode + encrypt + send one frame
                let opus_frame = {
                    let mut guard = audio_send_state.lock();
                    match *guard {
                        Some(ref mut state) => state.next_opus_frame(),
                        None => None,
                    }
                };

                if let Some(opus) = opus_frame {
                    // DAVE encrypt if session is active
                    let encrypted = {
                        let mut guard = dave.lock();
                        match *guard {
                            Some(ref mut dm) if dm.is_ready() => {
                                dm.encrypt_opus(&opus).unwrap_or_else(|e| {
                                    debug!("DAVE encrypt fallback: {}", e);
                                    opus.clone()
                                })
                            }
                            _ => opus,
                        }
                    };

                    if let Some(ref conn) = voice_conn {
                        if let Err(e) = conn.send_rtp_frame(&encrypted).await {
                            debug!("RTP send error: {}", e);
                        }
                    }
                }
            }
        }
    }

    info!("Shutting down");
}

/// Attempt to establish the voice connection once we have all required info.
#[allow(clippy::too_many_arguments)]
async fn try_connect(
    pending: &PendingConnection,
    guild_id: Option<u64>,
    channel_id: Option<u64>,
    _self_user_id: Option<u64>,
    event_tx: &mpsc::Sender<VoiceEvent>,
    dave: &Arc<Mutex<Option<DaveManager>>>,
    voice_conn: &mut Option<VoiceConnection>,
    _audio_send_state: &Arc<Mutex<Option<AudioSendState>>>,
) {
    if voice_conn.is_some() || !pending.is_complete() {
        return;
    }
    let gid = match guild_id {
        Some(g) => g,
        None => return,
    };
    let cid = match channel_id {
        Some(c) => c,
        None => return,
    };
    let uid = pending.user_id.unwrap();

    info!(
        "Connecting to voice: endpoint={:?} guild={} channel={} user={}",
        pending.endpoint, gid, cid, uid
    );

    match VoiceConnection::connect(
        pending.endpoint.as_ref().unwrap(),
        gid,
        uid,
        pending.session_id.as_ref().unwrap(),
        pending.token.as_ref().unwrap(),
        cid,
        event_tx.clone(),
        dave.clone(),
    )
    .await
    {
        Ok(conn) => {
            *voice_conn = Some(conn);
        }
        Err(e) => {
            error!("Voice connection failed: {}", e);
            send_msg(&OutMsg::Error {
                message: format!("Voice connect failed: {}", e),
            });
        }
    }
}

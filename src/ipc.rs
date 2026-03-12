use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel as crossbeam;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::video::VideoStreamDescriptor;

#[derive(Clone, Debug)]
struct IpcSenders {
    control_tx: crossbeam::Sender<OutMsg>,
    audio_tx: crossbeam::Sender<OutMsg>,
    video_tx: crossbeam::Sender<OutMsg>,
}

static IPC_TX: std::sync::OnceLock<IpcSenders> = std::sync::OnceLock::new();
static DROPPED_OUTBOUND_VIDEO_FRAMES: AtomicU64 = AtomicU64::new(0);
const MAX_STDIN_LINE_BYTES: usize = 1_024 * 1_024;

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum InMsg {
    Join {
        #[serde(rename = "guildId")]
        guild_id: String,
        #[serde(rename = "channelId")]
        channel_id: String,
        #[serde(rename = "selfDeaf", default)]
        _self_deaf: bool,
        #[serde(rename = "selfMute", default)]
        self_mute: bool,
    },
    VoiceServer {
        data: VoiceServerData,
    },
    VoiceState {
        data: VoiceStateData,
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
    SubscribeUserVideo {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(
            rename = "maxFramesPerSecond",
            default = "default_video_max_frames_per_second"
        )]
        max_frames_per_second: u32,
        #[serde(rename = "preferredQuality", default = "default_video_quality")]
        preferred_quality: u32,
        #[serde(rename = "preferredPixelCount")]
        preferred_pixel_count: Option<u32>,
        #[serde(rename = "preferredStreamType")]
        preferred_stream_type: Option<String>,
    },
    UnsubscribeUserVideo {
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
    Destroy,
}

pub fn default_sample_rate() -> u32 {
    24000
}

pub fn default_silence_duration() -> u32 {
    700
}

pub fn default_video_max_frames_per_second() -> u32 {
    2
}

pub fn default_video_quality() -> u32 {
    100
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidJson,
    InputTooLarge,
    VoiceConnectFailed,
    VoiceRuntimeError,
}

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum OutMsg {
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
    UserVideoState {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(rename = "audioSsrc", skip_serializing_if = "Option::is_none")]
        audio_ssrc: Option<u32>,
        #[serde(rename = "videoSsrc", skip_serializing_if = "Option::is_none")]
        video_ssrc: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        streams: Vec<VideoStreamDescriptor>,
    },
    UserVideoFrame {
        #[serde(rename = "userId")]
        user_id: String,
        ssrc: u32,
        codec: String,
        keyframe: bool,
        #[serde(rename = "frameBase64")]
        frame_base64: String,
        #[serde(rename = "rtpTimestamp")]
        rtp_timestamp: u32,
        #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
        stream_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rid: Option<String>,
    },
    UserVideoEnd {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ssrc: Option<u32>,
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
    Error {
        code: ErrorCode,
        message: String,
    },
    BufferDepth {
        #[serde(rename = "ttsSamples")]
        tts_samples: usize,
        #[serde(rename = "musicSamples")]
        music_samples: usize,
    },
}

#[derive(Deserialize, Debug, Clone)]
pub struct VoiceServerData {
    pub endpoint: Option<String>,
    pub token: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(clippy::struct_field_names)] // These mirror Discord gateway field names verbatim.
pub struct VoiceStateData {
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub channel_id: Option<String>,
}

fn is_lossy_inbound_msg(msg: &InMsg) -> bool {
    matches!(msg, InMsg::Audio { .. })
}

fn encode_user_audio_payload(
    user_id: &str,
    pcm: &[u8],
    signal_peak_abs: u16,
    signal_active_sample_count: usize,
    signal_sample_count: usize,
) -> Option<Vec<u8>> {
    let uid = match user_id.parse::<u64>() {
        Ok(uid) => uid,
        Err(err) => {
            warn!(user_id, error = %err, "dropping user audio IPC with non-numeric user id");
            return None;
        }
    };

    let Ok(active_sample_count) = u32::try_from(signal_active_sample_count) else {
        warn!(
            user_id,
            signal_active_sample_count,
            "dropping user audio IPC with oversized active sample count"
        );
        return None;
    };

    let Ok(sample_count) = u32::try_from(signal_sample_count) else {
        warn!(
            user_id,
            signal_sample_count, "dropping user audio IPC with oversized sample count"
        );
        return None;
    };

    let mut payload = Vec::with_capacity(8 + 2 + 4 + 4 + pcm.len());
    payload.extend_from_slice(&uid.to_le_bytes());
    payload.extend_from_slice(&signal_peak_abs.to_le_bytes());
    payload.extend_from_slice(&active_sample_count.to_le_bytes());
    payload.extend_from_slice(&sample_count.to_le_bytes());
    payload.extend_from_slice(pcm);
    Some(payload)
}

pub fn send_msg(msg: &OutMsg) {
    if let Some(tx) = IPC_TX.get() {
        match msg {
            OutMsg::UserAudio { .. } => {
                // Audio frames are lossy — drop on backpressure rather than blocking.
                if let Err(err) = tx.audio_tx.try_send(msg.clone()) {
                    if !matches!(err, crossbeam::TrySendError::Full(_)) {
                        error!("failed to send lossy audio IPC message: {}", err);
                    }
                }
            }
            OutMsg::UserVideoFrame { user_id, ssrc, .. } => match tx.video_tx.try_send(msg.clone())
            {
                Ok(()) => {
                    let dropped = DROPPED_OUTBOUND_VIDEO_FRAMES.swap(0, Ordering::Relaxed);
                    if dropped > 0 {
                        info!(
                            dropped_video_frames = dropped,
                            "clankvox_outbound_video_backpressure_recovered"
                        );
                    }
                }
                Err(crossbeam::TrySendError::Full(_)) => {
                    let dropped = DROPPED_OUTBOUND_VIDEO_FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
                    if dropped == 1 || dropped % 100 == 0 {
                        warn!(
                            user_id,
                            ssrc,
                            dropped_video_frames = dropped,
                            "dropping outbound clankvox video IPC due to backpressure"
                        );
                    }
                }
                Err(crossbeam::TrySendError::Disconnected(_)) => {
                    error!("failed to send lossy video IPC message: channel disconnected");
                }
            },
            _ => {
                // Control messages must not block real-time async tasks. Route them
                // through an unbounded control lane handled by the writer thread.
                if let Err(err) = tx.control_tx.send(msg.clone()) {
                    error!("failed to send control IPC message: {}", err);
                }
            }
        }
    }
}

pub fn send_error(code: ErrorCode, message: impl Into<String>) {
    send_msg(&OutMsg::Error {
        code,
        message: message.into(),
    });
}

pub fn try_send_error(code: ErrorCode, message: impl Into<String>) {
    if let Some(tx) = IPC_TX.get() {
        let msg = OutMsg::Error {
            code,
            message: message.into(),
        };
        if let Err(err) = tx.control_tx.send(msg) {
            error!("failed to enqueue non-blocking IPC error: {}", err);
        }
    }
}

pub fn send_tts_playback_state(status: &str, reason: &str) {
    info!(
        status = status,
        reason = reason,
        "clankvox_tts_playback_state"
    );
    send_msg(&OutMsg::TtsPlaybackState {
        status: status.to_string(),
    });
}

pub fn send_buffer_depth(tts_samples: usize, music_samples: usize, reason: &str) {
    if matches!(reason, "periodic_nonempty" | "periodic_drained") {
        debug!(
            tts_samples = tts_samples,
            music_samples = music_samples,
            reason = reason,
            "clankvox_buffer_depth"
        );
    } else {
        info!(
            tts_samples = tts_samples,
            music_samples = music_samples,
            reason = reason,
            "clankvox_buffer_depth"
        );
    }
    send_msg(&OutMsg::BufferDepth {
        tts_samples,
        music_samples,
    });
}

pub fn send_gateway_voice_state_update(guild_id: u64, channel_id: u64, self_mute: bool) {
    send_msg(&OutMsg::AdapterSend {
        payload: serde_json::json!({
            "op": 4,
            "d": {
                "guild_id": guild_id.to_string(),
                "channel_id": channel_id.to_string(),
                "self_mute": self_mute,
                "self_deaf": false,
            }
        }),
    });
}

pub struct InboundIpc {
    control_rx: mpsc::UnboundedReceiver<InMsg>,
    audio_rx: mpsc::Receiver<InMsg>,
}

impl InboundIpc {
    pub async fn recv(&mut self) -> Option<InMsg> {
        tokio::select! {
            biased;
            Some(msg) = self.control_rx.recv() => Some(msg),
            Some(msg) = self.audio_rx.recv() => Some(msg),
            else => None,
        }
    }
}

pub fn spawn_ipc_reader(audio_debug: bool) -> InboundIpc {
    let (control_tx, control_rx) = mpsc::unbounded_channel::<InMsg>();
    let (audio_tx, audio_rx) = mpsc::channel::<InMsg>(256);

    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut line_buf = String::new();
        let mut dropped_audio_messages: u64 = 0;

        loop {
            line_buf.clear();
            match handle.read_line(&mut line_buf) {
                Ok(0) => break,
                Ok(n) => {
                    if n > MAX_STDIN_LINE_BYTES {
                        if audio_debug {
                            eprintln!(
                                "[rust-subprocess] Dropping oversized stdin line ({n} bytes)"
                            );
                        }
                        try_send_error(
                            ErrorCode::InputTooLarge,
                            format!("Dropped oversized stdin line ({n} bytes)"),
                        );
                        continue;
                    }

                    let trimmed = line_buf.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    let msg = match serde_json::from_str::<InMsg>(trimmed) {
                        Ok(msg) => msg,
                        Err(err) => {
                            if audio_debug {
                                eprintln!(
                                    "[rust-subprocess] JSON parse error: {} for line: {}",
                                    err,
                                    &trimmed[..trimmed.len().min(200)]
                                );
                            }
                            try_send_error(
                                ErrorCode::InvalidJson,
                                format!("Invalid stdin JSON message: {err}"),
                            );
                            continue;
                        }
                    };

                    if is_lossy_inbound_msg(&msg) {
                        match audio_tx.try_send(msg) {
                            Ok(()) => {
                                if dropped_audio_messages > 0 {
                                    info!(
                                        dropped_audio_messages = dropped_audio_messages,
                                        "clankvox_inbound_audio_backpressure_recovered"
                                    );
                                    dropped_audio_messages = 0;
                                }
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                dropped_audio_messages = dropped_audio_messages.saturating_add(1);
                                if dropped_audio_messages == 1 || dropped_audio_messages % 100 == 0
                                {
                                    warn!(
                                        dropped_audio_messages = dropped_audio_messages,
                                        "dropping inbound clankvox audio IPC due to backpressure"
                                    );
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    } else if control_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "stdin reader exiting after read error");
                    break;
                }
            }
        }

        let _ = control_tx.send(InMsg::Destroy);
    });

    InboundIpc {
        control_rx,
        audio_rx,
    }
}

pub fn spawn_ipc_writer() {
    if let Some(tx) = IPC_TX.get() {
        let _ = tx;
        return;
    }

    let (control_tx, control_rx) = crossbeam::unbounded::<OutMsg>();
    let (audio_tx, audio_rx) = crossbeam::bounded::<OutMsg>(512);
    let (video_tx, video_rx) = crossbeam::bounded::<OutMsg>(64);
    std::thread::spawn(move || {
        let mut out = io::stdout().lock();
        loop {
            let msg = match control_rx.try_recv() {
                Ok(msg) => msg,
                Err(crossbeam::TryRecvError::Empty) => match audio_rx.try_recv() {
                    Ok(msg) => msg,
                    Err(crossbeam::TryRecvError::Empty) => {
                        crossbeam::select! {
                            recv(control_rx) -> msg => match msg {
                                Ok(msg) => msg,
                                Err(_) => break,
                            },
                            recv(audio_rx) -> msg => match msg {
                                Ok(msg) => msg,
                                Err(_) => break,
                            },
                            recv(video_rx) -> msg => match msg {
                                Ok(msg) => msg,
                                Err(_) => break,
                            },
                        }
                    }
                    Err(crossbeam::TryRecvError::Disconnected) => break,
                },
                Err(crossbeam::TryRecvError::Disconnected) => break,
            };

            match msg {
                OutMsg::UserAudio {
                    user_id,
                    pcm,
                    signal_peak_abs,
                    signal_active_sample_count,
                    signal_sample_count,
                } => {
                    let Some(payload) = encode_user_audio_payload(
                        &user_id,
                        &pcm,
                        signal_peak_abs,
                        signal_active_sample_count,
                        signal_sample_count,
                    ) else {
                        continue;
                    };

                    let len = payload.len() as u32;
                    if let Err(e) = out
                        .write_all(&[1])
                        .and_then(|()| out.write_all(&len.to_le_bytes()))
                        .and_then(|()| out.write_all(&payload))
                        .and_then(|()| out.flush())
                    {
                        // Stdout broken — parent process likely exited. Audio frames
                        // are lossy so we just log once and let the reader thread
                        // detect stdin EOF to trigger a clean shutdown.
                        error!("IPC stdout write failed (audio): {e}");
                        break;
                    }
                }
                other => {
                    if let Ok(json) = serde_json::to_string(&other) {
                        let payload = json.as_bytes();
                        let len = payload.len() as u32;
                        if let Err(e) = out
                            .write_all(&[0])
                            .and_then(|()| out.write_all(&len.to_le_bytes()))
                            .and_then(|()| out.write_all(payload))
                            .and_then(|()| out.flush())
                        {
                            error!("IPC stdout write failed (control): {e}");
                            break;
                        }
                    }
                }
            }
        }
    });

    let senders = IpcSenders {
        control_tx,
        audio_tx,
        video_tx,
    };
    IPC_TX
        .set(senders.clone())
        .expect("IPC_TX already initialized");
}

#[cfg(test)]
mod tests {
    use super::encode_user_audio_payload;

    #[test]
    fn encode_user_audio_payload_serializes_header_fields() {
        let payload = encode_user_audio_payload("42", &[1, 2, 3, 4], 7, 8, 9).expect("payload");

        assert_eq!(&payload[0..8], &42_u64.to_le_bytes());
        assert_eq!(&payload[8..10], &7_u16.to_le_bytes());
        assert_eq!(&payload[10..14], &8_u32.to_le_bytes());
        assert_eq!(&payload[14..18], &9_u32.to_le_bytes());
        assert_eq!(&payload[18..], &[1, 2, 3, 4]);
    }

    #[test]
    fn encode_user_audio_payload_rejects_non_numeric_user_ids() {
        assert!(encode_user_audio_payload("not-a-user", &[], 0, 0, 0).is_none());
    }
}

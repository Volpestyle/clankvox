use std::io::{self, Write};

use crossbeam_channel as crossbeam;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{error, info};

static IPC_TX: std::sync::OnceLock<crossbeam::Sender<OutMsg>> = std::sync::OnceLock::new();

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

pub fn default_sample_rate() -> u32 {
    24000
}

pub fn default_silence_duration() -> u32 {
    700
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

#[derive(Deserialize, Debug, Clone)]
pub struct VoiceServerData {
    pub endpoint: Option<String>,
    pub token: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct VoiceStateData {
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub channel_id: Option<String>,
}

fn is_lossy_ipc_msg(msg: &OutMsg) -> bool {
    matches!(msg, OutMsg::UserAudio { .. })
}

pub fn send_msg(msg: &OutMsg) {
    if let Some(tx) = IPC_TX.get() {
        if is_lossy_ipc_msg(msg) {
            if let Err(err) = tx.try_send(msg.clone()) {
                if !matches!(err, crossbeam::TrySendError::Full(_)) {
                    error!("failed to send lossy IPC message: {}", err);
                }
            }
        } else if let Err(err) = tx.send(msg.clone()) {
            error!("failed to send IPC message: {}", err);
        }
    }
}

pub fn send_error(message: &str) {
    send_msg(&OutMsg::Error {
        message: message.to_string(),
    });
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
    info!(
        tts_samples = tts_samples,
        music_samples = music_samples,
        reason = reason,
        "clankvox_buffer_depth"
    );
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

pub fn spawn_ipc_writer() -> crossbeam::Sender<OutMsg> {
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
                    let _ = out.write_all(&[1]);
                    let _ = out.write_all(&len.to_le_bytes());
                    let _ = out.write_all(&payload);
                    let _ = out.flush();
                }
                other => {
                    if let Ok(json) = serde_json::to_string(&other) {
                        let payload = json.as_bytes();
                        let len = payload.len() as u32;
                        let _ = out.write_all(&[0]);
                        let _ = out.write_all(&len.to_le_bytes());
                        let _ = out.write_all(payload);
                        let _ = out.flush();
                    }
                }
            }
        }
    });
    IPC_TX.set(tx.clone()).expect("IPC_TX already initialized");
    tx
}

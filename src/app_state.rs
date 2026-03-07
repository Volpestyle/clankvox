use std::collections::HashMap;
use std::sync::Arc;

use audiopus::coder::Decoder as OpusDecoder;
use crossbeam_channel as crossbeam;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time;
use tracing::warn;

use crate::audio_pipeline::AudioSendState;
use crate::capture::{SpeakingState, UserCaptureState};
use crate::dave::DaveManager;
use crate::ipc::{send_error, send_gateway_voice_state_update, send_msg, ErrorCode, OutMsg};
use crate::music::{drain_music_pcm_queue, MusicEvent, MusicState};
use crate::voice_conn::{VoiceConnection, VoiceEvent};

pub(crate) fn schedule_reconnect(
    reconnect_deadline: &mut Option<time::Instant>,
    reconnect_attempt: &mut u32,
    guild_id: Option<u64>,
    channel_id: Option<u64>,
    self_mute: bool,
    reason: &str,
) {
    let (Some(guild_id), Some(channel_id)) = (guild_id, channel_id) else {
        warn!(reason = reason, "reconnect skipped: missing guild/channel");
        return;
    };

    *reconnect_attempt = reconnect_attempt.saturating_add(1);
    let backoff_shift = reconnect_attempt.saturating_sub(1).min(4);
    let backoff_ms = 1_000u64 << backoff_shift;
    *reconnect_deadline = Some(time::Instant::now() + std::time::Duration::from_millis(backoff_ms));

    send_msg(&OutMsg::ConnectionState {
        status: "reconnecting".into(),
    });
    send_gateway_voice_state_update(guild_id, channel_id, self_mute);
    warn!(
        attempt = *reconnect_attempt,
        backoff_ms = backoff_ms,
        reason = reason,
        "scheduled clankvox voice reconnect"
    );
}

#[derive(Default, Clone)]
pub(crate) struct PendingConnection {
    pub(crate) endpoint: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<u64>,
}

pub(crate) fn parse_user_id_field(user_id: &str, context: &str) -> Option<u64> {
    if let Ok(uid) = user_id.parse::<u64>() {
        Some(uid)
    } else {
        send_error(
            ErrorCode::InvalidRequest,
            format!("{context} requires a numeric user_id, got {user_id:?}"),
        );
        None
    }
}

pub(crate) struct AppState {
    pub(crate) pending_conn: PendingConnection,
    pub(crate) guild_id: Option<u64>,
    pub(crate) channel_id: Option<u64>,
    pub(crate) self_mute: bool,
    pub(crate) reconnect_deadline: Option<time::Instant>,
    pub(crate) reconnect_attempt: u32,
    pub(crate) dave: Arc<Mutex<Option<DaveManager>>>,
    pub(crate) voice_conn: Option<VoiceConnection>,
    pub(crate) voice_event_tx: mpsc::Sender<VoiceEvent>,
    pub(crate) audio_send_state: Arc<Mutex<Option<AudioSendState>>>,
    pub(crate) music_pcm_tx: crossbeam::Sender<Vec<i16>>,
    pub(crate) music_pcm_rx: crossbeam::Receiver<Vec<i16>>,
    pub(crate) music_event_tx: mpsc::Sender<MusicEvent>,
    pub(crate) music: MusicState,
    pub(crate) opus_decoders: HashMap<u32, OpusDecoder>,
    pub(crate) ssrc_map: HashMap<u32, u64>,
    pub(crate) self_user_id: Option<u64>,
    pub(crate) user_capture_states: HashMap<u64, UserCaptureState>,
    pub(crate) speaking_states: HashMap<u64, SpeakingState>,
    pub(crate) buffer_depth_tick_counter: u32,
    pub(crate) buffer_depth_was_nonempty: bool,
    pub(crate) tts_playback_buffered: bool,
}

impl AppState {
    pub(crate) const BUFFER_DEPTH_REPORT_INTERVAL: u32 = 25;

    pub(crate) fn new(
        dave: Arc<Mutex<Option<DaveManager>>>,
        voice_event_tx: mpsc::Sender<VoiceEvent>,
        audio_send_state: Arc<Mutex<Option<AudioSendState>>>,
        music_pcm_tx: crossbeam::Sender<Vec<i16>>,
        music_pcm_rx: crossbeam::Receiver<Vec<i16>>,
        music_event_tx: mpsc::Sender<MusicEvent>,
    ) -> Self {
        Self {
            pending_conn: PendingConnection::default(),
            guild_id: None,
            channel_id: None,
            self_mute: false,
            reconnect_deadline: None,
            reconnect_attempt: 0,
            dave,
            voice_conn: None,
            voice_event_tx,
            audio_send_state,
            music_pcm_tx,
            music_pcm_rx,
            music_event_tx,
            music: MusicState::default(),
            opus_decoders: HashMap::new(),
            ssrc_map: HashMap::new(),
            self_user_id: None,
            user_capture_states: HashMap::new(),
            speaking_states: HashMap::new(),
            buffer_depth_tick_counter: 0,
            buffer_depth_was_nonempty: false,
            tts_playback_buffered: false,
        }
    }

    pub(crate) fn start_music_pipeline(
        &mut self,
        url: &str,
        resolved_direct_url: bool,
        clear_output_buffers: bool,
    ) {
        crate::music::start_music_pipeline(
            crate::music::MusicPipelineRequest {
                url,
                resolved_direct_url,
                clear_output_buffers,
            },
            crate::music::MusicPipelineContext {
                music_player: &mut self.music.player,
                music_pcm_rx: &self.music_pcm_rx,
                music_pcm_tx: &self.music_pcm_tx,
                music_event_tx: &self.music_event_tx,
                audio_send_state: &self.audio_send_state,
            },
        );
    }

    pub(crate) fn schedule_reconnect(&mut self, reason: &str) {
        schedule_reconnect(
            &mut self.reconnect_deadline,
            &mut self.reconnect_attempt,
            self.guild_id,
            self.channel_id,
            self.self_mute,
            reason,
        );
    }

    pub(crate) fn reset_reconnect(&mut self) {
        self.reconnect_deadline = None;
        self.reconnect_attempt = 0;
    }

    pub(crate) fn clear_voice_connection(&mut self) {
        if let Some(ref conn) = self.voice_conn {
            conn.shutdown();
        }
        self.voice_conn = None;
        *self.audio_send_state.lock() = None;
    }

    pub(crate) fn remove_user_runtime_state(&mut self, user_id: u64) {
        let removed_ssrcs = self
            .ssrc_map
            .iter()
            .filter_map(|(&ssrc, &mapped_uid)| (mapped_uid == user_id).then_some(ssrc))
            .collect::<Vec<_>>();
        self.ssrc_map.retain(|_, v| *v != user_id);
        for ssrc in removed_ssrcs {
            self.opus_decoders.remove(&ssrc);
        }

        let uid_str = user_id.to_string();
        if let Some(ss) = self.speaking_states.remove(&user_id) {
            if ss.is_speaking {
                send_msg(&OutMsg::SpeakingEnd {
                    user_id: uid_str.clone(),
                });
            }
        }

        if let Some(state) = self.user_capture_states.remove(&user_id) {
            if state.stream_active {
                send_msg(&OutMsg::UserAudioEnd {
                    user_id: uid_str.clone(),
                });
            }
        }

        send_msg(&OutMsg::ClientDisconnect { user_id: uid_str });
    }

    pub(crate) fn handle_disconnected(&mut self, reason: &str) {
        tracing::warn!("Voice disconnected: {}", reason);
        send_error(ErrorCode::VoiceRuntimeError, reason.to_string());
        send_msg(&OutMsg::ConnectionState {
            status: "disconnected".into(),
        });
        self.music.reset();
        drain_music_pcm_queue(&self.music_pcm_rx);
        self.clear_voice_connection();
        self.ssrc_map.clear();
        self.opus_decoders.clear();
        self.speaking_states.clear();
        self.schedule_reconnect(reason);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TryConnectOutcome {
    AlreadyConnected,
    MissingData,
    Connected,
    Failed,
}

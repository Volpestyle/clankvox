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
use crate::stream_publish::{StreamPublishEvent, StreamPublishFrame, StreamPublishState};
use crate::video::{RemoteVideoState, UserVideoSubscription};
use crate::video_decoder::PersistentVideoDecoder;
use crate::voice_conn::{TransportRole, VoiceConnection, VoiceEvent};

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
    send_msg(&OutMsg::TransportState {
        role: TransportRole::Voice,
        status: "reconnecting".into(),
        reason: Some(reason.to_string()),
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

#[derive(Default, Clone)]
pub(crate) struct PendingStreamWatchConnection {
    pub(crate) endpoint: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<u64>,
    pub(crate) server_id: Option<u64>,
    pub(crate) dave_channel_id: Option<u64>,
}

#[derive(Default, Clone)]
pub(crate) struct PendingStreamPublishConnection {
    pub(crate) endpoint: Option<String>,
    pub(crate) token: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<u64>,
    pub(crate) server_id: Option<u64>,
    pub(crate) dave_channel_id: Option<u64>,
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
    pub(crate) stream_watch_pending_conn: PendingStreamWatchConnection,
    pub(crate) stream_watch_dave: Arc<Mutex<Option<DaveManager>>>,
    pub(crate) stream_watch_conn: Option<VoiceConnection>,
    pub(crate) stream_publish_pending_conn: PendingStreamPublishConnection,
    pub(crate) stream_publish_dave: Arc<Mutex<Option<DaveManager>>>,
    pub(crate) stream_publish_conn: Option<VoiceConnection>,
    pub(crate) voice_event_tx: mpsc::Sender<VoiceEvent>,
    pub(crate) audio_send_state: Arc<Mutex<Option<AudioSendState>>>,
    pub(crate) music_pcm_tx: crossbeam::Sender<Vec<i16>>,
    pub(crate) music_pcm_rx: crossbeam::Receiver<Vec<i16>>,
    pub(crate) music_event_tx: mpsc::Sender<MusicEvent>,
    pub(crate) stream_publish_frame_tx: crossbeam::Sender<StreamPublishFrame>,
    pub(crate) stream_publish_frame_rx: crossbeam::Receiver<StreamPublishFrame>,
    pub(crate) stream_publish_event_tx: crossbeam::Sender<StreamPublishEvent>,
    pub(crate) stream_publish_event_rx: crossbeam::Receiver<StreamPublishEvent>,
    pub(crate) music: MusicState,
    pub(crate) stream_publish: StreamPublishState,
    pub(crate) opus_decoders: HashMap<u32, OpusDecoder>,
    /// Last RTP sequence number seen per audio SSRC, for gap detection / FEC / PLC.
    pub(crate) last_rtp_seq: HashMap<u32, u16>,
    pub(crate) ssrc_map: HashMap<u32, u64>,
    pub(crate) self_user_id: Option<u64>,
    pub(crate) user_capture_states: HashMap<u64, UserCaptureState>,
    pub(crate) user_video_subscriptions: HashMap<u64, UserVideoSubscription>,
    pub(crate) user_video_decoders: HashMap<u64, PersistentVideoDecoder>,
    pub(crate) remote_video_states: HashMap<u64, RemoteVideoState>,
    pub(crate) speaking_states: HashMap<u64, SpeakingState>,
    pub(crate) buffer_depth_tick_counter: u32,
    pub(crate) buffer_depth_was_nonempty: bool,
    pub(crate) tts_playback_buffered: bool,
    pub(crate) stream_publish_frames_sent: u64,
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
        stream_publish_frame_tx: crossbeam::Sender<StreamPublishFrame>,
        stream_publish_frame_rx: crossbeam::Receiver<StreamPublishFrame>,
        stream_publish_event_tx: crossbeam::Sender<StreamPublishEvent>,
        stream_publish_event_rx: crossbeam::Receiver<StreamPublishEvent>,
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
            stream_watch_pending_conn: PendingStreamWatchConnection::default(),
            stream_watch_dave: Arc::new(Mutex::new(None)),
            stream_watch_conn: None,
            stream_publish_pending_conn: PendingStreamPublishConnection::default(),
            stream_publish_dave: Arc::new(Mutex::new(None)),
            stream_publish_conn: None,
            voice_event_tx,
            audio_send_state,
            music_pcm_tx,
            music_pcm_rx,
            music_event_tx,
            stream_publish_frame_tx,
            stream_publish_frame_rx,
            stream_publish_event_tx,
            stream_publish_event_rx,
            music: MusicState::default(),
            stream_publish: StreamPublishState::default(),
            opus_decoders: HashMap::new(),
            last_rtp_seq: HashMap::new(),
            ssrc_map: HashMap::new(),
            self_user_id: None,
            user_capture_states: HashMap::new(),
            user_video_subscriptions: HashMap::new(),
            user_video_decoders: HashMap::new(),
            remote_video_states: HashMap::new(),
            speaking_states: HashMap::new(),
            buffer_depth_tick_counter: 0,
            buffer_depth_was_nonempty: false,
            tts_playback_buffered: false,
            stream_publish_frames_sent: 0,
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
        *self.dave.lock() = None;
        *self.audio_send_state.lock() = None;
    }

    pub(crate) fn clear_stream_watch_connection(&mut self) {
        if let Some(ref conn) = self.stream_watch_conn {
            conn.shutdown();
        }
        self.stream_watch_conn = None;
        *self.stream_watch_dave.lock() = None;
    }

    pub(crate) fn clear_stream_publish_connection(&mut self) {
        if let Some(ref conn) = self.stream_publish_conn {
            conn.shutdown();
        }
        self.stream_publish_conn = None;
        *self.stream_publish_dave.lock() = None;
    }

    pub(crate) fn video_conn(&self) -> Option<&VoiceConnection> {
        self.stream_watch_conn.as_ref().or(self.voice_conn.as_ref())
    }

    pub(crate) fn emit_transport_state(
        &self,
        role: TransportRole,
        status: &str,
        reason: Option<&str>,
    ) {
        send_msg(&OutMsg::TransportState {
            role,
            status: status.to_string(),
            reason: reason.map(ToString::to_string),
        });
    }

    pub(crate) fn clear_transport_runtime_state(&mut self, reason: &str) {
        let cleared_audio_ssrcs = self.ssrc_map.len();
        let cleared_decoders = self.opus_decoders.len();
        let cleared_speaking_users = self.speaking_states.len();
        let cleared_video_users = self.remote_video_states.len();
        let cleared_video_decoders = self.user_video_decoders.len();
        self.ssrc_map.clear();
        self.opus_decoders.clear();
        self.last_rtp_seq.clear();
        self.speaking_states.clear();
        self.remote_video_states.clear();
        self.user_video_decoders.clear();

        tracing::info!(
            reason = reason,
            cleared_audio_ssrcs,
            cleared_decoders,
            cleared_speaking_users,
            cleared_video_users,
            cleared_video_decoders,
            "cleared voice transport runtime state"
        );
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
            self.last_rtp_seq.remove(&ssrc);
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

        self.user_video_subscriptions.remove(&user_id);
        self.user_video_decoders.remove(&user_id);
        if let Some(state) = self.remote_video_states.remove(&user_id) {
            let end_ssrc = state
                .video_ssrc
                .or_else(|| state.streams.first().map(|stream| stream.ssrc));
            send_msg(&OutMsg::UserVideoEnd {
                user_id: uid_str.clone(),
                ssrc: end_ssrc,
            });
        }

        send_msg(&OutMsg::ClientDisconnect { user_id: uid_str });
    }

    pub(crate) fn handle_disconnected(&mut self, reason: &str) {
        tracing::warn!("Voice disconnected: {}", reason);
        send_error(ErrorCode::VoiceRuntimeError, reason.to_string());
        send_msg(&OutMsg::ConnectionState {
            status: "disconnected".into(),
        });
        self.emit_transport_state(TransportRole::Voice, "disconnected", Some(reason));
        self.music.reset();
        self.stream_publish.reset();
        drain_music_pcm_queue(&self.music_pcm_rx);
        self.clear_voice_connection();
        self.clear_stream_publish_connection();
        self.clear_transport_runtime_state(reason);

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel as crossbeam;
    use parking_lot::Mutex;
    use tokio::sync::mpsc;

    use crate::audio_pipeline::AudioSendState;
    use crate::capture::SpeakingState;
    use crate::dave::DaveManager;
    use crate::music::MusicEvent;
    use crate::stream_publish::{StreamPublishEvent, StreamPublishFrame};
    use crate::video::{RemoteVideoState, UserVideoSubscription, VideoStreamDescriptor};
    use crate::voice_conn::VoiceEvent;

    use super::AppState;

    fn test_app_state() -> AppState {
        let dave: Arc<Mutex<Option<DaveManager>>> = Arc::new(Mutex::new(None));
        let (voice_event_tx, _voice_event_rx) = mpsc::channel::<VoiceEvent>(4);
        let audio_send_state = Arc::new(Mutex::new(None::<AudioSendState>));
        let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let (music_event_tx, _music_event_rx) = mpsc::channel::<MusicEvent>(4);
        let (stream_publish_frame_tx, stream_publish_frame_rx) =
            crossbeam::bounded::<StreamPublishFrame>(4);
        let (stream_publish_event_tx, stream_publish_event_rx) =
            crossbeam::bounded::<StreamPublishEvent>(4);

        AppState::new(
            dave,
            voice_event_tx,
            audio_send_state,
            music_pcm_tx,
            music_pcm_rx,
            music_event_tx,
            stream_publish_frame_tx,
            stream_publish_frame_rx,
            stream_publish_event_tx,
            stream_publish_event_rx,
        )
    }

    #[test]
    fn clear_transport_runtime_state_drops_transport_state_but_keeps_subscriptions() {
        let mut state = test_app_state();
        state.ssrc_map.insert(111, 42);
        state.speaking_states.insert(
            42,
            SpeakingState {
                last_packet_at: None,
                is_speaking: true,
            },
        );
        state.user_video_subscriptions.insert(
            42,
            UserVideoSubscription::new(15, 80, Some(1_280 * 720), Some("screen".into()), None),
        );
        state.remote_video_states.insert(
            42,
            RemoteVideoState {
                audio_ssrc: Some(211),
                video_ssrc: Some(311),
                codec: Some("h264".into()),
                streams: vec![VideoStreamDescriptor {
                    ssrc: 311,
                    rtx_ssrc: Some(411),
                    rid: Some("f".into()),
                    quality: Some(100),
                    stream_type: Some("screen".into()),
                    active: Some(true),
                    max_bitrate: Some(4_000_000),
                    max_framerate: Some(30),
                    max_resolution: None,
                }],
            },
        );

        state.clear_transport_runtime_state("test");

        assert!(state.ssrc_map.is_empty());
        assert!(state.opus_decoders.is_empty());
        assert!(state.speaking_states.is_empty());
        assert!(state.remote_video_states.is_empty());
        assert!(state.user_video_subscriptions.contains_key(&42));
    }
}

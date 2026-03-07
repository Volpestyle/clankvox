mod asr;
mod audio_pipeline;
mod capture;
mod dave;
mod ipc;
mod music;
mod voice_conn;

use std::collections::{hash_map::Entry, HashMap};
use std::future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use audiopus::coder::Decoder as OpusDecoder;
use audiopus::packet::Packet as OpusPacket;
use audiopus::{Channels, MutSignals, SampleRate};
use base64::Engine as _;
use crossbeam_channel as crossbeam;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::asr::{run_asr_client, AsrCommand};
use crate::audio_pipeline::{
    clear_audio_send_buffer, clear_tts_send_buffer, convert_decoded_to_llm,
    convert_llm_to_48k_mono, emit_playback_armed, AudioSendState,
};
use crate::capture::{
    normalize_sample_rate, normalize_silence_duration_ms, SpeakingState, UserCaptureState,
    SPEAKING_TIMEOUT_MS,
};
use crate::dave::DaveManager;
use crate::ipc::{
    send_buffer_depth, send_error, send_gateway_voice_state_update, send_msg,
    send_tts_playback_state, spawn_ipc_reader, spawn_ipc_writer, ErrorCode, InMsg, OutMsg,
};
use crate::music::{
    drain_music_pcm_queue, is_music_output_drained, start_music_pipeline, MusicEvent,
    MusicPipelineContext, MusicPipelineRequest, MusicState,
};
use crate::voice_conn::{VoiceConnection, VoiceConnectionParams, VoiceEvent};

fn schedule_reconnect(
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
    *reconnect_deadline = Some(time::Instant::now() + Duration::from_millis(backoff_ms));

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

struct AsrSession {
    session_id: u64,
    tx: mpsc::UnboundedSender<AsrCommand>,
    handle: JoinHandle<()>,
}

fn shutdown_asr_session(session: AsrSession) {
    let _ = session.tx.send(AsrCommand::Shutdown);
    let handle = session.handle;
    tokio::spawn(async move {
        let _ = time::timeout(Duration::from_secs(2), handle).await;
    });
}

fn parse_user_id_field(user_id: &str, context: &str) -> Option<u64> {
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

struct AppState {
    pending_conn: PendingConnection,
    guild_id: Option<u64>,
    channel_id: Option<u64>,
    self_mute: bool,
    reconnect_deadline: Option<time::Instant>,
    reconnect_attempt: u32,
    dave: Arc<Mutex<Option<DaveManager>>>,
    voice_conn: Option<VoiceConnection>,
    voice_event_tx: mpsc::Sender<VoiceEvent>,
    audio_send_state: Arc<Mutex<Option<AudioSendState>>>,
    music_pcm_tx: crossbeam::Sender<Vec<i16>>,
    music_pcm_rx: crossbeam::Receiver<Vec<i16>>,
    music_event_tx: mpsc::Sender<MusicEvent>,
    music: MusicState,
    opus_decoders: HashMap<u32, OpusDecoder>,
    ssrc_map: HashMap<u32, u64>,
    self_user_id: Option<u64>,
    user_capture_states: HashMap<u64, UserCaptureState>,
    asr_sessions: HashMap<u64, AsrSession>,
    asr_exit_tx: mpsc::Sender<(u64, u64, String)>,
    next_asr_session_id: u64,
    speaking_states: HashMap<u64, SpeakingState>,
    buffer_depth_tick_counter: u32,
    buffer_depth_was_nonempty: bool,
    tts_playback_buffered: bool,
}

impl AppState {
    const BUFFER_DEPTH_REPORT_INTERVAL: u32 = 25;

    fn new(
        dave: Arc<Mutex<Option<DaveManager>>>,
        voice_event_tx: mpsc::Sender<VoiceEvent>,
        audio_send_state: Arc<Mutex<Option<AudioSendState>>>,
        music_pcm_tx: crossbeam::Sender<Vec<i16>>,
        music_pcm_rx: crossbeam::Receiver<Vec<i16>>,
        music_event_tx: mpsc::Sender<MusicEvent>,
        asr_exit_tx: mpsc::Sender<(u64, u64, String)>,
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
            asr_sessions: HashMap::new(),
            asr_exit_tx,
            next_asr_session_id: 1,
            speaking_states: HashMap::new(),
            buffer_depth_tick_counter: 0,
            buffer_depth_was_nonempty: false,
            tts_playback_buffered: false,
        }
    }

    fn schedule_reconnect(&mut self, reason: &str) {
        schedule_reconnect(
            &mut self.reconnect_deadline,
            &mut self.reconnect_attempt,
            self.guild_id,
            self.channel_id,
            self.self_mute,
            reason,
        );
    }

    fn reset_reconnect(&mut self) {
        self.reconnect_deadline = None;
        self.reconnect_attempt = 0;
    }

    fn start_music_pipeline(&mut self, url: &str, resolved_direct_url: bool, clear_output_buffers: bool) {
        start_music_pipeline(
            MusicPipelineRequest {
                url,
                resolved_direct_url,
                clear_output_buffers,
            },
            MusicPipelineContext {
                music_player: &mut self.music.player,
                music_pcm_rx: &self.music_pcm_rx,
                music_pcm_tx: &self.music_pcm_tx,
                music_event_tx: &self.music_event_tx,
                audio_send_state: &self.audio_send_state,
            },
        );
    }

    fn clear_voice_connection(&mut self) {
        if let Some(ref conn) = self.voice_conn {
            conn.shutdown();
        }
        self.voice_conn = None;
        *self.audio_send_state.lock() = None;
    }

    fn shutdown_all_asr(&mut self) {
        for (_, session) in self.asr_sessions.drain() {
            shutdown_asr_session(session);
        }
    }

    fn remove_user_runtime_state(&mut self, user_id: u64) {
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
                send_msg(&OutMsg::SpeakingEnd { user_id: uid_str.clone() });
            }
        }

        if let Some(state) = self.user_capture_states.remove(&user_id) {
            if state.stream_active {
                send_msg(&OutMsg::UserAudioEnd {
                    user_id: uid_str.clone(),
                });
            }
        }

        if let Some(session) = self.asr_sessions.remove(&user_id) {
            shutdown_asr_session(session);
        }

        send_msg(&OutMsg::ClientDisconnect { user_id: uid_str });
    }

    fn handle_disconnected(&mut self, reason: &str) {
        warn!("Voice disconnected: {}", reason);
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

    fn apply_connect_outcome(&mut self, outcome: TryConnectOutcome, failure_reason: &str) {
        match outcome {
            TryConnectOutcome::Connected => self.reset_reconnect(),
            TryConnectOutcome::Failed => self.schedule_reconnect(failure_reason),
            TryConnectOutcome::AlreadyConnected | TryConnectOutcome::MissingData => {}
        }
    }

    async fn attempt_connect(&mut self) -> TryConnectOutcome {
        try_connect(
            &self.pending_conn,
            self.guild_id,
            self.channel_id,
            &self.voice_event_tx,
            &self.dave,
            &mut self.voice_conn,
        )
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_ipc_msg(&mut self, msg: InMsg) -> bool {
        match msg {
            InMsg::Join {
                guild_id: gid,
                channel_id: cid,
                _self_deaf: _,
                self_mute: sm,
            } => {
                let Ok(g) = gid.parse::<u64>() else {
                    send_error(
                        ErrorCode::InvalidRequest,
                        format!("join requires a numeric guild_id, got {gid:?}"),
                    );
                    return false;
                };
                let Ok(c) = cid.parse::<u64>() else {
                    send_error(
                        ErrorCode::InvalidRequest,
                        format!("join requires a numeric channel_id, got {cid:?}"),
                    );
                    return false;
                };
                self.guild_id = Some(g);
                self.channel_id = Some(c);
                self.self_mute = sm;
                self.reset_reconnect();

                send_gateway_voice_state_update(g, c, sm);
                info!("Join requested guild={} channel={}, sent OP4", g, c);
            }
            InMsg::VoiceServer { data } => {
                let ep = data.endpoint.clone();
                let has_token = data.token.is_some();
                info!(
                    "IPC voice_server: endpoint={:?} token={} connected={}",
                    ep,
                    if has_token { "present" } else { "missing" },
                    self.voice_conn.is_some()
                );
                if let Some(ref e) = ep {
                    self.pending_conn.endpoint = Some(e.clone());
                }
                if let Some(tk) = data.token.as_deref() {
                    self.pending_conn.token = Some(tk.to_string());
                }
                let outcome = self.attempt_connect().await;
                self.apply_connect_outcome(outcome, "voice_server_connect_failed");
            }
            InMsg::VoiceState { data } => {
                let new_session_id = data.session_id.clone();
                let old_session_id = self.pending_conn.session_id.clone();
                let new_user_id = match data.user_id.as_deref() {
                    Some(user_id) => parse_user_id_field(user_id, "voice_state"),
                    None => None,
                };
                let new_channel = data.channel_id.clone();
                info!(
                    "IPC voice_state: session_id={:?} prev_session_id={:?} channel_id={:?} user_id={:?} connected={}",
                    new_session_id,
                    old_session_id,
                    new_channel,
                    new_user_id,
                    self.voice_conn.is_some()
                );

                if let Some(ref sid) = new_session_id {
                    if self.voice_conn.is_some() && old_session_id.as_deref() != Some(sid.as_str()) {
                        warn!(
                            "Session ID changed while connected: {:?} -> {:?}, tearing down for reconnect",
                            old_session_id,
                            new_session_id
                        );
                        self.clear_voice_connection();
                    }
                    self.pending_conn.session_id = Some(sid.clone());
                }
                if let Some(uid) = new_user_id {
                    self.pending_conn.user_id = Some(uid);
                    self.self_user_id = Some(uid);
                }
                let outcome = self.attempt_connect().await;
                self.apply_connect_outcome(outcome, "voice_state_connect_failed");
            }
            InMsg::Audio {
                pcm_base64,
                sample_rate,
            } => {
                let now = time::Instant::now();
                if self.music.pending_url.is_some() {
                    self.music.pending_audio_seen = true;
                    self.music.pending_last_audio_at = Some(now);
                }

                if self.music.active && !self.music.paused {
                    let is_ducked = {
                        let guard = self.audio_send_state.lock();
                        guard
                            .as_ref()
                            .is_some_and(audio_pipeline::AudioSendState::is_music_ducked)
                    };
                    if !is_ducked {
                        return false;
                    }
                }

                let engine = base64::engine::general_purpose::STANDARD;
                if let Ok(raw) = engine.decode(&pcm_base64) {
                    let samples = convert_llm_to_48k_mono(&raw, sample_rate);
                    if !samples.is_empty() {
                        let mut emit_tts_buffered = false;
                        {
                            let mut guard = self.audio_send_state.lock();
                            if let Some(ref mut state) = *guard {
                                state.push_pcm(samples);
                                if state.tts_buffer_samples() > 0 && !self.tts_playback_buffered {
                                    self.tts_playback_buffered = true;
                                    emit_tts_buffered = true;
                                }
                            }
                        }
                        if emit_tts_buffered {
                            send_tts_playback_state("buffered", "tts_pcm_enqueued");
                        }
                    }
                }
            }
            InMsg::StopPlayback => {
                self.music.reset();
                drain_music_pcm_queue(&self.music_pcm_rx);
                clear_audio_send_buffer(&self.audio_send_state);
                if self.tts_playback_buffered {
                    self.tts_playback_buffered = false;
                    send_tts_playback_state("idle", "stop_playback");
                }
                send_msg(&OutMsg::PlayerState {
                    status: "idle".into(),
                });
                emit_playback_armed("stop_playback", &self.audio_send_state);
            }
            InMsg::StopTtsPlayback => {
                clear_tts_send_buffer(&self.audio_send_state);
                if self.tts_playback_buffered {
                    self.tts_playback_buffered = false;
                    send_tts_playback_state("idle", "stop_tts_playback");
                }
            }
            InMsg::SubscribeUser {
                user_id,
                silence_duration_ms,
                sample_rate,
            } => {
                let Some(uid) = parse_user_id_field(&user_id, "subscribe_user") else {
                    return false;
                };
                let normalized_sample_rate = normalize_sample_rate(sample_rate);
                let normalized_silence_duration_ms =
                    normalize_silence_duration_ms(silence_duration_ms);
                let state = self.user_capture_states.entry(uid).or_insert_with(|| {
                    UserCaptureState::new(normalized_sample_rate, normalized_silence_duration_ms)
                });
                state.sample_rate = normalized_sample_rate;
                state.silence_duration_ms = normalized_silence_duration_ms;
            }
            InMsg::UnsubscribeUser { user_id } => {
                let Some(uid) = parse_user_id_field(&user_id, "unsubscribe_user") else {
                    return false;
                };
                if let Some(state) = self.user_capture_states.remove(&uid) {
                    if state.stream_active {
                        send_msg(&OutMsg::UserAudioEnd {
                            user_id: uid.to_string(),
                        });
                    }
                }
            }
            InMsg::MusicPlay {
                url,
                resolved_direct_url,
            } => {
                let normalized_url = url.trim().to_string();
                if normalized_url.is_empty() {
                    send_msg(&OutMsg::MusicError {
                        message: "music_play missing url".to_string(),
                    });
                    return false;
                }

                self.music
                    .queue_pending_start(normalized_url.clone(), resolved_direct_url);
                self.start_music_pipeline(&normalized_url, resolved_direct_url, false);
                info!(
                    "music_play queued pending start url={} direct={} (waiting for announcement drain)",
                    normalized_url,
                    resolved_direct_url
                );
            }
            InMsg::MusicStop => {
                if self.music.player.is_some() && self.music.active {
                    let mut guard = self.audio_send_state.lock();
                    if let Some(ref mut state) = *guard {
                        let _ = state.set_music_gain(0.0, 300);
                    }
                    self.music.pending_stop = true;
                } else {
                    self.music.reset();
                    drain_music_pcm_queue(&self.music_pcm_rx);
                    clear_audio_send_buffer(&self.audio_send_state);
                    if self.tts_playback_buffered {
                        self.tts_playback_buffered = false;
                        send_tts_playback_state("idle", "music_stop");
                    }
                    send_msg(&OutMsg::PlayerState {
                        status: "idle".into(),
                    });
                    send_msg(&OutMsg::MusicIdle);
                    emit_playback_armed("music_stop", &self.audio_send_state);
                }
            }
            InMsg::MusicPause => {
                self.music.clear_pending_start();
                self.music.pending_stop = false;
                let was_finishing = self.music.finishing;
                self.music.finishing = false;
                if self.music.player.is_some() || self.music.active || was_finishing {
                    self.music.paused = true;
                    self.music.active = false;
                    clear_audio_send_buffer(&self.audio_send_state);
                    send_msg(&OutMsg::PlayerState {
                        status: "paused".into(),
                    });
                    emit_playback_armed("music_pause", &self.audio_send_state);
                }
            }
            InMsg::MusicResume => {
                self.music.clear_pending_start();
                self.music.pending_stop = false;
                self.music.finishing = false;

                if self.music.player.is_some() {
                    self.music.paused = false;
                    self.music.active = true;
                    send_msg(&OutMsg::PlayerState {
                        status: "playing".into(),
                    });
                } else if let Some(url) = self.music.active_url.clone() {
                    self.start_music_pipeline(&url, self.music.active_resolved_direct_url, true);
                    self.music.paused = false;
                    self.music.active = true;
                    send_msg(&OutMsg::PlayerState {
                        status: "playing".into(),
                    });
                }
            }
            InMsg::MusicSetGain { target, fade_ms } => {
                let clamped = target.clamp(0.0, 1.0);
                let mut guard = self.audio_send_state.lock();
                if let Some(ref mut state) = *guard {
                    if let Some(reached) = state.set_music_gain(clamped, fade_ms) {
                        drop(guard);
                        send_msg(&OutMsg::MusicGainReached { gain: reached });
                    }
                }
            }
            InMsg::ConnectAsr {
                user_id,
                api_key,
                model,
                language,
                prompt,
            } => {
                let Some(uid) = parse_user_id_field(&user_id, "connect_asr") else {
                    return false;
                };

                if let Some(session) = self.asr_sessions.remove(&uid) {
                    shutdown_asr_session(session);
                }

                let (asr_tx, asr_rx) = tokio::sync::mpsc::unbounded_channel();
                let session_id = self.next_asr_session_id;
                self.next_asr_session_id = self.next_asr_session_id.saturating_add(1);

                let exit_tx = self.asr_exit_tx.clone();
                let handle = tokio::spawn(async move {
                    let reason = match run_asr_client(
                        user_id.clone(),
                        api_key,
                        model,
                        language,
                        prompt,
                        asr_rx,
                    )
                    .await
                    {
                        Ok(()) => "closed".to_string(),
                        Err(e) => {
                            error!("ASR client {} exited: {}", user_id, e);
                            format!("{e}")
                        }
                    };
                    let _ = exit_tx.send((uid, session_id, reason)).await;
                });
                self.asr_sessions.insert(
                    uid,
                    AsrSession {
                        session_id,
                        tx: asr_tx,
                        handle,
                    },
                );
            }
            InMsg::DisconnectAsr { user_id } => {
                let Some(uid) = parse_user_id_field(&user_id, "disconnect_asr") else {
                    return false;
                };
                if let Some(session) = self.asr_sessions.remove(&uid) {
                    shutdown_asr_session(session);
                }
            }
            InMsg::CommitAsr { user_id } => {
                let Some(uid) = parse_user_id_field(&user_id, "commit_asr") else {
                    return false;
                };
                if let Some(session) = self.asr_sessions.get(&uid) {
                    let _ = session.tx.send(AsrCommand::Commit);
                }
            }
            InMsg::ClearAsr { user_id } => {
                let Some(uid) = parse_user_id_field(&user_id, "clear_asr") else {
                    return false;
                };
                if let Some(session) = self.asr_sessions.get(&uid) {
                    let _ = session.tx.send(AsrCommand::Clear);
                }
            }
            InMsg::Destroy => {
                self.music.stop_player();
                self.shutdown_all_asr();
                self.clear_voice_connection();
                return true;
            }
        }

        false
    }

    #[allow(clippy::too_many_lines)]
    fn handle_voice_event(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::Ready { ssrc } => {
                info!("Voice connection ready, ssrc={}", ssrc);
                self.reset_reconnect();
                send_msg(&OutMsg::ConnectionState {
                    status: "ready".into(),
                });
                send_msg(&OutMsg::Ready);

                match AudioSendState::new() {
                    Ok(state) => {
                        *self.audio_send_state.lock() = Some(state);
                        send_msg(&OutMsg::PlaybackArmed {
                            reason: "connection_ready".into(),
                        });
                    }
                    Err(e) => error!("Failed to init audio send state: {}", e),
                }
            }
            VoiceEvent::SsrcUpdate { ssrc, user_id } => {
                if self.ssrc_map.insert(ssrc, user_id) != Some(user_id) {
                    self.opus_decoders.remove(&ssrc);
                }
            }
            VoiceEvent::ClientDisconnect { user_id } => {
                if self.self_user_id != Some(user_id) {
                    self.remove_user_runtime_state(user_id);
                }
            }
            VoiceEvent::OpusReceived { ssrc, opus_frame } => {
                let Some(&uid) = self.ssrc_map.get(&ssrc) else {
                    debug!("Dropped Opus frame from unknown ssrc: {ssrc}");
                    return;
                };
                if self.self_user_id == Some(uid) {
                    return;
                }

                let Some(state) = self.user_capture_states.get(&uid) else {
                    return;
                };
                let target_sample_rate = state.sample_rate;

                let mut pcm_stereo = vec![0i16; 5760];
                if let Entry::Vacant(entry) = self.opus_decoders.entry(ssrc) {
                    let decoder = match OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo) {
                        Ok(decoder) => decoder,
                        Err(error) => {
                            error!("failed to init Opus decoder for ssrc={}: {:?}", ssrc, error);
                            return;
                        }
                    };
                    entry.insert(decoder);
                }

                let decode_result = {
                    let packet = match OpusPacket::try_from(opus_frame.as_slice()) {
                        Ok(packet) => packet,
                        Err(error) => {
                            debug!("Invalid Opus packet for ssrc={}: {:?}", ssrc, error);
                            return;
                        }
                    };
                    let signals = MutSignals::try_from(pcm_stereo.as_mut_slice())
                        .expect("non-empty signal buffer");
                    self.opus_decoders
                        .get_mut(&ssrc)
                        .expect("decoder inserted above")
                        .decode(Some(packet), signals, false)
                };

                match decode_result {
                    Ok(samples_per_channel) => {
                        let total_samples = samples_per_channel * 2;
                        let decoded = &pcm_stereo[..total_samples];

                        let now = time::Instant::now();
                        let ss = self.speaking_states.entry(uid).or_insert(SpeakingState {
                            last_packet_at: None,
                            is_speaking: false,
                        });
                        ss.last_packet_at = Some(now);
                        if !ss.is_speaking {
                            ss.is_speaking = true;
                            send_msg(&OutMsg::SpeakingStart {
                                user_id: uid.to_string(),
                            });
                        }

                        let (llm_pcm, peak, active, total) =
                            convert_decoded_to_llm(decoded, target_sample_rate);
                        if !llm_pcm.is_empty() {
                            if let Some(session) = self.asr_sessions.get(&uid) {
                                let _ = session.tx.send(AsrCommand::Audio(llm_pcm.clone()));
                            }

                            send_msg(&OutMsg::UserAudio {
                                user_id: uid.to_string(),
                                pcm: llm_pcm,
                                signal_peak_abs: peak,
                                signal_active_sample_count: active,
                                signal_sample_count: total,
                            });

                            if let Some(state) = self.user_capture_states.get_mut(&uid) {
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
                self.handle_disconnected(&reason);
            }
        }
    }

    fn handle_music_event(&mut self, event: MusicEvent) {
        match event {
            MusicEvent::Idle => {
                self.music.stop_player();
                self.music.paused = false;
                self.music.finishing = self.music.active;
                self.music.pending_stop = false;
                self.music.clear_pending_start();
                if !self.music.finishing {
                    self.music.active_url = None;
                    self.music.active_resolved_direct_url = false;
                    send_msg(&OutMsg::PlayerState {
                        status: "idle".into(),
                    });
                    send_msg(&OutMsg::MusicIdle);
                    emit_playback_armed("music_idle", &self.audio_send_state);
                }
            }
            MusicEvent::Error(message) => {
                self.music.reset();
                drain_music_pcm_queue(&self.music_pcm_rx);
                send_msg(&OutMsg::MusicError { message });
                send_msg(&OutMsg::PlayerState {
                    status: "idle".into(),
                });
                emit_playback_armed("music_error", &self.audio_send_state);
            }
            MusicEvent::FirstPcm {
                startup_ms,
                resolved_direct_url,
            } => {
                let now = time::Instant::now();
                self.music.pending_first_pcm_at = Some(now);
                if let Some(received_at) = self.music.pending_received_at {
                    info!(
                        "music_play prepared url={} direct={} startupMs={} requestToFirstPcmMs={}",
                        self.music.pending_url.as_deref().unwrap_or("unknown"),
                        resolved_direct_url,
                        startup_ms,
                        now.duration_since(received_at).as_millis() as u64
                    );
                } else {
                    info!(
                        "music_play prepared url={} direct={} startupMs={}",
                        self.music.pending_url.as_deref().unwrap_or("unknown"),
                        resolved_direct_url,
                        startup_ms
                    );
                }
            }
        }
    }

    fn handle_asr_exit(&mut self, uid: u64, session_id: u64, reason: String) {
        let is_current_session = self
            .asr_sessions
            .get(&uid)
            .is_some_and(|session| session.session_id == session_id);
        if is_current_session {
            self.asr_sessions.remove(&uid);
            send_msg(&OutMsg::AsrDisconnected {
                user_id: uid.to_string(),
                reason,
            });
        }
    }

    async fn handle_reconnect_timer(&mut self) {
        self.reconnect_deadline = None;
        let outcome = self.attempt_connect().await;
        match outcome {
            TryConnectOutcome::Connected | TryConnectOutcome::AlreadyConnected => {
                self.reconnect_attempt = 0;
            }
            TryConnectOutcome::Failed | TryConnectOutcome::MissingData => {
                self.schedule_reconnect("reconnect_retry");
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn on_audio_tick(&mut self) {
        let now = time::Instant::now();

        let mut speaking_ended_users: Vec<u64> = Vec::new();
        for (&user_id, ss) in &mut self.speaking_states {
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
            send_msg(&OutMsg::SpeakingEnd {
                user_id: user_id.to_string(),
            });
        }

        for (user_id, state) in &mut self.user_capture_states {
            if !state.stream_active {
                continue;
            }
            let Some(last_audio_at) = state.last_audio_at else {
                state.last_audio_at = Some(now);
                continue;
            };
            let silent_for_ms = now.duration_since(last_audio_at).as_millis() as u64;
            if silent_for_ms >= u64::from(state.silence_duration_ms) {
                state.stream_active = false;
                state.last_audio_at = None;
                send_msg(&OutMsg::UserAudioEnd {
                    user_id: user_id.to_string(),
                });
            }
        }

        if let Some(url) = self.music.pending_url.clone() {
            let mut start_music = false;
            let mut reason = "pending_unknown";
            if let Some(received_at) = self.music.pending_received_at {
                let elapsed_ms = now.duration_since(received_at).as_millis() as u64;
                if elapsed_ms > 15_000 {
                    start_music = true;
                    reason = "pending_safety_timeout";
                } else if !self.music.pending_audio_seen {
                    if elapsed_ms > 5_000 {
                        start_music = true;
                        reason = "pending_no_announcement_audio";
                    }
                } else {
                    let last_audio_at = self.music.pending_last_audio_at.unwrap_or(received_at);
                    let gap_ms = now.duration_since(last_audio_at).as_millis() as u64;
                    if !self.music.pending_waiting_for_drain && gap_ms > 500 {
                        self.music.pending_waiting_for_drain = true;
                        self.music.pending_drain_started_at = Some(now);
                    }
                    if self.music.pending_waiting_for_drain {
                        let audio_buffer_empty = {
                            let guard = self.audio_send_state.lock();
                            guard
                                .as_ref()
                                .is_none_or(audio_pipeline::AudioSendState::tts_is_empty)
                        };
                        let drain_elapsed_ms = self.music.pending_drain_started_at.map_or(0, |started| {
                            now.duration_since(started).as_millis() as u64
                        });
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
                let total_wait_ms = self.music.pending_received_at.map_or(0, |received_at| {
                    now.duration_since(received_at).as_millis() as u64
                });
                let prepared_lead_ms = self.music.pending_first_pcm_at.map_or(0, |first_pcm_at| {
                    now.duration_since(first_pcm_at).as_millis() as u64
                });
                let committed_direct_url = self.music.pending_resolved_direct_url;
                self.music.clear_pending_start();
                self.music.finishing = false;
                self.music.active_url = Some(url.clone());
                self.music.active_resolved_direct_url = committed_direct_url;
                self.music.active = true;
                self.music.paused = false;

                {
                    let mut guard = self.audio_send_state.lock();
                    if let Some(ref mut state) = *guard {
                        state.begin_music_fade_in(1500);
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

        if self.music.active && !self.music.paused {
            while let Ok(chunk) = self.music_pcm_rx.try_recv() {
                let mut guard = self.audio_send_state.lock();
                if let Some(ref mut state) = *guard {
                    state.push_music_pcm(chunk);
                }
            }
        }

        if self.music.finishing && is_music_output_drained(&self.music_pcm_rx, &self.audio_send_state) {
            self.music.finishing = false;
            self.music.active = false;
            self.music.paused = false;
            self.music.active_url = None;
            self.music.active_resolved_direct_url = false;
            send_msg(&OutMsg::PlayerState {
                status: "idle".into(),
            });
            send_msg(&OutMsg::MusicIdle);
            emit_playback_armed("music_idle", &self.audio_send_state);
        }

        {
            let mut guard = self.audio_send_state.lock();
            if let Some(ref mut state) = *guard {
                if let Some(reached) = state.maybe_take_music_gain_reached() {
                    drop(guard);
                    send_msg(&OutMsg::MusicGainReached { gain: reached });
                }
            }
        }

        if self.music.pending_stop {
            let fade_done = {
                let guard = self.audio_send_state.lock();
                guard
                    .as_ref()
                    .is_none_or(audio_pipeline::AudioSendState::is_music_fade_out_complete)
            };
            if fade_done {
                self.music.reset();
                drain_music_pcm_queue(&self.music_pcm_rx);
                clear_audio_send_buffer(&self.audio_send_state);
                if self.tts_playback_buffered {
                    self.tts_playback_buffered = false;
                    send_tts_playback_state("idle", "music_track_finished");
                }
                send_msg(&OutMsg::PlayerState {
                    status: "idle".into(),
                });
                send_msg(&OutMsg::MusicIdle);
                emit_playback_armed("music_stop", &self.audio_send_state);
            }
        }

        self.buffer_depth_tick_counter += 1;
        if self.buffer_depth_tick_counter >= Self::BUFFER_DEPTH_REPORT_INTERVAL {
            self.buffer_depth_tick_counter = 0;
            let guard = self.audio_send_state.lock();
            if let Some(ref state) = *guard {
                let tts = state.tts_buffer_samples();
                let music = state.music_buffer_samples();
                if tts > 0 || music > 0 {
                    self.buffer_depth_was_nonempty = true;
                    drop(guard);
                    send_buffer_depth(tts, music, "periodic_nonempty");
                    if tts > 0 && !self.tts_playback_buffered {
                        self.tts_playback_buffered = true;
                        send_tts_playback_state("buffered", "periodic_nonempty");
                    }
                } else if self.buffer_depth_was_nonempty {
                    self.buffer_depth_was_nonempty = false;
                    drop(guard);
                    send_buffer_depth(0, 0, "periodic_drained");
                    if self.tts_playback_buffered {
                        self.tts_playback_buffered = false;
                        send_tts_playback_state("idle", "periodic_drained");
                    }
                }
            } else if self.buffer_depth_was_nonempty {
                self.buffer_depth_was_nonempty = false;
                drop(guard);
                send_buffer_depth(0, 0, "audio_send_state_missing");
                if self.tts_playback_buffered {
                    self.tts_playback_buffered = false;
                    send_tts_playback_state("idle", "audio_send_state_missing");
                }
            }
        }

        let opus_frame = {
            let mut guard = self.audio_send_state.lock();
            match *guard {
                Some(ref mut state) => state.next_opus_frame(),
                None => None,
            }
        };

        if let Some(opus) = opus_frame {
            let encrypted = {
                let mut guard = self.dave.lock();
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

            if let Some(ref conn) = self.voice_conn {
                if let Err(e) = conn.send_rtp_frame(&encrypted).await {
                    debug!("RTP send error: {}", e);
                }
            }
        }
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

    spawn_ipc_writer();

    // Spawn stdin reader with separate control/audio lanes so backpressure on the
    // main loop or stdout path does not wedge the parent's stdin pipe.
    let audio_debug = std::env::var("AUDIO_DEBUG").is_ok();
    let (mut ipc_control_rx, mut ipc_audio_rx) = spawn_ipc_reader(audio_debug);

    info!("Voice subprocess started, waiting for IPC messages");

    let dave: Arc<Mutex<Option<DaveManager>>> = Arc::new(Mutex::new(None));
    let (voice_event_tx, mut voice_event_rx) = mpsc::channel::<VoiceEvent>(256);

    let audio_send_state = Arc::new(Mutex::new(None::<AudioSendState>));
    let mut send_interval = time::interval(Duration::from_millis(20));
    send_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    send_interval.tick().await; // consume first immediate tick

    let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(500);
    let (music_event_tx, mut music_event_rx) = mpsc::channel::<MusicEvent>(32);
    let (asr_exit_tx, mut asr_exit_rx) = mpsc::channel::<(u64, u64, String)>(32);

    let mut state = AppState::new(
        dave,
        voice_event_tx,
        audio_send_state,
        music_pcm_tx,
        music_pcm_rx,
        music_event_tx,
        asr_exit_tx,
    );

    loop {
        tokio::select! {
            msg = async {
                tokio::select! {
                    biased;
                    Some(msg) = ipc_control_rx.recv() => Some(msg),
                    Some(msg) = ipc_audio_rx.recv() => Some(msg),
                    else => None,
                }
            } => {
                let Some(msg) = msg else {
                    break;
                };
                if state.handle_ipc_msg(msg).await {
                    break;
                }
            }

            Some(event) = voice_event_rx.recv() => {
                state.handle_voice_event(event);
            }

            Some(event) = music_event_rx.recv() => {
                state.handle_music_event(event);
            }

            Some((uid, session_id, reason)) = asr_exit_rx.recv() => {
                state.handle_asr_exit(uid, session_id, reason);
            }

            () = async {
                if let Some(deadline) = state.reconnect_deadline {
                    time::sleep_until(deadline).await;
                } else {
                    future::pending::<()>().await;
                }
            }, if state.reconnect_deadline.is_some() => {
                state.handle_reconnect_timer().await;
            }

            _ = send_interval.tick() => {
                state.on_audio_tick().await;
            }
        }
    }

    info!("Shutting down");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TryConnectOutcome {
    AlreadyConnected,
    MissingData,
    Connected,
    Failed,
}

/// Attempt to establish the voice connection once we have all required info.
async fn try_connect(
    pending: &PendingConnection,
    guild_id: Option<u64>,
    channel_id: Option<u64>,
    event_tx: &mpsc::Sender<VoiceEvent>,
    dave: &Arc<Mutex<Option<DaveManager>>>,
    voice_conn: &mut Option<VoiceConnection>,
) -> TryConnectOutcome {
    if voice_conn.is_some() {
        return TryConnectOutcome::AlreadyConnected;
    }
    let Some(gid) = guild_id else {
        return TryConnectOutcome::MissingData;
    };
    let Some(cid) = channel_id else {
        return TryConnectOutcome::MissingData;
    };
    let Some(uid) = pending.user_id else {
        return TryConnectOutcome::MissingData;
    };
    let Some(endpoint) = pending.endpoint.as_deref() else {
        return TryConnectOutcome::MissingData;
    };
    let Some(session_id) = pending.session_id.as_deref() else {
        return TryConnectOutcome::MissingData;
    };
    let Some(token) = pending.token.as_deref() else {
        return TryConnectOutcome::MissingData;
    };

    info!(
        "Connecting to voice: endpoint={:?} guild={} channel={} user={}",
        pending.endpoint, gid, cid, uid
    );

    match VoiceConnection::connect(
        VoiceConnectionParams {
            endpoint,
            guild_id: gid,
            user_id: uid,
            session_id,
            token,
            channel_id: cid,
        },
        event_tx.clone(),
        dave.clone(),
    )
    .await
    {
        Ok(conn) => {
            *voice_conn = Some(conn);
            TryConnectOutcome::Connected
        }
        Err(e) => {
                        error!("Voice connection failed: {e}");
            send_error(
                ErrorCode::VoiceConnectFailed,
                format!("Voice connect failed: {e}"),
            );
            TryConnectOutcome::Failed
        }
    }
}

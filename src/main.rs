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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
#[allow(clippy::too_many_lines)] // Splitting into sub-functions would require passing 15+ mutable refs.
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

    // ---- State ----
    let mut pending_conn = PendingConnection::default();
    let mut guild_id: Option<u64> = None;
    let mut channel_id: Option<u64> = None;
    let mut self_mute = false;
    let mut reconnect_deadline: Option<time::Instant> = None;
    let mut reconnect_attempt: u32 = 0;

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
    let mut music = MusicState::default();

    // Per-SSRC Opus decoders for inbound stereo 48kHz user audio.
    let mut opus_decoders: HashMap<u32, OpusDecoder> = HashMap::new();

    // SSRC → user_id mapping (mirrored from voice_conn's Speaking events)
    let mut ssrc_map: HashMap<u32, u64> = HashMap::new();
    let mut self_user_id: Option<u64> = None;
    let mut user_capture_states: HashMap<u64, UserCaptureState> = HashMap::new();
    let mut asr_sessions: HashMap<u64, AsrSession> = HashMap::new();
    let (asr_exit_tx, mut asr_exit_rx) = mpsc::channel::<(u64, u64, String)>(32);
    let mut next_asr_session_id: u64 = 1;
    let mut speaking_states: HashMap<u64, SpeakingState> = HashMap::new();

    // Buffer depth reporting: emit every 25 ticks (500ms) while buffered, and
    // emit one final zero-depth update when playback drains so the main process
    // can clear its backlog state.
    let mut buffer_depth_tick_counter: u32 = 0;
    let mut buffer_depth_was_nonempty = false;
    let mut tts_playback_buffered = false;
    #[allow(clippy::items_after_statements)]
    const BUFFER_DEPTH_REPORT_INTERVAL: u32 = 25; // 25 × 20ms = 500ms

    loop {
        tokio::select! {
            // ---- IPC message from main process ----
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
                match msg {
                    InMsg::Join { guild_id: gid, channel_id: cid, _self_deaf: _, self_mute: sm } => {
                        let Ok(g) = gid.parse::<u64>() else {
                            send_error(
                                ErrorCode::InvalidRequest,
                                format!("join requires a numeric guild_id, got {gid:?}"),
                            );
                            continue;
                        };
                        let Ok(c) = cid.parse::<u64>() else {
                            send_error(
                                ErrorCode::InvalidRequest,
                                format!("join requires a numeric channel_id, got {cid:?}"),
                            );
                            continue;
                        };
                        guild_id = Some(g);
                        channel_id = Some(c);
                        self_mute = sm;
                        reconnect_deadline = None;
                        reconnect_attempt = 0;

                        send_gateway_voice_state_update(g, c, sm);
                        info!("Join requested guild={} channel={}, sent OP4", g, c);
                    }

                    InMsg::VoiceServer { data } => {
                        let ep = data.endpoint.clone();
                        let has_token = data.token.is_some();
                        info!(
                            "IPC voice_server: endpoint={:?} token={} connected={}",
                            ep, if has_token { "present" } else { "missing" }, voice_conn.is_some()
                        );
                        if let Some(ref e) = ep {
                            pending_conn.endpoint = Some(e.clone());
                        }
                        if let Some(tk) = data.token.as_deref() {
                            pending_conn.token = Some(tk.to_string());
                        }
                        match try_connect(
                            &pending_conn,
                            guild_id,
                            channel_id,
                            &voice_event_tx,
                            &dave,
                            &mut voice_conn,
                        )
                        .await
                        {
                            TryConnectOutcome::Connected => {
                                reconnect_deadline = None;
                                reconnect_attempt = 0;
                            }
                            TryConnectOutcome::Failed => {
                                schedule_reconnect(
                                    &mut reconnect_deadline,
                                    &mut reconnect_attempt,
                                    guild_id,
                                    channel_id,
                                    self_mute,
                                    "voice_server_connect_failed",
                                );
                            }
                            _ => {}
                        }
                    }

                    InMsg::VoiceState { data } => {
                        let new_session_id = data.session_id.clone();
                        let old_session_id = pending_conn.session_id.clone();
                        let new_user_id = match data.user_id.as_deref() {
                            Some(user_id) => parse_user_id_field(user_id, "voice_state"),
                            None => None,
                        };
                        let new_channel = data.channel_id.clone();
                        info!(
                            "IPC voice_state: session_id={:?} prev_session_id={:?} channel_id={:?} user_id={:?} connected={}",
                            new_session_id, old_session_id, new_channel, new_user_id, voice_conn.is_some()
                        );

                        if let Some(ref sid) = new_session_id {
                            // Detect session credential refresh while already connected
                            if voice_conn.is_some() && old_session_id.as_deref() != Some(sid.as_str()) {
                                warn!(
                                    "Session ID changed while connected: {:?} -> {:?}, tearing down for reconnect",
                                    old_session_id, new_session_id
                                );
                                if let Some(ref conn) = voice_conn {
                                    conn.shutdown();
                                }
                                voice_conn = None;
                                *audio_send_state.lock() = None;
                            }
                            pending_conn.session_id = Some(sid.clone());
                        }
                        if let Some(uid) = new_user_id {
                            pending_conn.user_id = Some(uid);
                            self_user_id = Some(uid);
                        }
                        match try_connect(
                            &pending_conn,
                            guild_id,
                            channel_id,
                            &voice_event_tx,
                            &dave,
                            &mut voice_conn,
                        )
                        .await
                        {
                            TryConnectOutcome::Connected => {
                                reconnect_deadline = None;
                                reconnect_attempt = 0;
                            }
                            TryConnectOutcome::Failed => {
                                schedule_reconnect(
                                    &mut reconnect_deadline,
                                    &mut reconnect_attempt,
                                    guild_id,
                                    channel_id,
                                    self_mute,
                                    "voice_state_connect_failed",
                                );
                            }
                            _ => {}
                        }
                    }

                    InMsg::Audio { pcm_base64, sample_rate } => {
                        let now = time::Instant::now();
                        if music.pending_url.is_some() {
                            music.pending_audio_seen = true;
                            music.pending_last_audio_at = Some(now);
                        }

                        // When music is playing at full volume (not ducked), drop TTS.
                        // When music is ducked (gain < 1.0), allow TTS through for mixing.
                        if music.active && !music.paused {
                            let is_ducked = {
                                let guard = audio_send_state.lock();
                                guard.as_ref().is_some_and(audio_pipeline::AudioSendState::is_music_ducked)
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
                                    send_tts_playback_state("buffered", "tts_pcm_enqueued");
                                }
                            }
                        }
                    }

                    InMsg::StopPlayback => {
                        music.reset();
                        drain_music_pcm_queue(&music_pcm_rx);
                        clear_audio_send_buffer(&audio_send_state);
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle", "stop_playback");
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
                            send_tts_playback_state("idle", "stop_tts_playback");
                        }
                    }

                    InMsg::SubscribeUser {
                        user_id,
                        silence_duration_ms,
                        sample_rate,
                    } => {
                        let Some(uid) = parse_user_id_field(&user_id, "subscribe_user") else {
                            continue;
                        };
                        let normalized_sample_rate = normalize_sample_rate(sample_rate);
                        let normalized_silence_duration_ms =
                            normalize_silence_duration_ms(silence_duration_ms);
                        let state = user_capture_states.entry(uid).or_insert_with(|| {
                            UserCaptureState::new(
                                normalized_sample_rate,
                                normalized_silence_duration_ms,
                            )
                        });
                        state.sample_rate = normalized_sample_rate;
                        state.silence_duration_ms = normalized_silence_duration_ms;
                    }

                    InMsg::UnsubscribeUser { user_id } => {
                        let Some(uid) = parse_user_id_field(&user_id, "unsubscribe_user") else {
                            continue;
                        };
                        if let Some(state) = user_capture_states.remove(&uid) {
                            if state.stream_active {
                                send_msg(&OutMsg::UserAudioEnd {
                                    user_id: uid.to_string(),
                                });
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

                        music.queue_pending_start(normalized_url.clone(), resolved_direct_url);
                        start_music_pipeline(
                            MusicPipelineRequest {
                                url: &normalized_url,
                                resolved_direct_url,
                                clear_output_buffers: false,
                            },
                            MusicPipelineContext {
                                music_player: &mut music.player,
                                music_pcm_rx: &music_pcm_rx,
                                music_pcm_tx: &music_pcm_tx,
                                music_event_tx: &music_event_tx,
                                audio_send_state: &audio_send_state,
                            },
                        );
                        info!(
                            "music_play queued pending start url={} direct={} (waiting for announcement drain)",
                            normalized_url,
                            resolved_direct_url
                        );
                    }

                    InMsg::MusicStop => {
                        if music.player.is_some() && music.active {
                            // Start a quick fade-out; actual stop happens in the
                            // 20ms tick when the fade completes.
                            let mut guard = audio_send_state.lock();
                            if let Some(ref mut state) = *guard {
                                let _ = state.set_music_gain(0.0, 300);
                            }
                            music.pending_stop = true;
                        } else {
                            // No active music, stop immediately
                            music.reset();
                            drain_music_pcm_queue(&music_pcm_rx);
                            clear_audio_send_buffer(&audio_send_state);
                            if tts_playback_buffered {
                                tts_playback_buffered = false;
                                send_tts_playback_state("idle", "music_stop");
                            }
                            send_msg(&OutMsg::PlayerState {
                                status: "idle".into(),
                            });
                            send_msg(&OutMsg::MusicIdle);
                            emit_playback_armed("music_stop", &audio_send_state);
                        }
                    }

                    InMsg::MusicPause => {
                        music.clear_pending_start();
                        music.pending_stop = false;
                        let was_finishing = music.finishing;
                        music.finishing = false;
                        if music.player.is_some() || music.active || was_finishing {
                            music.paused = true;
                            music.active = false;
                            clear_audio_send_buffer(&audio_send_state);
                            send_msg(&OutMsg::PlayerState {
                                status: "paused".into(),
                            });
                            emit_playback_armed("music_pause", &audio_send_state);
                        }
                    }

                    InMsg::MusicResume => {
                        music.clear_pending_start();
                        music.pending_stop = false;
                        music.finishing = false;

                        if music.player.is_some() {
                            music.paused = false;
                            music.active = true;
                            send_msg(&OutMsg::PlayerState {
                                status: "playing".into(),
                            });
                        } else if let Some(url) = music.active_url.clone() {
                            start_music_pipeline(
                                MusicPipelineRequest {
                                    url: &url,
                                    resolved_direct_url: music.active_resolved_direct_url,
                                    clear_output_buffers: true,
                                },
                                MusicPipelineContext {
                                    music_player: &mut music.player,
                                    music_pcm_rx: &music_pcm_rx,
                                    music_pcm_tx: &music_pcm_tx,
                                    music_event_tx: &music_event_tx,
                                    audio_send_state: &audio_send_state,
                                },
                            );
                            music.paused = false;
                            music.active = true;
                            send_msg(&OutMsg::PlayerState {
                                status: "playing".into(),
                            });
                        }
                    }

                    InMsg::MusicSetGain { target, fade_ms } => {
                        let clamped = target.clamp(0.0, 1.0);
                        let mut guard = audio_send_state.lock();
                        if let Some(ref mut state) = *guard {
                            if let Some(reached) = state.set_music_gain(clamped, fade_ms) {
                                drop(guard);
                                send_msg(&OutMsg::MusicGainReached { gain: reached });
                            }
                        }
                    }

                    InMsg::ConnectAsr { user_id, api_key, model, language, prompt } => {
                        let Some(uid) = parse_user_id_field(&user_id, "connect_asr") else {
                            continue;
                        };

                        if let Some(session) = asr_sessions.remove(&uid) {
                            shutdown_asr_session(session);
                        }

                        let (asr_tx, asr_rx) = tokio::sync::mpsc::unbounded_channel();
                        let session_id = next_asr_session_id;
                        next_asr_session_id = next_asr_session_id.saturating_add(1);

                        let exit_tx = asr_exit_tx.clone();
                        let handle = tokio::spawn(async move {
                            let reason = match run_asr_client(user_id.clone(), api_key, model, language, prompt, asr_rx).await {
                                Ok(()) => "closed".to_string(),
                                Err(e) => {
                                    error!("ASR client {} exited: {}", user_id, e);
                                    format!("{e}")
                                }
                            };
                            let _ = exit_tx.send((uid, session_id, reason)).await;
                        });
                        asr_sessions.insert(
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
                            continue;
                        };
                        if let Some(session) = asr_sessions.remove(&uid) {
                            shutdown_asr_session(session);
                        }
                    }

                    InMsg::CommitAsr { user_id } => {
                        let Some(uid) = parse_user_id_field(&user_id, "commit_asr") else {
                            continue;
                        };
                        if let Some(session) = asr_sessions.get(&uid) {
                            let _ = session.tx.send(AsrCommand::Commit);
                        }
                    }

                    InMsg::ClearAsr { user_id } => {
                        let Some(uid) = parse_user_id_field(&user_id, "clear_asr") else {
                            continue;
                        };
                        if let Some(session) = asr_sessions.get(&uid) {
                            let _ = session.tx.send(AsrCommand::Clear);
                        }
                    }

                    InMsg::Destroy => {
                        music.stop_player();
                        for (_, session) in asr_sessions.drain() {
                            shutdown_asr_session(session);
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
                        reconnect_deadline = None;
                        reconnect_attempt = 0;
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
                        if ssrc_map.insert(ssrc, user_id) != Some(user_id) {
                            opus_decoders.remove(&ssrc);
                        }
                    }

                    VoiceEvent::ClientDisconnect { user_id } => {
                        if self_user_id == Some(user_id) {
                            continue;
                        }
                        // Remove from SSRC map
                        let removed_ssrcs = ssrc_map
                            .iter()
                            .filter_map(|(&ssrc, &mapped_uid)| (mapped_uid == user_id).then_some(ssrc))
                            .collect::<Vec<_>>();
                        ssrc_map.retain(|_, v| *v != user_id);
                        for ssrc in removed_ssrcs {
                            opus_decoders.remove(&ssrc);
                        }

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
                        if let Some(session) = asr_sessions.remove(&user_id) {
                            shutdown_asr_session(session);
                        }

                        send_msg(&OutMsg::ClientDisconnect { user_id: uid_str });
                    }

                    VoiceEvent::OpusReceived { ssrc, opus_frame } => {
                        let Some(&uid) = ssrc_map.get(&ssrc) else {
                            debug!("Dropped Opus frame from unknown ssrc: {ssrc}");
                            continue;
                        };
                        if self_user_id == Some(uid) {
                            continue; // skip bot's own audio
                        }

                        let Some(state) = user_capture_states.get(&uid) else {
                            continue;
                        };

                        let target_sample_rate = state.sample_rate;

                        // Opus decode → stereo i16 48kHz
                        let mut pcm_stereo = vec![0i16; 5760]; // max Opus frame
                        if let Entry::Vacant(entry) = opus_decoders.entry(ssrc) {
                            let decoder =
                                match OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo) {
                                    Ok(decoder) => decoder,
                                    Err(error) => {
                                        error!(
                                            "failed to init Opus decoder for ssrc={}: {:?}",
                                            ssrc, error
                                        );
                                        continue;
                                    }
                                };
                            entry.insert(decoder);
                        }

                        let decode_result = {
                            let packet = match OpusPacket::try_from(opus_frame.as_slice()) {
                                Ok(packet) => packet,
                                Err(error) => {
                                    debug!("Invalid Opus packet for ssrc={}: {:?}", ssrc, error);
                                    continue;
                                }
                            };
                            let signals = MutSignals::try_from(pcm_stereo.as_mut_slice())
                                .expect("non-empty signal buffer");
                            opus_decoders
                                .get_mut(&ssrc)
                                .expect("decoder inserted above")
                                .decode(Some(packet), signals, false)
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
                                    if let Some(session) = asr_sessions.get(&uid) {
                                        let _ = session.tx.send(AsrCommand::Audio(llm_pcm.clone()));
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
                        send_error(ErrorCode::VoiceRuntimeError, reason.clone());
                        send_msg(&OutMsg::ConnectionState { status: "disconnected".into() });
                        music.reset();
                        drain_music_pcm_queue(&music_pcm_rx);
                        if let Some(ref conn) = voice_conn {
                            conn.shutdown();
                        }
                        voice_conn = None;
                        *audio_send_state.lock() = None;
                        ssrc_map.clear();
                        opus_decoders.clear();
                        speaking_states.clear();
                        schedule_reconnect(
                            &mut reconnect_deadline,
                            &mut reconnect_attempt,
                            guild_id,
                            channel_id,
                            self_mute,
                            &reason,
                        );
                    }

                }
            }

            // ---- Music pipeline events (from worker thread) ----
            Some(event) = music_event_rx.recv() => {
                match event {
                    MusicEvent::Idle => {
                        music.stop_player();
                        music.paused = false;
                        music.finishing = music.active;
                        music.pending_stop = false;
                        music.clear_pending_start();
                        if !music.finishing {
                            music.active_url = None;
                            music.active_resolved_direct_url = false;
                            send_msg(&OutMsg::PlayerState {
                                status: "idle".into(),
                            });
                            send_msg(&OutMsg::MusicIdle);
                            emit_playback_armed("music_idle", &audio_send_state);
                        }
                    }
                    MusicEvent::Error(message) => {
                        music.reset();
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
                        music.pending_first_pcm_at = Some(now);
                        if let Some(received_at) = music.pending_received_at {
                            info!(
                                "music_play prepared url={} direct={} startupMs={} requestToFirstPcmMs={}",
                                music.pending_url.as_deref().unwrap_or("unknown"),
                                resolved_direct_url,
                                startup_ms,
                                now.duration_since(received_at).as_millis() as u64
                            );
                        } else {
                            info!(
                                "music_play prepared url={} direct={} startupMs={}",
                                music.pending_url.as_deref().unwrap_or("unknown"),
                                resolved_direct_url,
                                startup_ms
                            );
                        }
                    }
                }
            }

            // ---- ASR task exit notification ----
            Some((uid, session_id, reason)) = asr_exit_rx.recv() => {
                let is_current_session = asr_sessions
                    .get(&uid)
                    .is_some_and(|session| session.session_id == session_id);

                if is_current_session {
                    asr_sessions.remove(&uid);
                    send_msg(&OutMsg::AsrDisconnected {
                        user_id: uid.to_string(),
                        reason,
                    });
                }
            }

            () = async {
                if let Some(deadline) = reconnect_deadline {
                    time::sleep_until(deadline).await;
                } else {
                    future::pending::<()>().await;
                }
            }, if reconnect_deadline.is_some() => {
                reconnect_deadline = None;
                let outcome = try_connect(
                    &pending_conn,
                    guild_id,
                    channel_id,
                    &voice_event_tx,
                    &dave,
                    &mut voice_conn,
                ).await;

                match outcome {
                    TryConnectOutcome::Connected | TryConnectOutcome::AlreadyConnected => {
                        reconnect_attempt = 0;
                    }
                    TryConnectOutcome::Failed | TryConnectOutcome::MissingData => {
                        schedule_reconnect(
                            &mut reconnect_deadline,
                            &mut reconnect_attempt,
                            guild_id,
                            channel_id,
                            self_mute,
                            "reconnect_retry",
                        );
                    }
                }
            }

            // ---- 20ms audio send tick ----
            _ = send_interval.tick() => {
                let now = time::Instant::now();

                // Audio-driven speaking timeout: emit SpeakingEnd after SPEAKING_TIMEOUT_MS
                // of no UDP audio packets from a user.
                let mut speaking_ended_users: Vec<u64> = Vec::new();
                for (&user_id, ss) in &mut speaking_states {
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
                for (user_id, state) in &mut user_capture_states {
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
                if let Some(url) = music.pending_url.clone() {
                    let mut start_music = false;
                    let mut reason = "pending_unknown";
                    if let Some(received_at) = music.pending_received_at {
                        let elapsed_ms = now.duration_since(received_at).as_millis() as u64;
                        if elapsed_ms > 15_000 {
                            start_music = true;
                            reason = "pending_safety_timeout";
                        } else if !music.pending_audio_seen {
                            if elapsed_ms > 5_000 {
                                start_music = true;
                                reason = "pending_no_announcement_audio";
                            }
                        } else {
                            let last_audio_at =
                                music.pending_last_audio_at.unwrap_or(received_at);
                            let gap_ms = now.duration_since(last_audio_at).as_millis() as u64;
                            if !music.pending_waiting_for_drain && gap_ms > 500 {
                                music.pending_waiting_for_drain = true;
                                music.pending_drain_started_at = Some(now);
                            }
                            if music.pending_waiting_for_drain {
                                let audio_buffer_empty = {
                                    let guard = audio_send_state.lock();
                                    guard
                                        .as_ref()
                                        .is_none_or(audio_pipeline::AudioSendState::tts_is_empty)
                                };
                                let drain_elapsed_ms = music.pending_drain_started_at
                                    .map_or(0, |started| now.duration_since(started).as_millis() as u64);
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
                        let total_wait_ms = music
                            .pending_received_at
                            .map_or(0, |received_at| now.duration_since(received_at).as_millis() as u64);
                        let prepared_lead_ms = music
                            .pending_first_pcm_at
                            .map_or(0, |first_pcm_at| now.duration_since(first_pcm_at).as_millis() as u64);
                        let committed_direct_url = music.pending_resolved_direct_url;
                        music.clear_pending_start();
                        music.finishing = false;
                        music.active_url = Some(url.clone());
                        music.active_resolved_direct_url = committed_direct_url;
                        music.active = true;
                        music.paused = false;

                        // Fade in over 1.5s
                        {
                            let mut guard = audio_send_state.lock();
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

                // Drain music PCM into the dedicated music buffer only while active.
                if music.active && !music.paused {
                    while let Ok(chunk) = music_pcm_rx.try_recv() {
                        let mut guard = audio_send_state.lock();
                        if let Some(ref mut state) = *guard {
                            state.push_music_pcm(chunk);
                        }
                    }
                }

                if music.finishing && is_music_output_drained(&music_pcm_rx, &audio_send_state) {
                    music.finishing = false;
                    music.active = false;
                    music.paused = false;
                    music.active_url = None;
                    music.active_resolved_direct_url = false;
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
                        if let Some(reached) = state.maybe_take_music_gain_reached() {
                            drop(guard);
                            send_msg(&OutMsg::MusicGainReached { gain: reached });
                        }
                    }
                }

                // Deferred music stop: execute after fade-out completes.
                if music.pending_stop {
                    let fade_done = {
                        let guard = audio_send_state.lock();
                        guard.as_ref().is_none_or(audio_pipeline::AudioSendState::is_music_fade_out_complete)
                    };
                    if fade_done {
                        music.reset();
                        drain_music_pcm_queue(&music_pcm_rx);
                        clear_audio_send_buffer(&audio_send_state);
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle", "music_track_finished");
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
                            send_buffer_depth(tts, music, "periodic_nonempty");
                            if tts > 0 && !tts_playback_buffered {
                                tts_playback_buffered = true;
                                send_tts_playback_state("buffered", "periodic_nonempty");
                            }
                        } else if buffer_depth_was_nonempty {
                            buffer_depth_was_nonempty = false;
                            drop(guard);
                            send_buffer_depth(0, 0, "periodic_drained");
                            if tts_playback_buffered {
                                tts_playback_buffered = false;
                                send_tts_playback_state("idle", "periodic_drained");
                            }
                        }
                    } else if buffer_depth_was_nonempty {
                        buffer_depth_was_nonempty = false;
                        drop(guard);
                        send_buffer_depth(0, 0, "audio_send_state_missing");
                        if tts_playback_buffered {
                            tts_playback_buffered = false;
                            send_tts_playback_state("idle", "audio_send_state_missing");
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

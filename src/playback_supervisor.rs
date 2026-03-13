use base64::Engine as _;
use tokio::time;
use tracing::{info, warn};

use crate::app_state::AppState;
use crate::audio_pipeline::{
    clear_audio_send_buffer, clear_music_send_buffer, clear_tts_send_buffer,
    convert_llm_to_48k_mono, emit_playback_armed, has_buffered_music_output, resume_music_output,
    suppress_music_output,
};
use crate::ipc::{OutMsg, send_buffer_depth, send_msg, send_tts_playback_state};
use crate::ipc_protocol::PlaybackCommand;
use crate::music::{MusicEvent, drain_music_pcm_queue, is_music_output_drained};

impl AppState {
    pub(crate) fn handle_playback_command(&mut self, msg: PlaybackCommand) -> bool {
        match msg {
            PlaybackCommand::Audio {
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
                            .is_some_and(crate::audio_pipeline::AudioSendState::is_music_ducked)
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
                false
            }
            PlaybackCommand::StopPlayback => {
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
                false
            }
            PlaybackCommand::StopTtsPlayback => {
                clear_tts_send_buffer(&self.audio_send_state);
                if self.tts_playback_buffered {
                    self.tts_playback_buffered = false;
                    send_tts_playback_state("idle", "stop_tts_playback");
                }
                false
            }
            PlaybackCommand::MusicPlay {
                url,
                resolved_direct_url,
                visualizer_mode,
            } => {
                let normalized_url = url.trim().to_string();
                if normalized_url.is_empty() {
                    send_msg(&OutMsg::MusicError {
                        message: "music_play missing url".to_string(),
                    });
                    return false;
                }

                clear_music_send_buffer(&self.audio_send_state);
                self.music.queue_pending_start(
                    normalized_url.clone(),
                    resolved_direct_url,
                    visualizer_mode.clone(),
                );
                self.start_music_pipeline(
                    &normalized_url,
                    resolved_direct_url,
                    false,
                    visualizer_mode.clone(),
                );
                tracing::info!(
                    "music_play queued pending start url={} direct={} visualizer={:?} (waiting for announcement drain)",
                    normalized_url,
                    resolved_direct_url,
                    visualizer_mode
                );
                false
            }
            PlaybackCommand::MusicStop => {
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
                false
            }
            PlaybackCommand::MusicPause => {
                self.music.clear_pending_start();
                self.music.pending_stop = false;
                let was_finishing = self.music.finishing;
                let player_alive = self
                    .music
                    .player
                    .as_ref()
                    .is_some_and(crate::music::MusicPlayer::is_alive);
                let buffered_music_output = !self.music_pcm_rx.is_empty()
                    || has_buffered_music_output(&self.audio_send_state);
                if player_alive || self.music.active || was_finishing {
                    info!(
                        "music_pause: player_alive={} active={} was_finishing={} buffered_output={}",
                        player_alive, self.music.active, was_finishing, buffered_music_output
                    );
                    if player_alive
                        && !self
                            .music
                            .player
                            .as_ref()
                            .is_some_and(crate::music::MusicPlayer::pause)
                    {
                        warn!("music_pause: failed to pause music process group");
                    }
                    self.music.paused = true;
                    self.music.active = false;
                    self.music.finishing =
                        was_finishing || (!player_alive && buffered_music_output);
                    suppress_music_output(&self.audio_send_state);
                    send_msg(&OutMsg::PlayerState {
                        status: "paused".into(),
                    });
                    emit_playback_armed("music_pause", &self.audio_send_state);
                }
                false
            }
            PlaybackCommand::MusicResume => {
                self.music.clear_pending_start();
                self.music.pending_stop = false;
                self.music.finishing = false;

                let player_alive = self
                    .music
                    .player
                    .as_ref()
                    .is_some_and(crate::music::MusicPlayer::is_alive);
                let resumed_in_place = player_alive
                    && self
                        .music
                        .player
                        .as_ref()
                        .is_some_and(crate::music::MusicPlayer::resume);
                let buffered_music_output = !self.music_pcm_rx.is_empty()
                    || has_buffered_music_output(&self.audio_send_state);

                if resumed_in_place {
                    info!("music_resume: player alive, resuming from position");
                    resume_music_output(&self.audio_send_state);
                    self.music.paused = false;
                    self.music.active = true;
                    send_msg(&OutMsg::PlayerState {
                        status: "playing".into(),
                    });
                } else if !player_alive && buffered_music_output {
                    info!(
                        "music_resume: player dead, resuming buffered output from current position"
                    );
                    resume_music_output(&self.audio_send_state);
                    self.music.paused = false;
                    self.music.active = true;
                    self.music.finishing = true;
                    send_msg(&OutMsg::PlayerState {
                        status: "playing".into(),
                    });
                } else if let Some(url) = self.music.active_url.clone() {
                    self.music.stop_player();
                    warn!(
                        "music_resume: player dead, restarting pipeline from url={}",
                        url
                    );
                    self.start_music_pipeline(
                        &url,
                        self.music.active_resolved_direct_url,
                        true,
                        self.music.active_visualizer_mode.clone(),
                    );
                    self.music.paused = false;
                    self.music.active = true;
                    send_msg(&OutMsg::PlayerState {
                        status: "playing".into(),
                    });
                } else {
                    warn!("music_resume: no player and no url, cannot resume");
                }
                false
            }
            PlaybackCommand::MusicSetGain { target, fade_ms } => {
                let clamped = target.clamp(0.0, 1.0);
                let mut guard = self.audio_send_state.lock();
                if let Some(ref mut state) = *guard {
                    if let Some(reached) = state.set_music_gain(clamped, fade_ms) {
                        drop(guard);
                        send_msg(&OutMsg::MusicGainReached { gain: reached });
                    }
                }
                false
            }
            PlaybackCommand::Destroy => {
                self.music.stop_player();
                self.stream_publish.reset();
                self.clear_voice_connection();
                self.clear_stream_publish_connection();
                true
            }
        }
    }

    pub(crate) fn handle_music_event(&mut self, event: MusicEvent) {
        match event {
            MusicEvent::Idle => {
                info!(
                    "music_event_idle: active={} paused={} finishing={}",
                    self.music.active, self.music.paused, self.music.finishing
                );
                self.music.stop_player();
                self.music.paused = false;
                self.music.finishing = self.music.active;
                self.music.pending_stop = false;
                self.music.clear_pending_start();
                if !self.music.finishing {
                    self.music.active_url = None;
                    self.music.active_resolved_direct_url = false;
                    self.music.active_visualizer_mode = None;
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
                    tracing::info!(
                        "music_play prepared url={} direct={} startupMs={} requestToFirstPcmMs={}",
                        self.music.pending_url.as_deref().unwrap_or("unknown"),
                        resolved_direct_url,
                        startup_ms,
                        now.duration_since(received_at).as_millis() as u64
                    );
                } else {
                    tracing::info!(
                        "music_play prepared url={} direct={} startupMs={}",
                        self.music.pending_url.as_deref().unwrap_or("unknown"),
                        resolved_direct_url,
                        startup_ms
                    );
                }
            }
        }
    }

    /// Evaluate the pending-music-start state machine: wait for announcement
    /// audio to arrive, then for a gap in TTS audio, then for the TTS buffer to
    /// drain, before committing the music start.  Various safety/timeout paths
    /// ensure music eventually starts even if the announcement never arrives or
    /// the buffer never drains.
    fn tick_pending_music_start(&mut self, now: time::Instant) {
        let Some(url) = self.music.pending_url.clone() else {
            return;
        };

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
                            .is_none_or(crate::audio_pipeline::AudioSendState::tts_is_empty)
                    };
                    let drain_elapsed_ms = self
                        .music
                        .pending_drain_started_at
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
            tracing::info!(
                "music_play committed url={} reason={} totalWaitMs={} preparedLeadMs={} direct={} visualizer={:?}",
                url,
                reason,
                total_wait_ms,
                prepared_lead_ms,
                committed_direct_url,
                self.music.active_visualizer_mode
            );
        }
    }

    /// Complete a pending stop once the music gain fade-out finishes: reset
    /// music state, drain queues, clear audio buffers, and notify the TS side.
    fn tick_pending_stop(&mut self) {
        if !self.music.pending_stop {
            return;
        }
        let fade_done = {
            let guard = self.audio_send_state.lock();
            guard
                .as_ref()
                .is_none_or(crate::audio_pipeline::AudioSendState::is_music_fade_out_complete)
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

    /// Periodically report TTS/music buffer depth to the TS side and
    /// synchronise the `tts_playback_buffered` flag with actual buffer state.
    fn tick_buffer_depth_report(&mut self) {
        self.buffer_depth_tick_counter += 1;
        if self.buffer_depth_tick_counter < Self::BUFFER_DEPTH_REPORT_INTERVAL {
            return;
        }
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

    pub(crate) async fn on_audio_tick(&mut self) {
        let now = time::Instant::now();
        self.on_capture_tick(now);
        self.drain_stream_publish_runtime_events();

        self.tick_pending_music_start(now);

        if self.music.active && !self.music.paused {
            let mut guard = self.audio_send_state.lock();
            if let Some(ref mut state) = *guard {
                while state.can_accept_music_chunk() {
                    let Ok(chunk) = self.music_pcm_rx.try_recv() else {
                        break;
                    };
                    state.push_music_pcm(chunk);
                }
            }
        }

        if self.music.finishing
            && is_music_output_drained(&self.music_pcm_rx, &self.audio_send_state)
        {
            self.music.finishing = false;
            self.music.active = false;
            self.music.paused = false;
            self.music.active_url = None;
            self.music.active_resolved_direct_url = false;
            self.music.active_visualizer_mode = None;
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

        self.tick_pending_stop();
        self.tick_buffer_depth_report();

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
                    Some(ref mut dave_manager) if dave_manager.is_ready() => {
                        dave_manager.encrypt_opus(&opus).unwrap_or_else(|error| {
                            tracing::debug!("DAVE encrypt fallback: {}", error);
                            opus.clone()
                        })
                    }
                    _ => opus,
                }
            };

            if let Some(ref conn) = self.voice_conn {
                if let Err(error) = conn.send_rtp_frame(&encrypted).await {
                    tracing::debug!("RTP send error: {}", error);
                }
            }
        }

        self.send_pending_stream_publish_frame().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel as crossbeam;
    use parking_lot::Mutex;
    use tokio::sync::mpsc;

    use super::AppState;
    use crate::audio_pipeline::{AUDIO_FRAME_SAMPLES, AudioSendState, MAX_MUSIC_BUFFER_SAMPLES};
    use crate::ipc_protocol::PlaybackCommand;
    use crate::stream_publish::{StreamPublishEvent, StreamPublishFrame};

    fn make_app_state_with_music_queue_capacity(queue_capacity: usize) -> AppState {
        let (_voice_event_tx, voice_event_rx) = mpsc::channel(4);
        drop(voice_event_rx);
        let (music_event_tx, music_event_rx) = mpsc::channel(4);
        drop(music_event_rx);

        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));
        let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(queue_capacity);
        let (stream_publish_frame_tx, stream_publish_frame_rx) =
            crossbeam::bounded::<StreamPublishFrame>(4);
        let (stream_publish_event_tx, stream_publish_event_rx) =
            crossbeam::bounded::<StreamPublishEvent>(4);

        AppState::new(
            Arc::new(Mutex::new(None)),
            _voice_event_tx,
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

    fn make_app_state() -> AppState {
        make_app_state_with_music_queue_capacity(4)
    }

    #[test]
    fn wake_word_pause_preserves_buffered_music_tail_when_player_is_dead() {
        let mut state = make_app_state();
        {
            let mut guard = state.audio_send_state.lock();
            let audio_state = guard.as_mut().expect("audio state");
            audio_state.push_music_pcm(vec![123; 960]);
        }
        state.music.active = true;
        state.music.finishing = true;
        state.music.active_url = Some("https://cdn.example.com/track.mp4".to_string());
        state.music.active_resolved_direct_url = true;

        assert!(!state.handle_playback_command(PlaybackCommand::MusicPause));
        assert!(state.music.paused);
        assert!(!state.music.active);
        assert!(state.music.finishing);
        {
            let guard = state.audio_send_state.lock();
            let audio_state = guard.as_ref().expect("audio state");
            assert_eq!(audio_state.music_buffer_samples(), 960);
            assert!(audio_state.is_music_output_suppressed());
        }

        assert!(!state.handle_playback_command(PlaybackCommand::MusicResume));
        assert!(
            state.music.player.is_none(),
            "buffered resume should not restart the pipeline"
        );
        assert!(!state.music.paused);
        assert!(state.music.active);
        assert!(state.music.finishing);
        {
            let mut guard = state.audio_send_state.lock();
            let audio_state = guard.as_mut().expect("audio state");
            assert!(!audio_state.is_music_output_suppressed());
            let frame = audio_state
                .next_opus_frame()
                .expect("buffered music should resume from preserved PCM");
            assert!(!frame.is_empty());
            assert_eq!(audio_state.music_buffer_samples(), 0);
        }
    }

    #[tokio::test]
    async fn on_audio_tick_caps_music_prefetch_to_live_window() {
        let mut state = make_app_state_with_music_queue_capacity(128);
        state.music.active = true;

        for _ in 0..128 {
            state
                .music_pcm_tx
                .send(vec![321; AUDIO_FRAME_SAMPLES])
                .expect("queue music chunk");
        }

        state.on_audio_tick().await;

        {
            let guard = state.audio_send_state.lock();
            let audio_state = guard.as_ref().expect("audio state");
            assert_eq!(
                audio_state.music_buffer_samples(),
                MAX_MUSIC_BUFFER_SAMPLES - AUDIO_FRAME_SAMPLES
            );
        }
        assert!(
            state.music_pcm_rx.len() > 0,
            "backpressure should leave upstream music PCM queued instead of draining the full track"
        );
    }
}

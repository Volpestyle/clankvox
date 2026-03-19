use std::collections::hash_map::Entry;
use std::collections::BTreeMap;

use audiopus::coder::Decoder as OpusDecoder;
use audiopus::packet::Packet as OpusPacket;
use audiopus::{Channels, MutSignals, SampleRate};
use base64::Engine as _;
use tokio::time;

use crate::app_state::AppState;
use crate::capture::{
    normalize_sample_rate, normalize_silence_duration_ms, SpeakingState, UserCaptureState,
    SPEAKING_TIMEOUT_MS,
};
use crate::ipc::{send_msg, OutMsg};
use crate::ipc_protocol::CaptureCommand;
use crate::video::{RemoteVideoState, UserVideoSubscription};
use crate::video_decoder::PersistentVideoDecoder;
use crate::voice_conn::{TransportRole, VoiceEvent};

/// Maximum number of lost RTP packets for which we attempt FEC/PLC recovery.
/// Gaps larger than this are likely DTX silence periods or reconnects —
/// concealment would produce garbage.
const MAX_RECOVERABLE_GAP: i16 = 5;

const FIRST_KEYFRAME_REASSERT_INTERVAL_MS: u64 = 2_000;
/// Interval between periodic PLI requests after the first keyframe has been
/// received.  With DAVE decrypt failures causing ~45-55% frame loss, the
/// H264 reference chain accumulates corruption quickly.  Aggressive PLI
/// ensures the decoder resyncs via IDR frames every 2 seconds.
const PERIODIC_KEYFRAME_PLI_INTERVAL_MS: u64 = 2_000;

/// Classification of an incoming RTP sequence number relative to the last
/// accepted packet for the same SSRC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RtpSeqClass {
    /// First packet for this SSRC — no history yet.
    First,
    /// Next expected sequence — no loss.
    Sequential,
    /// Forward gap: `lost_count` packets were skipped (1..=MAX_RECOVERABLE_GAP).
    ForwardLoss { lost_count: u16 },
    /// Forward gap too large to recover — likely DTX or reconnect.
    ForwardLarge,
    /// Duplicate of the last accepted packet.
    Duplicate,
    /// Stale / reordered — a packet older than the last accepted.
    Stale,
}

/// Classify an incoming RTP sequence relative to the last accepted one.
///
/// Uses signed distance (i16 cast of wrapping_sub) to correctly handle u16
/// wraparound. Positive distance = forward gap; negative = stale/reordered.
fn classify_rtp_sequence(prev_seq: Option<u16>, incoming: u16) -> RtpSeqClass {
    let Some(prev) = prev_seq else {
        return RtpSeqClass::First;
    };
    let expected = prev.wrapping_add(1);
    if incoming == expected {
        return RtpSeqClass::Sequential;
    }
    if incoming == prev {
        return RtpSeqClass::Duplicate;
    }
    // Signed distance: positive means the incoming packet is ahead of expected,
    // negative means it is behind (stale/reordered).
    let distance = incoming.wrapping_sub(expected) as i16;
    if distance > 0 && distance <= MAX_RECOVERABLE_GAP {
        RtpSeqClass::ForwardLoss {
            lost_count: distance as u16,
        }
    } else if distance > MAX_RECOVERABLE_GAP {
        RtpSeqClass::ForwardLarge
    } else {
        // distance <= 0 (or very large u16 wrapping → negative i16)
        RtpSeqClass::Stale
    }
}

fn update_speaking_state(
    speaking_states: &mut std::collections::HashMap<u64, SpeakingState>,
    user_id: u64,
    now: time::Instant,
) -> bool {
    let speaking = speaking_states.entry(user_id).or_insert(SpeakingState {
        last_packet_at: None,
        is_speaking: false,
    });
    speaking.last_packet_at = Some(now);
    if speaking.is_speaking {
        false
    } else {
        speaking.is_speaking = true;
        true
    }
}

fn should_reassert_sink_wants_for_waiting_keyframe(
    subscription: &mut UserVideoSubscription,
    keyframe: bool,
    now: time::Instant,
) -> bool {
    if keyframe {
        subscription.last_keyframe_forwarded_at = Some(now);
        subscription.last_sink_wants_reasserted_at = None;
        return false;
    }

    // Before first keyframe: request aggressively at 2s intervals
    let interval_ms = if subscription.last_keyframe_forwarded_at.is_some() {
        // After first keyframe: request periodically so the per-frame
        // decoder gets fresh independently-decodable keyframes for the
        // vision scanner.
        PERIODIC_KEYFRAME_PLI_INTERVAL_MS
    } else {
        FIRST_KEYFRAME_REASSERT_INTERVAL_MS
    };
    let reassert_interval = std::time::Duration::from_millis(interval_ms);
    match subscription.last_sink_wants_reasserted_at {
        Some(last_reasserted_at) if now.duration_since(last_reasserted_at) < reassert_interval => {
            false
        }
        _ => {
            subscription.last_sink_wants_reasserted_at = Some(now);
            true
        }
    }
}

impl AppState {
    fn emit_user_video_state(&self, user_id: u64, state: &RemoteVideoState) {
        let stream_ssrcs = state
            .streams
            .iter()
            .map(|stream| stream.ssrc)
            .collect::<Vec<_>>();
        let active_stream_count = state
            .streams
            .iter()
            .filter(|stream| stream.is_active())
            .count();
        tracing::info!(
            user_id,
            audio_ssrc = state.audio_ssrc,
            video_ssrc = state.video_ssrc,
            codec = ?state.codec.as_deref(),
            stream_count = state.streams.len(),
            active_stream_count,
            stream_ssrcs = ?stream_ssrcs,
            "clankvox_native_video_state_emitted"
        );
        send_msg(&OutMsg::UserVideoState {
            user_id: user_id.to_string(),
            audio_ssrc: state.audio_ssrc,
            video_ssrc: state.video_ssrc,
            codec: state.codec.clone(),
            streams: state.streams.clone(),
        });
    }

    fn emit_user_video_end(&self, user_id: u64, state: Option<&RemoteVideoState>) {
        let ssrc = state.and_then(|state| {
            state
                .video_ssrc
                .or_else(|| state.streams.first().map(|stream| stream.ssrc))
        });
        tracing::info!(
            user_id,
            ssrc = ssrc,
            had_cached_state = state.is_some(),
            "clankvox_native_video_end_emitted"
        );
        send_msg(&OutMsg::UserVideoEnd {
            user_id: user_id.to_string(),
            ssrc,
        });
    }

    fn remove_user_video_runtime_state(&mut self, user_id: u64) {
        let ended_state = self.remote_video_states.remove(&user_id);
        self.user_video_decoders.remove(&user_id);
        self.emit_user_video_end(user_id, ended_state.as_ref());
    }

    fn refresh_video_sink_wants(&self, reason: &str) {
        if self.voice_conn.is_none() && self.stream_watch_conn.is_none() {
            tracing::info!(
                reason = reason,
                subscribed_user_count = self.user_video_subscriptions.len(),
                remote_video_user_count = self.remote_video_states.len(),
                "clankvox_video_sink_wants_skipped_no_connection"
            );
            return;
        }

        // Collect all wants, partitioned by which transport should
        // carry them.  Screen-share SSRCs go to stream_watch (if
        // connected), everything else (webcam / unknown) to voice.
        let mut voice_wants: BTreeMap<u32, u8> = BTreeMap::new();
        let mut voice_pixels: BTreeMap<u32, f64> = BTreeMap::new();
        let mut sw_wants: BTreeMap<u32, u8> = BTreeMap::new();
        let mut sw_pixels: BTreeMap<u32, f64> = BTreeMap::new();
        let have_sw = self.stream_watch_conn.is_some();

        for (&user_id, remote_state) in &self.remote_video_states {
            // Mark all known SSRCs as "quality 0" (= don't send but
            // acknowledge existence) on the appropriate transport.
            for stream in &remote_state.streams {
                let is_screen = stream
                    .stream_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("screen"));
                if is_screen && have_sw {
                    sw_wants.entry(stream.ssrc).or_insert(0);
                } else {
                    voice_wants.entry(stream.ssrc).or_insert(0);
                }
            }
            if let Some(video_ssrc) = remote_state.video_ssrc {
                voice_wants.entry(video_ssrc).or_insert(0);
            }

            let Some(subscription) = self.user_video_subscriptions.get(&user_id) else {
                continue;
            };

            if let Some(stream) = remote_state.preferred_stream(subscription) {
                let is_screen = stream
                    .stream_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("screen"));
                let (wants, pixels) = if is_screen && have_sw {
                    (&mut sw_wants, &mut sw_pixels)
                } else {
                    (&mut voice_wants, &mut voice_pixels)
                };
                wants.insert(stream.ssrc, subscription.preferred_quality);
                if let Some(pixel_count) = subscription
                    .preferred_pixel_count
                    .or_else(|| stream.pixel_count_hint())
                {
                    pixels.insert(stream.ssrc, f64::from(pixel_count));
                }
            } else if let Some(video_ssrc) = remote_state.video_ssrc {
                voice_wants.insert(video_ssrc, subscription.preferred_quality);
                if let Some(pixel_count) = subscription.preferred_pixel_count {
                    voice_pixels.insert(video_ssrc, f64::from(pixel_count));
                }
            }
        }

        // Send wants to each transport that has entries.
        let voice_wants_vec = voice_wants.into_iter().collect::<Vec<_>>();
        let voice_pixels_vec = voice_pixels.into_iter().collect::<Vec<_>>();
        let sw_wants_vec = sw_wants.into_iter().collect::<Vec<_>>();
        let sw_pixels_vec = sw_pixels.into_iter().collect::<Vec<_>>();

        let total_wanted = voice_wants_vec.len() + sw_wants_vec.len();
        tracing::info!(
            reason = reason,
            subscribed_user_count = self.user_video_subscriptions.len(),
            remote_video_user_count = self.remote_video_states.len(),
            wanted_ssrc_count = total_wanted,
            wanted_streams = ?voice_wants_vec,
            sw_wanted_streams = ?sw_wants_vec,
            pixel_count_overrides = ?voice_pixels_vec,
            "clankvox_video_sink_wants_updated"
        );

        if !voice_wants_vec.is_empty() || !have_sw {
            if let Some(conn) = self.voice_conn.as_ref() {
                if let Err(error) =
                    conn.update_media_sink_wants(&voice_wants_vec, &voice_pixels_vec)
                {
                    tracing::warn!(reason = reason, error = %error, "failed to update voice media sink wants");
                }
            }
        }
        if !sw_wants_vec.is_empty() {
            if let Some(conn) = self.stream_watch_conn.as_ref() {
                if let Err(error) = conn.update_media_sink_wants(&sw_wants_vec, &sw_pixels_vec) {
                    tracing::warn!(reason = reason, error = %error, "failed to update stream_watch media sink wants");
                }
            }
        }
    }

    pub(crate) fn handle_capture_command(&mut self, msg: CaptureCommand) {
        match msg {
            CaptureCommand::SubscribeUser {
                user_id,
                silence_duration_ms,
                sample_rate,
            } => {
                let Some(user_id) =
                    crate::app_state::parse_user_id_field(&user_id, "subscribe_user")
                else {
                    return;
                };
                let sample_rate = normalize_sample_rate(sample_rate);
                let silence_duration_ms = normalize_silence_duration_ms(silence_duration_ms);
                let state = self
                    .user_capture_states
                    .entry(user_id)
                    .or_insert_with(|| UserCaptureState::new(sample_rate, silence_duration_ms));
                state.sample_rate = sample_rate;
                state.silence_duration_ms = silence_duration_ms;
            }
            CaptureCommand::UnsubscribeUser { user_id } => {
                let Some(user_id) =
                    crate::app_state::parse_user_id_field(&user_id, "unsubscribe_user")
                else {
                    return;
                };
                if let Some(state) = self.user_capture_states.remove(&user_id) {
                    if state.stream_active {
                        send_msg(&OutMsg::UserAudioEnd {
                            user_id: user_id.to_string(),
                        });
                    }
                }
            }
            CaptureCommand::SubscribeUserVideo {
                user_id,
                max_frames_per_second,
                preferred_quality,
                preferred_pixel_count,
                preferred_stream_type,
                jpeg_quality,
            } => {
                let Some(user_id) =
                    crate::app_state::parse_user_id_field(&user_id, "subscribe_user_video")
                else {
                    return;
                };
                let subscription = UserVideoSubscription::new(
                    max_frames_per_second,
                    preferred_quality,
                    preferred_pixel_count,
                    preferred_stream_type,
                    jpeg_quality,
                );
                // Update JPEG quality on any existing decoder for this user
                if let Some(decoder) = self.user_video_decoders.get_mut(&user_id) {
                    decoder.set_jpeg_quality(subscription.jpeg_quality);
                }
                let had_cached_remote_state = self.remote_video_states.contains_key(&user_id);
                tracing::info!(
                    user_id,
                    max_frames_per_second = subscription.max_frames_per_second,
                    preferred_quality = subscription.preferred_quality,
                    preferred_pixel_count = subscription.preferred_pixel_count,
                    preferred_stream_type = ?subscription.preferred_stream_type.as_deref(),
                    had_cached_remote_state,
                    "clankvox_native_video_subscribe_requested"
                );
                self.user_video_subscriptions.insert(user_id, subscription);
                if let Some(state) = self.remote_video_states.get(&user_id) {
                    self.emit_user_video_state(user_id, state);
                }
                self.refresh_video_sink_wants("subscribe_user_video");
            }
            CaptureCommand::UnsubscribeUserVideo { user_id } => {
                let Some(user_id) =
                    crate::app_state::parse_user_id_field(&user_id, "unsubscribe_user_video")
                else {
                    return;
                };
                let had_subscription = self.user_video_subscriptions.remove(&user_id).is_some();
                tracing::info!(
                    user_id,
                    had_subscription,
                    "clankvox_native_video_unsubscribe_requested"
                );
                self.refresh_video_sink_wants("unsubscribe_user_video");
            }
        }
    }

    pub(crate) fn handle_voice_event(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::Ready { role, ssrc } => {
                tracing::info!(role = role.as_str(), ssrc, "Transport ready");
                match role {
                    TransportRole::Voice => {
                        self.reset_reconnect();
                        send_msg(&OutMsg::ConnectionState {
                            status: "ready".into(),
                        });
                        self.emit_transport_state(TransportRole::Voice, "ready", None);
                        send_msg(&OutMsg::Ready);

                        match crate::audio_pipeline::AudioSendState::new() {
                            Ok(state) => {
                                *self.audio_send_state.lock() = Some(state);
                                crate::audio_pipeline::emit_playback_armed(
                                    "connection_ready",
                                    &self.audio_send_state,
                                );
                            }
                            Err(error) => {
                                tracing::error!("Failed to init audio send state: {}", error)
                            }
                        }
                    }
                    TransportRole::StreamWatch => {
                        self.emit_transport_state(TransportRole::StreamWatch, "ready", None);
                    }
                    TransportRole::StreamPublish => {
                        self.emit_transport_state(TransportRole::StreamPublish, "ready", None);
                        self.maybe_start_stream_publish_pipeline();
                    }
                }
                self.refresh_video_sink_wants(match role {
                    TransportRole::Voice => "voice_ready",
                    TransportRole::StreamWatch => "stream_watch_ready",
                    TransportRole::StreamPublish => "stream_publish_ready",
                });
            }
            VoiceEvent::SsrcUpdate {
                role,
                ssrc,
                user_id,
            } => {
                if role == TransportRole::Voice
                    && self.ssrc_map.insert(ssrc, user_id) != Some(user_id)
                {
                    self.opus_decoders.remove(&ssrc);
                    self.last_rtp_seq.remove(&ssrc);
                }
            }
            VoiceEvent::VideoStateUpdate {
                role,
                user_id,
                audio_ssrc,
                video_ssrc,
                codec,
                streams,
            } => {
                // When a stream_watch transport exists, screen-share video
                // state arrives on that transport.  Voice-transport video
                // state updates for screen shares are redundant — but
                // webcam ("video") streams only appear on the voice
                // transport and must be allowed through.
                if role == TransportRole::Voice && self.stream_watch_conn.is_some() {
                    let has_non_screen_stream = streams.iter().any(|s| {
                        s.stream_type
                            .as_deref()
                            .is_some_and(|t| !t.eq_ignore_ascii_case("screen"))
                    });
                    if !has_non_screen_stream {
                        return;
                    }
                }
                if self.self_user_id == Some(user_id) {
                    return;
                }

                let previous = self.remote_video_states.get(&user_id).cloned();
                let clear_video_state = video_ssrc.is_none() && streams.is_empty();
                let incoming_stream_ssrcs =
                    streams.iter().map(|stream| stream.ssrc).collect::<Vec<_>>();
                let incoming_active_stream_count =
                    streams.iter().filter(|stream| stream.is_active()).count();
                let previous_stream_count = previous
                    .as_ref()
                    .map(|state| state.streams.len())
                    .unwrap_or_default();
                tracing::info!(
                    user_id,
                    clear_video_state,
                    audio_ssrc = audio_ssrc,
                    video_ssrc = video_ssrc,
                    codec = ?codec.as_deref(),
                    incoming_stream_count = streams.len(),
                    incoming_active_stream_count,
                    incoming_stream_ssrcs = ?incoming_stream_ssrcs,
                    previous_stream_count,
                    "clankvox_native_video_state_received"
                );
                let state = RemoteVideoState {
                    audio_ssrc: if clear_video_state {
                        None
                    } else {
                        audio_ssrc.or_else(|| previous.as_ref().and_then(|state| state.audio_ssrc))
                    },
                    video_ssrc: if clear_video_state {
                        None
                    } else {
                        video_ssrc.or_else(|| previous.as_ref().and_then(|state| state.video_ssrc))
                    },
                    codec: if clear_video_state {
                        None
                    } else {
                        codec.or_else(|| previous.as_ref().and_then(|state| state.codec.clone()))
                    },
                    streams: if clear_video_state {
                        Vec::new()
                    } else if streams.is_empty() {
                        previous
                            .as_ref()
                            .map(|state| state.streams.clone())
                            .unwrap_or_default()
                    } else {
                        streams
                    },
                };

                if state.has_streams() {
                    self.remote_video_states.insert(user_id, state.clone());
                    self.emit_user_video_state(user_id, &state);
                } else {
                    let ended_state = self.remote_video_states.remove(&user_id).or(previous);
                    self.emit_user_video_end(user_id, ended_state.as_ref());
                }

                self.refresh_video_sink_wants(match role {
                    TransportRole::Voice => "video_state_update",
                    TransportRole::StreamWatch => "stream_watch_video_state_update",
                    TransportRole::StreamPublish => "stream_publish_video_state_update",
                });
            }
            VoiceEvent::ClientDisconnect { role, user_id } => {
                if self.self_user_id != Some(user_id) {
                    match role {
                        TransportRole::Voice => self.remove_user_runtime_state(user_id),
                        TransportRole::StreamWatch => self.remove_user_video_runtime_state(user_id),
                        TransportRole::StreamPublish => {}
                    }
                    self.refresh_video_sink_wants(match role {
                        TransportRole::Voice => "client_disconnect",
                        TransportRole::StreamWatch => "stream_watch_client_disconnect",
                        TransportRole::StreamPublish => "stream_publish_client_disconnect",
                    });
                }
            }
            VoiceEvent::OpusReceived {
                role,
                ssrc,
                opus_frame,
                rtp_sequence,
            } => {
                if role != TransportRole::Voice {
                    return;
                }
                let Some(&user_id) = self.ssrc_map.get(&ssrc) else {
                    tracing::debug!("Dropped Opus frame from unknown ssrc: {ssrc}");
                    return;
                };
                if self.self_user_id == Some(user_id) {
                    return;
                }

                // --- RTP sequence classification ---
                let seq_class =
                    classify_rtp_sequence(self.last_rtp_seq.get(&ssrc).copied(), rtp_sequence);

                // Drop stale and duplicate packets — feeding them to the
                // decoder would corrupt its internal state and produce
                // out-of-order or doubled audio.  Speaking state is NOT
                // updated for these packets so that duplicates/reorders
                // cannot artificially stretch SpeakingStart/SpeakingEnd timing.
                match seq_class {
                    RtpSeqClass::Duplicate => {
                        tracing::debug!(ssrc, rtp_sequence, "Dropped duplicate RTP packet");
                        return;
                    }
                    RtpSeqClass::Stale => {
                        tracing::debug!(ssrc, rtp_sequence, "Dropped stale/reordered RTP packet");
                        return;
                    }
                    _ => {}
                }

                // Speaking state is updated only after duplicate/stale
                // filtering so that discarded packets cannot stretch
                // speaking activity.  This fires BEFORE the user_capture_states
                // gate so that the initial SpeakingStart reaches TypeScript and
                // triggers subscribe_user (bootstrap).
                if update_speaking_state(&mut self.speaking_states, user_id, time::Instant::now()) {
                    send_msg(&OutMsg::SpeakingStart {
                        user_id: user_id.to_string(),
                    });
                }

                // Gate audio decode/forwarding on subscription — only users
                // that TypeScript has subscribed via subscribe_user get their
                // Opus decoded and forwarded as UserAudio PCM.
                let Some(state) = self.user_capture_states.get(&user_id) else {
                    return;
                };
                let target_sample_rate = state.sample_rate;

                // Ensure an Opus decoder exists for this SSRC.
                if let Entry::Vacant(entry) = self.opus_decoders.entry(ssrc) {
                    let decoder = match OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo) {
                        Ok(decoder) => decoder,
                        Err(error) => {
                            tracing::error!(
                                "failed to init Opus decoder for ssrc={}: {:?}",
                                ssrc,
                                error
                            );
                            return;
                        }
                    };
                    entry.insert(decoder);
                }

                let decoder = self
                    .opus_decoders
                    .get_mut(&ssrc)
                    .expect("decoder inserted above");

                // Helper: convert decoded stereo PCM to LLM-ready output.
                let convert_frame = |decoded: &[i16], target_sample_rate: u32| {
                    crate::audio_pipeline::convert_decoded_to_llm(decoded, target_sample_rate)
                };

                // --- FEC / PLC for forward packet loss ---
                // Recovery frames are buffered (not emitted) until the
                // current anchor packet decodes successfully. If the anchor
                // fails, the recovery audio is discarded so we never emit
                // orphaned concealment frames without the real packet that
                // anchors them.
                let mut recovery_frames: Vec<(Vec<u8>, u16, usize, usize)> = Vec::new();
                if let RtpSeqClass::ForwardLoss { lost_count } = seq_class {
                    let plc_count = lost_count.saturating_sub(1) as usize;
                    if plc_count > 0 {
                        tracing::debug!(
                            ssrc,
                            lost_count,
                            plc_count,
                            "Opus PLC: synthesizing {plc_count} concealment frame(s)"
                        );
                    }
                    for _ in 0..plc_count {
                        let mut plc_buf = vec![0i16; 5760];
                        let plc_signals = MutSignals::try_from(plc_buf.as_mut_slice())
                            .expect("non-empty signal buffer");
                        if let Ok(plc_samples) = decoder.decode(None, plc_signals, false) {
                            let total = plc_samples * 2;
                            recovery_frames
                                .push(convert_frame(&plc_buf[..total], target_sample_rate));
                        }
                    }

                    // Recover the frame immediately before the current packet
                    // using in-band FEC.
                    let mut fec_buf = vec![0i16; 5760];
                    let fec_packet = match OpusPacket::try_from(opus_frame.as_slice()) {
                        Ok(p) => p,
                        Err(error) => {
                            tracing::debug!("Invalid Opus packet (FEC) ssrc={}: {:?}", ssrc, error);
                            return;
                        }
                    };
                    let fec_signals = MutSignals::try_from(fec_buf.as_mut_slice())
                        .expect("non-empty signal buffer");
                    if let Ok(fec_samples) = decoder.decode(Some(fec_packet), fec_signals, true) {
                        let total = fec_samples * 2;
                        recovery_frames.push(convert_frame(&fec_buf[..total], target_sample_rate));
                        tracing::debug!(ssrc, lost_count, "Opus FEC: recovered prior frame");
                    }
                }

                // --- Normal decode of the current packet ---
                let mut pcm_stereo = vec![0i16; 5760];
                let decode_result = {
                    let packet = match OpusPacket::try_from(opus_frame.as_slice()) {
                        Ok(packet) => packet,
                        Err(error) => {
                            tracing::debug!("Invalid Opus packet for ssrc={}: {:?}", ssrc, error);
                            return;
                        }
                    };
                    let signals = MutSignals::try_from(pcm_stereo.as_mut_slice())
                        .expect("non-empty signal buffer");
                    decoder.decode(Some(packet), signals, false)
                };

                match decode_result {
                    Ok(samples_per_channel) => {
                        // Anchor decode succeeded — emit any buffered recovery
                        // frames first (in chronological order), then the
                        // current packet.
                        for (pcm, peak, active, total) in recovery_frames {
                            if !pcm.is_empty() {
                                send_msg(&OutMsg::UserAudio {
                                    user_id: user_id.to_string(),
                                    pcm,
                                    signal_peak_abs: peak,
                                    signal_active_sample_count: active,
                                    signal_sample_count: total,
                                });
                            }
                        }

                        let total_samples = samples_per_channel * 2;
                        let (llm_pcm, peak, active, total) =
                            convert_frame(&pcm_stereo[..total_samples], target_sample_rate);
                        if !llm_pcm.is_empty() {
                            send_msg(&OutMsg::UserAudio {
                                user_id: user_id.to_string(),
                                pcm: llm_pcm,
                                signal_peak_abs: peak,
                                signal_active_sample_count: active,
                                signal_sample_count: total,
                            });
                        }

                        // Only advance the sequence tracker after a successful
                        // decode — failed decodes should not corrupt gap detection.
                        self.last_rtp_seq.insert(ssrc, rtp_sequence);

                        if let Some(state) = self.user_capture_states.get_mut(&user_id) {
                            state.touch_audio(time::Instant::now());
                        }
                    }
                    Err(error) => {
                        // Anchor decode failed — discard buffered recovery
                        // frames (they were decoded into the Opus decoder's
                        // state but we do not emit them without a valid anchor).
                        if !recovery_frames.is_empty() {
                            tracing::debug!(
                                ssrc,
                                rtp_sequence,
                                recovery_count = recovery_frames.len(),
                                "Opus anchor decode failed; discarding {count} recovery frame(s)",
                                count = recovery_frames.len()
                            );
                        }
                        tracing::debug!("Opus decode error for ssrc={}: {:?}", ssrc, error);
                    }
                }
            }
            VoiceEvent::VideoFrameReceived {
                role,
                user_id,
                ssrc,
                codec,
                keyframe,
                frame,
                rtp_timestamp,
                stream_type,
                rid,
                dave_decrypted,
            } => {
                // When a stream_watch transport is active, screen-share
                // video frames arrive on that transport.  Voice-transport
                // frames for screen shares would be duplicates.  But
                // webcam frames only arrive on the voice transport —
                // allow them through based on stream_type.
                if role == TransportRole::Voice && self.stream_watch_conn.is_some() {
                    let is_screen = stream_type
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case("screen"));
                    // Also allow through when stream_type is unknown (None)
                    // since webcam streams sometimes lack a type tag.
                    if is_screen {
                        return;
                    }
                }
                if self.self_user_id == Some(user_id) {
                    return;
                }

                if !self.user_video_subscriptions.contains_key(&user_id) {
                    return;
                }

                let is_h264 = codec.eq_ignore_ascii_case("h264");

                if is_h264 {
                    // ── Persistent H264 decode path ──
                    //
                    // Feed EVERY frame to the decoder so it maintains full
                    // reference-frame state.  Only rate-limit the JPEG
                    // emission over IPC.

                    // Read subscription values before taking mutable borrows
                    // on other AppState fields (avoids borrow conflicts).
                    let max_fps = self.user_video_subscriptions[&user_id].max_frames_per_second;
                    let sub_jpeg_quality = self.user_video_subscriptions[&user_id].jpeg_quality;

                    if let Some(subscription) = self.user_video_subscriptions.get_mut(&user_id) {
                        subscription.forwarded_frame_count =
                            subscription.forwarded_frame_count.saturating_add(1);
                        if subscription.forwarded_frame_count == 1 {
                            tracing::info!(
                                user_id,
                                ssrc,
                                codec = %codec,
                                keyframe,
                                frame_bytes = frame.len(),
                                rtp_timestamp,
                                stream_type = ?stream_type.as_deref(),
                                rid = ?rid.as_deref(),
                                max_frames_per_second = max_fps,
                                "clankvox_first_video_frame_forwarded"
                            );
                        }
                    }

                    // Lazily create or retrieve the decoder.  If init fails,
                    // skip H264 decode for this user instead of panicking.
                    //
                    // Decode + extract scalar state inside a scoped block so
                    // the mutable borrow on `user_video_decoders` is released
                    // before we call `self.refresh_video_sink_wants()` etc.
                    let (decoded, needs_pli, frames_decoded) = {
                        let decoder = match self.user_video_decoders.entry(user_id) {
                            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                match PersistentVideoDecoder::new() {
                                    Ok(mut d) => {
                                        d.set_jpeg_quality(sub_jpeg_quality);
                                        tracing::info!(
                                            user_id,
                                            ssrc,
                                            jpeg_quality = sub_jpeg_quality,
                                            "clankvox_persistent_h264_decoder_created"
                                        );
                                        entry.insert(d)
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            user_id,
                                            error = %e,
                                            "clankvox_persistent_h264_decoder_init_failed"
                                        );
                                        return;
                                    }
                                }
                            }
                        };

                        let decoded = decoder.decode_frame(&frame);
                        let needs_pli = decoder.take_pending_pli();
                        let frames_decoded = decoder.frames_decoded();
                        (decoded, needs_pli, frames_decoded)
                    };

                    // If the decoder was reset after sustained errors, it
                    // needs a fresh keyframe.  Send PLI once.
                    if needs_pli {
                        tracing::info!(user_id, ssrc, "clankvox_decoder_reset_requesting_pli");
                        self.refresh_video_sink_wants("decoder_reset_pli");
                        if let Some(conn) = self.video_conn() {
                            if let Err(error) = conn.send_rtcp_pli(ssrc) {
                                tracing::warn!(
                                    ssrc,
                                    error = %error,
                                    "clankvox_decoder_reset_pli_failed"
                                );
                            }
                        }
                    }

                    // Rate-limit JPEG emission — only send over IPC at the
                    // configured FPS.  The decoder still ingested every frame
                    // above so inter-frame state is intact.
                    let Some(decoded) = decoded else {
                        return;
                    };

                    let now = time::Instant::now();
                    let min_gap =
                        std::time::Duration::from_secs_f64(1.0 / f64::from(max_fps.max(1)));
                    let should_emit =
                        if let Some(subscription) = self.user_video_subscriptions.get(&user_id) {
                            match subscription.last_frame_sent_at {
                                Some(last) => now.duration_since(last) >= min_gap,
                                None => true,
                            }
                        } else {
                            return;
                        };
                    if !should_emit {
                        return;
                    }
                    if let Some(subscription) = self.user_video_subscriptions.get_mut(&user_id) {
                        subscription.last_frame_sent_at = Some(now);
                    }

                    if frames_decoded == 1 {
                        tracing::info!(
                            user_id,
                            ssrc,
                            width = decoded.width,
                            height = decoded.height,
                            jpeg_bytes = decoded.jpeg_data.len(),
                            change_score = decoded.change_score,
                            "clankvox_first_h264_frame_decoded"
                        );
                    }

                    // Periodic change-score logging for threshold tuning
                    // (every 60th frame ≈ 30 s at 2 fps).
                    if frames_decoded % 60 == 0 {
                        tracing::debug!(
                            user_id,
                            ssrc,
                            frames_decoded,
                            change_score = %format!("{:.4}", decoded.change_score),
                            ema_change_score = %format!("{:.4}", decoded.ema_change_score),
                            is_scene_cut = decoded.is_scene_cut,
                            "clankvox_frame_diff_periodic"
                        );
                    }

                    let jpeg_base64 =
                        base64::engine::general_purpose::STANDARD.encode(&decoded.jpeg_data);
                    send_msg(&OutMsg::DecodedVideoFrame {
                        user_id: user_id.to_string(),
                        ssrc,
                        width: decoded.width,
                        height: decoded.height,
                        jpeg_base64,
                        rtp_timestamp,
                        stream_type,
                        rid,
                        change_score: decoded.change_score,
                        ema_change_score: decoded.ema_change_score,
                        is_scene_cut: decoded.is_scene_cut,
                    });
                } else {
                    // ── Non-H264 (VP8): forward raw frame for TS-side ffmpeg decode ──
                    let Some(subscription) = self.user_video_subscriptions.get_mut(&user_id) else {
                        return;
                    };

                    let now = time::Instant::now();
                    let min_gap = std::time::Duration::from_secs_f64(
                        1.0 / f64::from(subscription.max_frames_per_second.max(1)),
                    );
                    if let Some(last_frame_sent_at) = subscription.last_frame_sent_at {
                        if now.duration_since(last_frame_sent_at) < min_gap && !keyframe {
                            return;
                        }
                    }
                    subscription.last_frame_sent_at = Some(now);

                    subscription.forwarded_frame_count =
                        subscription.forwarded_frame_count.saturating_add(1);
                    if subscription.forwarded_frame_count == 1 {
                        tracing::info!(
                            user_id,
                            ssrc,
                            codec = %codec,
                            keyframe,
                            frame_bytes = frame.len(),
                            rtp_timestamp,
                            stream_type = ?stream_type.as_deref(),
                            rid = ?rid.as_deref(),
                            max_frames_per_second = subscription.max_frames_per_second,
                            "clankvox_first_video_frame_forwarded"
                        );
                    }

                    let should_reassert_sink_wants =
                        should_reassert_sink_wants_for_waiting_keyframe(
                            subscription,
                            keyframe,
                            now,
                        );

                    let frame_base64 = base64::engine::general_purpose::STANDARD.encode(frame);
                    send_msg(&OutMsg::UserVideoFrame {
                        user_id: user_id.to_string(),
                        ssrc,
                        codec,
                        keyframe,
                        frame_base64,
                        rtp_timestamp,
                        stream_type: stream_type.clone(),
                        rid: rid.clone(),
                        dave_decrypted,
                    });
                    if should_reassert_sink_wants {
                        tracing::info!(
                            user_id,
                            ssrc,
                            forwarded_frame_count = subscription.forwarded_frame_count,
                            "clankvox_waiting_for_first_keyframe_reasserting_sink_wants"
                        );
                        self.refresh_video_sink_wants("waiting_for_first_keyframe");
                        if let Some(conn) = self.video_conn() {
                            if let Err(error) = conn.send_rtcp_pli(ssrc) {
                                tracing::warn!(
                                    ssrc,
                                    error = %error,
                                    "clankvox_rtcp_pli_failed"
                                );
                            }
                        }
                    }
                }
            }
            VoiceEvent::DaveReady { role } => {
                tracing::info!(role = role.as_str(), "DAVE E2EE session is ready");
                // For stream watch: the initial keyframe burst from Discord
                // often arrives before the DAVE session is ready, so those
                // frames fail decrypt and are lost.  Immediately request a
                // fresh keyframe now that we can actually decrypt.
                if role == TransportRole::StreamWatch || role == TransportRole::Voice {
                    if let Some(conn) = self.video_conn() {
                        for remote_state in self.remote_video_states.values() {
                            for stream in &remote_state.streams {
                                tracing::info!(
                                    role = role.as_str(),
                                    ssrc = stream.ssrc,
                                    "clankvox_dave_ready_pli_requesting_keyframe"
                                );
                                if let Err(error) = conn.send_rtcp_pli(stream.ssrc) {
                                    tracing::warn!(
                                        ssrc = stream.ssrc,
                                        error = %error,
                                        "clankvox_dave_ready_pli_failed"
                                    );
                                }
                            }
                            if let Some(video_ssrc) = remote_state.video_ssrc {
                                if !remote_state.streams.iter().any(|s| s.ssrc == video_ssrc) {
                                    tracing::info!(
                                        role = role.as_str(),
                                        ssrc = video_ssrc,
                                        "clankvox_dave_ready_pli_requesting_keyframe"
                                    );
                                    if let Err(error) = conn.send_rtcp_pli(video_ssrc) {
                                        tracing::warn!(
                                            ssrc = video_ssrc,
                                            error = %error,
                                            "clankvox_dave_ready_pli_failed"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            VoiceEvent::Disconnected { role, reason } => match role {
                TransportRole::Voice => self.handle_disconnected(&reason),
                TransportRole::StreamWatch => {
                    tracing::warn!(reason = %reason, "Stream watch transport disconnected");
                    self.clear_stream_watch_connection();
                    self.emit_transport_state(
                        TransportRole::StreamWatch,
                        "disconnected",
                        Some(&reason),
                    );
                    self.refresh_video_sink_wants("stream_watch_disconnected");
                }
                TransportRole::StreamPublish => {
                    tracing::warn!(reason = %reason, "Stream publish transport disconnected");
                    self.stop_stream_publish_runtime("stream_publish_transport_disconnected");
                    self.clear_stream_publish_connection();
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "disconnected",
                        Some(&reason),
                    );
                }
            },
        }
    }

    pub(crate) fn on_capture_tick(&mut self, now: time::Instant) {
        let mut speaking_ended_users: Vec<u64> = Vec::new();
        for (&user_id, state) in &mut self.speaking_states {
            if !state.is_speaking {
                continue;
            }
            if let Some(last_at) = state.last_packet_at {
                let silent_ms = now.duration_since(last_at).as_millis() as u64;
                if silent_ms >= SPEAKING_TIMEOUT_MS {
                    state.is_speaking = false;
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
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use tokio::time;

    use crate::capture::SpeakingState;
    use crate::video::UserVideoSubscription;

    use super::{
        classify_rtp_sequence, should_reassert_sink_wants_for_waiting_keyframe,
        update_speaking_state, RtpSeqClass,
    };

    #[test]
    fn update_speaking_state_only_triggers_on_first_packet_of_burst() {
        let mut speaking_states: HashMap<u64, SpeakingState> = HashMap::new();
        let first_packet_at = time::Instant::now();

        assert!(update_speaking_state(
            &mut speaking_states,
            42,
            first_packet_at
        ));
        assert!(!update_speaking_state(
            &mut speaking_states,
            42,
            first_packet_at + Duration::from_millis(20)
        ));

        let state = speaking_states.get(&42).expect("speaking state inserted");
        assert!(state.is_speaking);
        assert_eq!(
            state.last_packet_at,
            Some(first_packet_at + Duration::from_millis(20))
        );
    }

    #[test]
    fn waiting_for_first_keyframe_reasserts_sink_wants_until_keyframe_arrives() {
        let mut subscription =
            UserVideoSubscription::new(2, 100, Some(921_600), Some("screen".into()), None);
        let started_at = time::Instant::now();

        assert!(should_reassert_sink_wants_for_waiting_keyframe(
            &mut subscription,
            false,
            started_at
        ));
        assert!(!should_reassert_sink_wants_for_waiting_keyframe(
            &mut subscription,
            false,
            started_at + Duration::from_millis(500)
        ));
        assert!(should_reassert_sink_wants_for_waiting_keyframe(
            &mut subscription,
            false,
            started_at + Duration::from_secs(2)
        ));
        assert!(!should_reassert_sink_wants_for_waiting_keyframe(
            &mut subscription,
            true,
            started_at + Duration::from_secs(3)
        ));
        assert_eq!(
            subscription.last_keyframe_forwarded_at,
            Some(started_at + Duration::from_secs(3))
        );
        assert_eq!(subscription.last_sink_wants_reasserted_at, None);
    }

    // --- RTP sequence classification tests ---

    #[test]
    fn rtp_seq_first_packet_returns_first() {
        assert_eq!(classify_rtp_sequence(None, 100), RtpSeqClass::First);
        assert_eq!(classify_rtp_sequence(None, 0), RtpSeqClass::First);
        assert_eq!(classify_rtp_sequence(None, u16::MAX), RtpSeqClass::First);
    }

    #[test]
    fn rtp_seq_sequential_packet() {
        assert_eq!(
            classify_rtp_sequence(Some(100), 101),
            RtpSeqClass::Sequential
        );
        assert_eq!(classify_rtp_sequence(Some(0), 1), RtpSeqClass::Sequential);
    }

    #[test]
    fn rtp_seq_sequential_wraps_u16() {
        assert_eq!(
            classify_rtp_sequence(Some(u16::MAX), 0),
            RtpSeqClass::Sequential
        );
        assert_eq!(
            classify_rtp_sequence(Some(65534), 65535),
            RtpSeqClass::Sequential
        );
    }

    #[test]
    fn rtp_seq_duplicate_detected() {
        assert_eq!(
            classify_rtp_sequence(Some(100), 100),
            RtpSeqClass::Duplicate
        );
        assert_eq!(classify_rtp_sequence(Some(0), 0), RtpSeqClass::Duplicate);
        assert_eq!(
            classify_rtp_sequence(Some(u16::MAX), u16::MAX),
            RtpSeqClass::Duplicate
        );
    }

    #[test]
    fn rtp_seq_forward_loss_small_gaps() {
        // Gap of 1 lost packet: prev=100, expected=101, got 102
        assert_eq!(
            classify_rtp_sequence(Some(100), 102),
            RtpSeqClass::ForwardLoss { lost_count: 1 }
        );
        // Gap of 3 lost packets
        assert_eq!(
            classify_rtp_sequence(Some(100), 104),
            RtpSeqClass::ForwardLoss { lost_count: 3 }
        );
        // Gap of exactly MAX_RECOVERABLE_GAP (5)
        assert_eq!(
            classify_rtp_sequence(Some(100), 106),
            RtpSeqClass::ForwardLoss { lost_count: 5 }
        );
    }

    #[test]
    fn rtp_seq_forward_loss_across_wraparound() {
        // prev=65534, expected=65535, got 0 → gap of 1 lost packet
        assert_eq!(
            classify_rtp_sequence(Some(65534), 0),
            RtpSeqClass::ForwardLoss { lost_count: 1 }
        );
        // prev=65533, expected=65534, got 0 → gap of 2
        assert_eq!(
            classify_rtp_sequence(Some(65533), 0),
            RtpSeqClass::ForwardLoss { lost_count: 2 }
        );
    }

    #[test]
    fn rtp_seq_forward_large_gap() {
        // Gap of 6 (> MAX_RECOVERABLE_GAP): prev=100, expected=101, got 107
        assert_eq!(
            classify_rtp_sequence(Some(100), 107),
            RtpSeqClass::ForwardLarge
        );
        assert_eq!(
            classify_rtp_sequence(Some(100), 200),
            RtpSeqClass::ForwardLarge
        );
        assert_eq!(
            classify_rtp_sequence(Some(100), 1000),
            RtpSeqClass::ForwardLarge
        );
    }

    #[test]
    fn rtp_seq_stale_reordered_packet() {
        assert_eq!(classify_rtp_sequence(Some(100), 99), RtpSeqClass::Stale);
        assert_eq!(classify_rtp_sequence(Some(100), 98), RtpSeqClass::Stale);
        assert_eq!(classify_rtp_sequence(Some(100), 50), RtpSeqClass::Stale);
    }

    #[test]
    fn rtp_seq_stale_across_wraparound() {
        // prev=5, expected=6, got 65535 → stale (late arrival from before wrap)
        assert_eq!(classify_rtp_sequence(Some(5), 65535), RtpSeqClass::Stale);
        assert_eq!(classify_rtp_sequence(Some(5), 65534), RtpSeqClass::Stale);
    }

    #[test]
    fn rtp_seq_large_forward_near_half_u16_is_forward() {
        // Distance ~32000 (positive i16) → ForwardLarge
        assert_eq!(
            classify_rtp_sequence(Some(0), 32000),
            RtpSeqClass::ForwardLarge
        );
    }

    #[test]
    fn rtp_seq_large_backward_near_half_u16_is_stale() {
        // prev=32000, expected=32001, got 0 → wrapping_sub maps to negative i16 → stale
        assert_eq!(classify_rtp_sequence(Some(32000), 0), RtpSeqClass::Stale);
    }
}

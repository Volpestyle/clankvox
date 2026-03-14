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
use crate::voice_conn::{TransportRole, VoiceEvent};

const FIRST_KEYFRAME_REASSERT_INTERVAL_MS: u64 = 2_000;
/// Interval between periodic PLI requests after the first keyframe has been
/// received.  The per-frame ffmpeg decoder can only decode keyframes
/// independently, so we need fresh keyframes at roughly the vision scanner
/// rate to keep the brain context up to date.
const PERIODIC_KEYFRAME_PLI_INTERVAL_MS: u64 = 4_000;

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
        self.emit_user_video_end(user_id, ended_state.as_ref());
    }

    fn refresh_video_sink_wants(&self, reason: &str) {
        let Some(conn) = self.video_conn() else {
            tracing::info!(
                reason = reason,
                subscribed_user_count = self.user_video_subscriptions.len(),
                remote_video_user_count = self.remote_video_states.len(),
                "clankvox_video_sink_wants_skipped_no_connection"
            );
            return;
        };

        let mut wants: BTreeMap<u32, u8> = BTreeMap::new();
        let mut pixel_counts: BTreeMap<u32, f64> = BTreeMap::new();

        for (&user_id, remote_state) in &self.remote_video_states {
            for stream in &remote_state.streams {
                wants.entry(stream.ssrc).or_insert(0);
            }
            if let Some(video_ssrc) = remote_state.video_ssrc {
                wants.entry(video_ssrc).or_insert(0);
            }

            let Some(subscription) = self.user_video_subscriptions.get(&user_id) else {
                continue;
            };

            if let Some(stream) = remote_state.preferred_stream(subscription) {
                wants.insert(stream.ssrc, subscription.preferred_quality);
                if let Some(pixel_count) = subscription
                    .preferred_pixel_count
                    .or_else(|| stream.pixel_count_hint())
                {
                    pixel_counts.insert(stream.ssrc, f64::from(pixel_count));
                }
            } else if let Some(video_ssrc) = remote_state.video_ssrc {
                wants.insert(video_ssrc, subscription.preferred_quality);
                if let Some(pixel_count) = subscription.preferred_pixel_count {
                    pixel_counts.insert(video_ssrc, f64::from(pixel_count));
                }
            }
        }

        let wants = wants.into_iter().collect::<Vec<_>>();
        let pixel_counts = pixel_counts.into_iter().collect::<Vec<_>>();
        tracing::info!(
            reason = reason,
            subscribed_user_count = self.user_video_subscriptions.len(),
            remote_video_user_count = self.remote_video_states.len(),
            wanted_ssrc_count = wants.len(),
            wanted_streams = ?wants,
            pixel_count_overrides = ?pixel_counts,
            "clankvox_video_sink_wants_updated"
        );
        if let Err(error) = conn.update_media_sink_wants(&wants, &pixel_counts) {
            tracing::warn!(reason = reason, error = %error, "failed to update Discord media sink wants");
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
                );
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
                if role == TransportRole::Voice && self.stream_watch_conn.is_some() {
                    return;
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

                if update_speaking_state(&mut self.speaking_states, user_id, time::Instant::now()) {
                    send_msg(&OutMsg::SpeakingStart {
                        user_id: user_id.to_string(),
                    });
                }

                let Some(state) = self.user_capture_states.get(&user_id) else {
                    return;
                };
                let target_sample_rate = state.sample_rate;

                let mut pcm_stereo = vec![0i16; 5760];
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
                    self.opus_decoders
                        .get_mut(&ssrc)
                        .expect("decoder inserted above")
                        .decode(Some(packet), signals, false)
                };

                match decode_result {
                    Ok(samples_per_channel) => {
                        let total_samples = samples_per_channel * 2;
                        let decoded = &pcm_stereo[..total_samples];

                        let (llm_pcm, peak, active, total) =
                            crate::audio_pipeline::convert_decoded_to_llm(
                                decoded,
                                target_sample_rate,
                            );
                        if !llm_pcm.is_empty() {
                            send_msg(&OutMsg::UserAudio {
                                user_id: user_id.to_string(),
                                pcm: llm_pcm,
                                signal_peak_abs: peak,
                                signal_active_sample_count: active,
                                signal_sample_count: total,
                            });

                            if let Some(state) = self.user_capture_states.get_mut(&user_id) {
                                state.touch_audio(time::Instant::now());
                            }
                        }
                    }
                    Err(error) => {
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
                if role == TransportRole::Voice && self.stream_watch_conn.is_some() {
                    return;
                }
                if self.self_user_id == Some(user_id) {
                    return;
                }

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

                let frame_bytes = frame.len();
                subscription.forwarded_frame_count =
                    subscription.forwarded_frame_count.saturating_add(1);
                if subscription.forwarded_frame_count == 1 {
                    tracing::info!(
                        user_id,
                        ssrc,
                        codec = %codec,
                        keyframe,
                        frame_bytes,
                        rtp_timestamp,
                        stream_type = ?stream_type.as_deref(),
                        rid = ?rid.as_deref(),
                        max_frames_per_second = subscription.max_frames_per_second,
                        "clankvox_first_video_frame_forwarded"
                    );
                }
                let should_reassert_sink_wants =
                    should_reassert_sink_wants_for_waiting_keyframe(subscription, keyframe, now);
                let codec_for_log = codec.clone();

                let frame_base64 = base64::engine::general_purpose::STANDARD.encode(frame);
                send_msg(&OutMsg::UserVideoFrame {
                    user_id: user_id.to_string(),
                    ssrc,
                    codec,
                    keyframe,
                    frame_base64,
                    rtp_timestamp,
                    stream_type,
                    rid,
                    dave_decrypted,
                });
                if should_reassert_sink_wants {
                    tracing::info!(
                        user_id,
                        ssrc,
                        codec = %codec_for_log,
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

    use super::{should_reassert_sink_wants_for_waiting_keyframe, update_speaking_state};

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
            UserVideoSubscription::new(2, 100, Some(921_600), Some("screen".into()));
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
}

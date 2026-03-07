use std::collections::hash_map::Entry;

use audiopus::coder::Decoder as OpusDecoder;
use audiopus::packet::Packet as OpusPacket;
use audiopus::{Channels, MutSignals, SampleRate};
use tokio::time;

use crate::app_state::AppState;
use crate::capture::{
    normalize_sample_rate, normalize_silence_duration_ms, SpeakingState, UserCaptureState,
    SPEAKING_TIMEOUT_MS,
};
use crate::ipc::{send_msg, OutMsg};
use crate::ipc_protocol::CaptureCommand;
use crate::voice_conn::VoiceEvent;

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

impl AppState {
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
        }
    }

    pub(crate) fn handle_voice_event(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::Ready { ssrc } => {
                tracing::info!("Voice connection ready, ssrc={}", ssrc);
                self.reset_reconnect();
                send_msg(&OutMsg::ConnectionState {
                    status: "ready".into(),
                });
                send_msg(&OutMsg::Ready);

                match crate::audio_pipeline::AudioSendState::new() {
                    Ok(state) => {
                        *self.audio_send_state.lock() = Some(state);
                        crate::audio_pipeline::emit_playback_armed(
                            "connection_ready",
                            &self.audio_send_state,
                        );
                    }
                    Err(error) => tracing::error!("Failed to init audio send state: {}", error),
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
            VoiceEvent::DaveReady => {
                tracing::info!("DAVE E2EE session is ready");
            }
            VoiceEvent::Disconnected { reason } => {
                self.handle_disconnected(&reason);
            }
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

    use super::update_speaking_state;

    #[test]
    fn update_speaking_state_only_triggers_on_first_packet_of_burst() {
        let mut speaking_states: HashMap<u64, SpeakingState> = HashMap::new();
        let first_packet_at = time::Instant::now();

        assert!(update_speaking_state(&mut speaking_states, 42, first_packet_at));
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
}

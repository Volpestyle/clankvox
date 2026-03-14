use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::rtp::VideoCodecKind;
use crate::video::{VideoResolution, VideoStreamDescriptor};
use crate::voice_conn::{parse_user_id, TransportRole, VoiceEvent};

// ---------------------------------------------------------------------------
// Payload types deserialized from Discord voice gateway
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct RemoteVideoStreamPayload {
    pub(crate) ssrc: Option<u32>,
    #[serde(default)]
    pub(crate) rtx_ssrc: Option<u32>,
    #[serde(default)]
    pub(crate) rid: Option<String>,
    #[serde(default)]
    pub(crate) quality: Option<u32>,
    #[serde(default, rename = "type")]
    pub(crate) stream_type: Option<String>,
    #[serde(default)]
    pub(crate) active: Option<bool>,
    #[serde(default)]
    pub(crate) max_bitrate: Option<u32>,
    #[serde(default)]
    pub(crate) max_framerate: Option<u32>,
    #[serde(default)]
    pub(crate) max_resolution: Option<RemoteVideoResolutionPayload>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct RemoteVideoResolutionPayload {
    #[serde(default)]
    pub(crate) width: Option<u32>,
    #[serde(default)]
    pub(crate) height: Option<u32>,
    #[serde(default, rename = "type")]
    pub(crate) resolution_type: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct RemoteVideoStatePayload {
    #[serde(default)]
    pub(crate) user_id: Option<String>,
    #[serde(default)]
    pub(crate) audio_ssrc: Option<u32>,
    #[serde(default)]
    pub(crate) video_ssrc: Option<u32>,
    #[serde(default)]
    pub(crate) streams: Vec<RemoteVideoStreamPayload>,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteVideoTrackBinding {
    pub(crate) user_id: u64,
    pub(crate) descriptor: VideoStreamDescriptor,
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

pub(crate) fn normalize_stream_type(stream_type: Option<String>) -> Option<String> {
    stream_type
        .map(|stream_type| stream_type.trim().to_ascii_lowercase())
        .filter(|stream_type| !stream_type.is_empty())
}

pub(crate) fn convert_video_stream_descriptor(
    stream: RemoteVideoStreamPayload,
) -> Option<VideoStreamDescriptor> {
    let ssrc = stream.ssrc.filter(|ssrc| *ssrc != 0)?;
    Some(VideoStreamDescriptor {
        ssrc,
        rtx_ssrc: stream.rtx_ssrc.filter(|ssrc| *ssrc != 0),
        rid: stream.rid,
        quality: stream.quality,
        stream_type: normalize_stream_type(stream.stream_type),
        active: stream.active,
        max_bitrate: stream.max_bitrate,
        max_framerate: stream.max_framerate,
        max_resolution: stream.max_resolution.map(|resolution| VideoResolution {
            width: resolution.width,
            height: resolution.height,
            resolution_type: resolution.resolution_type,
        }),
    })
}

pub(crate) fn build_video_state_announcement(
    audio_ssrc: u32,
    streams: &[VideoStreamDescriptor],
    active: bool,
) -> Option<Value> {
    let primary_stream = streams
        .iter()
        .find(|stream| stream.is_active())
        .or_else(|| streams.first())?;
    Some(json!({
        "op": 12,
        "d": {
            "audio_ssrc": audio_ssrc,
            "video_ssrc": primary_stream.ssrc,
            "rtx_ssrc": primary_stream.rtx_ssrc,
            "streams": streams.iter().map(|stream| json!({
                "type": stream.stream_type,
                "rid": stream.rid,
                "ssrc": stream.ssrc,
                "rtx_ssrc": stream.rtx_ssrc,
                "active": active,
                "quality": stream.quality,
                "max_bitrate": stream.max_bitrate,
                "max_framerate": stream.max_framerate,
                "max_resolution": stream.max_resolution.as_ref().map(|resolution| json!({
                    "type": resolution.resolution_type,
                    "width": resolution.width,
                    "height": resolution.height,
                })),
            })).collect::<Vec<_>>()
        }
    }))
}

pub(crate) fn update_current_video_codec(
    codec_state: &Arc<Mutex<Option<String>>>,
    codec: Option<String>,
) {
    if let Some(codec) = codec.filter(|codec| !codec.trim().is_empty()) {
        let normalized = VideoCodecKind::from_name(&codec)
            .map(|codec| codec.as_str().to_string())
            .unwrap_or(codec);
        *codec_state.lock() = Some(normalized);
    }
}

pub(crate) async fn apply_remote_video_state(
    payload: RemoteVideoStatePayload,
    event_tx: &mpsc::Sender<VoiceEvent>,
    video_ssrc_map: &Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>>,
    current_video_codec: &Arc<Mutex<Option<String>>>,
    role: TransportRole,
) {
    let stream_count = payload.streams.len();
    let Some(raw_user_id) = payload.user_id.as_deref() else {
        warn!(
            audio_ssrc = payload.audio_ssrc,
            video_ssrc = payload.video_ssrc,
            stream_count,
            "ignoring video state payload without user_id"
        );
        return;
    };
    let Some(user_id) = parse_user_id(raw_user_id, "video_state") else {
        return;
    };

    let audio_ssrc = payload.audio_ssrc.filter(|ssrc| *ssrc != 0);
    let video_ssrc = payload.video_ssrc.filter(|ssrc| *ssrc != 0);
    let mut streams = payload
        .streams
        .into_iter()
        .filter_map(convert_video_stream_descriptor)
        .collect::<Vec<_>>();
    let clear_video_state = video_ssrc.is_none() && streams.is_empty();

    {
        let mut guard = video_ssrc_map.lock();
        let mut previous_streams = guard
            .values()
            .filter(|binding| binding.user_id == user_id)
            .map(|binding| binding.descriptor.clone())
            .collect::<Vec<_>>();
        previous_streams.sort_by_key(|stream| stream.ssrc);

        if !clear_video_state && streams.is_empty() && !previous_streams.is_empty() {
            debug!(
                user_id,
                preserved_streams = previous_streams.len(),
                video_ssrc,
                "Voice video state update omitted streams; preserving prior SSRC bindings"
            );
            streams = previous_streams;
        }

        if let Some(video_ssrc) = video_ssrc {
            if !streams.iter().any(|stream| stream.ssrc == video_ssrc) {
                streams.push(VideoStreamDescriptor {
                    ssrc: video_ssrc,
                    rtx_ssrc: None,
                    rid: None,
                    quality: None,
                    stream_type: None,
                    active: Some(true),
                    max_bitrate: None,
                    max_framerate: None,
                    max_resolution: None,
                });
            }
        }

        guard.retain(|_, binding| binding.user_id != user_id);
        for descriptor in &streams {
            guard.insert(
                descriptor.ssrc,
                RemoteVideoTrackBinding {
                    user_id,
                    descriptor: descriptor.clone(),
                },
            );
        }
    }

    let codec = current_video_codec.lock().clone();
    let stream_ssrcs = streams.iter().map(|stream| stream.ssrc).collect::<Vec<_>>();
    let active_stream_count = streams.iter().filter(|stream| stream.is_active()).count();
    info!(
        user_id,
        audio_ssrc = audio_ssrc,
        video_ssrc = video_ssrc,
        codec = ?codec.as_deref(),
        stream_count = streams.len(),
        active_stream_count,
        stream_ssrcs = ?stream_ssrcs,
        "clankvox_discord_video_state_observed"
    );
    let _ = event_tx
        .send(VoiceEvent::VideoStateUpdate {
            role,
            user_id,
            audio_ssrc,
            video_ssrc,
            codec,
            streams,
        })
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_remote_video_state_preserves_existing_streams_when_update_omits_streams() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let descriptor = VideoStreamDescriptor {
            ssrc: 4001,
            rtx_ssrc: Some(5001),
            rid: Some("f".into()),
            quality: Some(100),
            stream_type: Some("screen".into()),
            active: Some(true),
            max_bitrate: Some(4_000_000),
            max_framerate: Some(30),
            max_resolution: None,
        };
        let video_ssrc_map = Arc::new(Mutex::new(HashMap::from([
            (
                descriptor.ssrc,
                RemoteVideoTrackBinding {
                    user_id: 42,
                    descriptor: descriptor.clone(),
                },
            ),
            (
                9001,
                RemoteVideoTrackBinding {
                    user_id: 99,
                    descriptor: VideoStreamDescriptor {
                        ssrc: 9001,
                        rtx_ssrc: None,
                        rid: None,
                        quality: Some(50),
                        stream_type: Some("camera".into()),
                        active: Some(true),
                        max_bitrate: None,
                        max_framerate: None,
                        max_resolution: None,
                    },
                },
            ),
        ])));
        let current_video_codec = Arc::new(Mutex::new(Some("h264".to_string())));

        apply_remote_video_state(
            RemoteVideoStatePayload {
                user_id: Some("42".into()),
                audio_ssrc: Some(3001),
                video_ssrc: Some(descriptor.ssrc),
                streams: Vec::new(),
            },
            &event_tx,
            &video_ssrc_map,
            &current_video_codec,
            TransportRole::Voice,
        )
        .await;

        let event = event_rx.recv().await.expect("video state event");
        match event {
            VoiceEvent::VideoStateUpdate {
                user_id,
                audio_ssrc,
                video_ssrc,
                codec,
                streams,
                ..
            } => {
                assert_eq!(user_id, 42);
                assert_eq!(audio_ssrc, Some(3001));
                assert_eq!(video_ssrc, Some(descriptor.ssrc));
                assert_eq!(codec.as_deref(), Some("h264"));
                assert_eq!(streams, vec![descriptor.clone()]);
            }
            _ => panic!("unexpected event type"),
        }

        let guard = video_ssrc_map.lock();
        assert_eq!(
            guard.get(&descriptor.ssrc).map(|binding| binding.user_id),
            Some(42)
        );
        assert_eq!(
            guard
                .get(&descriptor.ssrc)
                .map(|binding| binding.descriptor.clone()),
            Some(descriptor)
        );
        assert_eq!(guard.get(&9001).map(|binding| binding.user_id), Some(99));
    }

    #[tokio::test]
    async fn apply_remote_video_state_clears_bindings_on_explicit_empty_state() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let descriptor = VideoStreamDescriptor {
            ssrc: 4101,
            rtx_ssrc: None,
            rid: Some("h".into()),
            quality: Some(80),
            stream_type: Some("screen".into()),
            active: Some(true),
            max_bitrate: None,
            max_framerate: None,
            max_resolution: None,
        };
        let video_ssrc_map = Arc::new(Mutex::new(HashMap::from([(
            descriptor.ssrc,
            RemoteVideoTrackBinding {
                user_id: 42,
                descriptor: descriptor.clone(),
            },
        )])));
        let current_video_codec = Arc::new(Mutex::new(None));

        apply_remote_video_state(
            RemoteVideoStatePayload {
                user_id: Some("42".into()),
                audio_ssrc: None,
                video_ssrc: None,
                streams: Vec::new(),
            },
            &event_tx,
            &video_ssrc_map,
            &current_video_codec,
            TransportRole::Voice,
        )
        .await;

        let event = event_rx.recv().await.expect("video state event");
        match event {
            VoiceEvent::VideoStateUpdate {
                user_id,
                audio_ssrc,
                video_ssrc,
                codec,
                streams,
                ..
            } => {
                assert_eq!(user_id, 42);
                assert_eq!(audio_ssrc, None);
                assert_eq!(video_ssrc, None);
                assert_eq!(codec, None);
                assert!(streams.is_empty());
            }
            _ => panic!("unexpected event type"),
        }

        assert!(!video_ssrc_map.lock().contains_key(&descriptor.ssrc));
    }
}

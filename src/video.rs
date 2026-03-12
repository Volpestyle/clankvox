use serde::Serialize;
use tokio::time;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct VideoResolution {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<u32>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) resolution_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct VideoStreamDescriptor {
    pub(crate) ssrc: u32,
    #[serde(rename = "rtxSsrc", skip_serializing_if = "Option::is_none")]
    pub(crate) rtx_ssrc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) quality: Option<u32>,
    #[serde(rename = "streamType", skip_serializing_if = "Option::is_none")]
    pub(crate) stream_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) active: Option<bool>,
    #[serde(rename = "maxBitrate", skip_serializing_if = "Option::is_none")]
    pub(crate) max_bitrate: Option<u32>,
    #[serde(rename = "maxFramerate", skip_serializing_if = "Option::is_none")]
    pub(crate) max_framerate: Option<u32>,
    #[serde(rename = "maxResolution", skip_serializing_if = "Option::is_none")]
    pub(crate) max_resolution: Option<VideoResolution>,
}

impl VideoStreamDescriptor {
    pub(crate) fn pixel_count_hint(&self) -> Option<u32> {
        let resolution = self.max_resolution.as_ref()?;
        let width = resolution.width?;
        let height = resolution.height?;
        let pixels = width.checked_mul(height)?;
        (pixels > 0).then_some(pixels)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.unwrap_or(true)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UserVideoSubscription {
    pub(crate) max_frames_per_second: u32,
    pub(crate) preferred_quality: u8,
    pub(crate) preferred_pixel_count: Option<u32>,
    pub(crate) preferred_stream_type: Option<String>,
    pub(crate) last_frame_sent_at: Option<time::Instant>,
}

impl UserVideoSubscription {
    pub(crate) fn new(
        max_frames_per_second: u32,
        preferred_quality: u32,
        preferred_pixel_count: Option<u32>,
        preferred_stream_type: Option<String>,
    ) -> Self {
        Self {
            max_frames_per_second: normalize_video_max_fps(max_frames_per_second),
            preferred_quality: normalize_video_quality(preferred_quality),
            preferred_pixel_count: preferred_pixel_count.and_then(normalize_video_pixel_count),
            preferred_stream_type: preferred_stream_type
                .map(|stream_type| stream_type.trim().to_ascii_lowercase())
                .filter(|stream_type| !stream_type.is_empty()),
            last_frame_sent_at: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteVideoState {
    pub(crate) audio_ssrc: Option<u32>,
    pub(crate) video_ssrc: Option<u32>,
    pub(crate) codec: Option<String>,
    pub(crate) streams: Vec<VideoStreamDescriptor>,
}

impl RemoteVideoState {
    pub(crate) fn has_streams(&self) -> bool {
        self.video_ssrc.is_some() || !self.streams.is_empty()
    }

    pub(crate) fn preferred_stream<'a>(
        &'a self,
        subscription: &UserVideoSubscription,
    ) -> Option<&'a VideoStreamDescriptor> {
        let preferred_type = subscription.preferred_stream_type.as_deref();
        self.streams
            .iter()
            .filter(|stream| stream.ssrc != 0)
            .max_by(|a, b| {
                let a_score = video_stream_sort_key(self.video_ssrc, preferred_type, a);
                let b_score = video_stream_sort_key(self.video_ssrc, preferred_type, b);
                a_score.cmp(&b_score)
            })
    }
}

fn video_stream_sort_key(
    preferred_ssrc: Option<u32>,
    preferred_type: Option<&str>,
    stream: &VideoStreamDescriptor,
) -> (u8, u8, u8, u32, u32) {
    let ssrc_match = preferred_ssrc.is_some_and(|ssrc| ssrc == stream.ssrc) as u8;
    let type_match = preferred_type
        .zip(stream.stream_type.as_deref())
        .is_some_and(|(preferred, actual)| preferred.eq_ignore_ascii_case(actual))
        as u8;
    let active = stream.is_active() as u8;
    let pixels = stream.pixel_count_hint().unwrap_or_default();
    let quality = stream.quality.unwrap_or_default();
    (active, type_match, ssrc_match, pixels, quality)
}

pub(crate) fn normalize_video_max_fps(fps: u32) -> u32 {
    fps.clamp(1, 30)
}

pub(crate) fn normalize_video_quality(quality: u32) -> u8 {
    quality.clamp(0, 100) as u8
}

pub(crate) fn normalize_video_pixel_count(pixel_count: u32) -> Option<u32> {
    if pixel_count == 0 {
        return None;
    }
    Some(pixel_count.clamp(64 * 64, 3840 * 2160))
}

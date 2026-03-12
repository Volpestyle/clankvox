use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, trace, warn};

use crate::dave::DaveManager;
use crate::video::{VideoResolution, VideoStreamDescriptor};

type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug, Deserialize)]
struct VoiceOpcode<T> {
    op: u64,
    d: T,
}

#[derive(Debug, Deserialize)]
struct HelloPayload {
    heartbeat_interval: Option<f64>,
}

#[derive(Debug, Deserialize, Clone)]
struct ReadyPayload {
    ssrc: u32,
    ip: String,
    port: u16,
    modes: Vec<String>,
    #[serde(default)]
    experiments: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct SessionDescriptionPayload {
    secret_key: Vec<u8>,
    #[serde(default)]
    dave_protocol_version: u16,
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    media_session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpeakingPayload {
    ssrc: u32,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct UserIdPayload {
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct TransitionPayload {
    transition_id: u16,
    #[serde(default)]
    protocol_version: u16,
}

#[derive(Debug, Deserialize)]
struct EpochPayload {
    protocol_version: u16,
    epoch: u64,
}

#[derive(Debug, Deserialize, Clone)]
struct RemoteVideoStreamPayload {
    ssrc: Option<u32>,
    #[serde(default)]
    rtx_ssrc: Option<u32>,
    #[serde(default)]
    rid: Option<String>,
    #[serde(default)]
    quality: Option<u32>,
    #[serde(default, rename = "type")]
    stream_type: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    max_bitrate: Option<u32>,
    #[serde(default)]
    max_framerate: Option<u32>,
    #[serde(default)]
    max_resolution: Option<RemoteVideoResolutionPayload>,
}

#[derive(Debug, Deserialize, Clone)]
struct RemoteVideoResolutionPayload {
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default, rename = "type")]
    resolution_type: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct RemoteVideoStatePayload {
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    audio_ssrc: Option<u32>,
    #[serde(default)]
    video_ssrc: Option<u32>,
    #[serde(default)]
    streams: Vec<RemoteVideoStreamPayload>,
}

#[derive(Debug, Deserialize, Clone)]
struct SessionUpdatePayload {
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    media_session_id: Option<String>,
    #[serde(default)]
    keyframe_interval: Option<u32>,
}

#[derive(Clone, Debug)]
struct RemoteVideoTrackBinding {
    user_id: u64,
    descriptor: VideoStreamDescriptor,
}

fn parse_voice_opcode<T>(text: &str) -> Result<VoiceOpcode<T>>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(text).context("invalid voice gateway payload")
}

fn parse_user_id(user_id: &str, context: &str) -> Option<u64> {
    match user_id.parse::<u64>() {
        Ok(user_id) => Some(user_id),
        Err(error) => {
            warn!(user_id, context, error = %error, "ignoring voice gateway payload with invalid user id");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Events emitted by the voice connection back to the main loop
// ---------------------------------------------------------------------------

pub enum VoiceEvent {
    Ready {
        ssrc: u32,
    },
    SsrcUpdate {
        ssrc: u32,
        user_id: u64,
    },
    VideoStateUpdate {
        user_id: u64,
        audio_ssrc: Option<u32>,
        video_ssrc: Option<u32>,
        codec: Option<String>,
        streams: Vec<VideoStreamDescriptor>,
    },
    ClientDisconnect {
        user_id: u64,
    },
    OpusReceived {
        ssrc: u32,
        opus_frame: Vec<u8>,
    },
    VideoFrameReceived {
        user_id: u64,
        ssrc: u32,
        codec: String,
        keyframe: bool,
        frame: Vec<u8>,
        rtp_timestamp: u32,
        stream_type: Option<String>,
        rid: Option<String>,
    },
    DaveReady,
    Disconnected {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Internal commands for the WS write task
// ---------------------------------------------------------------------------

enum WsCommand {
    SendJson(Value),
    SendBinary(Vec<u8>),
}

// ---------------------------------------------------------------------------
// RTP header (minimal, Discord voice)
// ---------------------------------------------------------------------------

const RTP_HEADER_LEN: usize = 12;
const OPUS_PT: u8 = 0x78; // payload type 120
const H264_PT: u8 = 103;
const H264_RTX_PT: u8 = 104;
const VP8_PT: u8 = 105;
const VP8_RTX_PT: u8 = 106;
const MAX_VIDEO_FRAME_BYTES: usize = 8 * 1024 * 1024;

fn build_rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; RTP_HEADER_LEN] {
    let mut h = [0u8; RTP_HEADER_LEN];
    h[0] = 0x80; // V=2, P=0, X=0, CC=0
    h[1] = OPUS_PT;
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

fn parse_rtp_header(data: &[u8]) -> Option<(u16, u32, u32, usize, bool)> {
    if data.len() < RTP_HEADER_LEN {
        return None;
    }
    let cc = (data[0] & 0x0F) as usize;
    let has_ext = (data[0] >> 4) & 0x01 != 0;
    let seq = u16::from_be_bytes([data[2], data[3]]);
    let ts = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let marker = (data[1] & 0x80) != 0;

    let mut header_size = RTP_HEADER_LEN + cc * 4;
    if data.len() < header_size {
        return None;
    }
    if has_ext {
        if data.len() < header_size + 4 {
            return None;
        }
        let ext_len = u16::from_be_bytes([data[header_size + 2], data[header_size + 3]]) as usize;
        header_size += 4 + ext_len * 4;
        if data.len() < header_size {
            return None;
        }
    }
    Some((seq, ts, ssrc, header_size, marker))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoCodecKind {
    H264,
    Vp8,
}

impl VideoCodecKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Vp8 => "VP8",
        }
    }

    fn payload_type(self) -> u8 {
        match self {
            Self::H264 => H264_PT,
            Self::Vp8 => VP8_PT,
        }
    }

    fn rtx_payload_type(self) -> u8 {
        match self {
            Self::H264 => H264_RTX_PT,
            Self::Vp8 => VP8_RTX_PT,
        }
    }

    fn from_payload_type(payload_type: u8) -> Option<Self> {
        match payload_type {
            H264_PT => Some(Self::H264),
            VP8_PT => Some(Self::Vp8),
            _ => None,
        }
    }

    fn is_rtx_payload_type(payload_type: u8) -> bool {
        matches!(payload_type, H264_RTX_PT | VP8_RTX_PT)
    }

    fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "H264" | "H.264" => Some(Self::H264),
            "VP8" => Some(Self::Vp8),
            _ => None,
        }
    }
}

#[derive(Default)]
struct VideoDepacketizers {
    by_ssrc: HashMap<u32, VideoDepacketizerState>,
}

impl VideoDepacketizers {
    fn push(
        &mut self,
        ssrc: u32,
        codec: VideoCodecKind,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<(Vec<u8>, bool)> {
        let state = self
            .by_ssrc
            .entry(ssrc)
            .or_insert_with(|| VideoDepacketizerState::new(codec));
        if state.codec != codec {
            *state = VideoDepacketizerState::new(codec);
        }
        state.push(ssrc, sequence, timestamp, marker, payload)
    }
}

struct VideoDepacketizerState {
    codec: VideoCodecKind,
    last_sequence: Option<u16>,
    h264: H264Depacketizer,
    vp8: Vp8Depacketizer,
}

impl VideoDepacketizerState {
    fn new(codec: VideoCodecKind) -> Self {
        Self {
            codec,
            last_sequence: None,
            h264: H264Depacketizer::default(),
            vp8: Vp8Depacketizer::default(),
        }
    }

    fn push(
        &mut self,
        ssrc: u32,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<(Vec<u8>, bool)> {
        if let Some(previous_sequence) = self.last_sequence {
            let expected_sequence = previous_sequence.wrapping_add(1);
            if expected_sequence != sequence {
                debug!(
                    ssrc,
                    codec = self.codec.as_str(),
                    expected_sequence,
                    sequence,
                    timestamp,
                    "UDP video sequence gap/reorder detected; dropping partial frame"
                );
                self.clear_partial_frame();
            }
        }
        self.last_sequence = Some(sequence);

        match self.codec {
            VideoCodecKind::H264 => self.h264.push(timestamp, marker, payload),
            VideoCodecKind::Vp8 => self.vp8.push(timestamp, marker, payload),
        }
    }

    fn clear_partial_frame(&mut self) {
        self.h264.reset();
        self.vp8.reset();
    }
}

#[derive(Default)]
struct H264Depacketizer {
    timestamp: Option<u32>,
    buffer: Vec<u8>,
    keyframe: bool,
    in_fu: bool,
}

impl H264Depacketizer {
    fn push(&mut self, timestamp: u32, marker: bool, payload: &[u8]) -> Option<(Vec<u8>, bool)> {
        if payload.is_empty() {
            return None;
        }
        self.prepare_timestamp(timestamp);
        let nal_type = payload[0] & 0x1F;
        match nal_type {
            1..=23 => {
                self.append_start_code();
                self.extend(payload)?;
                if matches!(nal_type, 5 | 7) {
                    self.keyframe = true;
                }
                self.in_fu = false;
            }
            24 => {
                let mut cursor = 1usize;
                while cursor + 2 <= payload.len() {
                    let nalu_len =
                        u16::from_be_bytes([payload[cursor], payload[cursor + 1]]) as usize;
                    cursor += 2;
                    if nalu_len == 0 || cursor + nalu_len > payload.len() {
                        self.reset();
                        return None;
                    }
                    let nalu = &payload[cursor..cursor + nalu_len];
                    if !nalu.is_empty() {
                        let stap_nal_type = nalu[0] & 0x1F;
                        if matches!(stap_nal_type, 5 | 7) {
                            self.keyframe = true;
                        }
                        self.append_start_code();
                        self.extend(nalu)?;
                    }
                    cursor += nalu_len;
                }
            }
            28 => {
                if payload.len() < 2 {
                    return None;
                }
                let indicator = payload[0];
                let fu_header = payload[1];
                let start = (fu_header & 0x80) != 0;
                let nal_type = fu_header & 0x1F;
                if start {
                    let reconstructed_header = (indicator & 0xE0) | nal_type;
                    self.append_start_code();
                    self.extend(&[reconstructed_header])?;
                    self.extend(&payload[2..])?;
                    self.in_fu = true;
                    if matches!(nal_type, 5 | 7) {
                        self.keyframe = true;
                    }
                } else {
                    if !self.in_fu {
                        return None;
                    }
                    self.extend(&payload[2..])?;
                    if (fu_header & 0x40) != 0 {
                        self.in_fu = false;
                    }
                }
            }
            _ => {
                return None;
            }
        }

        if marker && !self.buffer.is_empty() {
            let keyframe = self.keyframe || h264_annexb_is_keyframe(&self.buffer);
            let frame = std::mem::take(&mut self.buffer);
            self.timestamp = None;
            self.keyframe = false;
            self.in_fu = false;
            return Some((frame, keyframe));
        }

        None
    }

    fn prepare_timestamp(&mut self, timestamp: u32) {
        if self.timestamp != Some(timestamp) {
            self.timestamp = Some(timestamp);
            self.buffer.clear();
            self.keyframe = false;
            self.in_fu = false;
        }
    }

    fn append_start_code(&mut self) {
        self.buffer.extend_from_slice(&[0, 0, 0, 1]);
    }

    fn extend(&mut self, bytes: &[u8]) -> Option<()> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_VIDEO_FRAME_BYTES {
            self.reset();
            return None;
        }
        self.buffer.extend_from_slice(bytes);
        Some(())
    }

    fn reset(&mut self) {
        self.timestamp = None;
        self.buffer.clear();
        self.keyframe = false;
        self.in_fu = false;
    }
}

#[derive(Default)]
struct Vp8Depacketizer {
    timestamp: Option<u32>,
    buffer: Vec<u8>,
    keyframe: bool,
    saw_partition_start: bool,
}

impl Vp8Depacketizer {
    fn push(&mut self, timestamp: u32, marker: bool, payload: &[u8]) -> Option<(Vec<u8>, bool)> {
        let (descriptor_len, start_of_partition) = parse_vp8_payload_descriptor(payload)?;
        let frame_payload = &payload[descriptor_len..];
        if frame_payload.is_empty() {
            return None;
        }

        if self.timestamp != Some(timestamp) {
            self.timestamp = Some(timestamp);
            self.buffer.clear();
            self.keyframe = false;
            self.saw_partition_start = false;
        }

        if start_of_partition {
            self.saw_partition_start = true;
            if let Some(first_byte) = frame_payload.first() {
                self.keyframe = (first_byte & 0x01) == 0;
            }
        } else if !self.saw_partition_start && self.buffer.is_empty() {
            return None;
        }

        if self.buffer.len().saturating_add(frame_payload.len()) > MAX_VIDEO_FRAME_BYTES {
            self.timestamp = None;
            self.buffer.clear();
            self.keyframe = false;
            self.saw_partition_start = false;
            return None;
        }
        self.buffer.extend_from_slice(frame_payload);

        if marker && !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            let keyframe = self.keyframe;
            self.timestamp = None;
            self.keyframe = false;
            self.saw_partition_start = false;
            return Some((frame, keyframe));
        }

        None
    }

    fn reset(&mut self) {
        self.timestamp = None;
        self.buffer.clear();
        self.keyframe = false;
        self.saw_partition_start = false;
    }
}

fn parse_vp8_payload_descriptor(payload: &[u8]) -> Option<(usize, bool)> {
    if payload.is_empty() {
        return None;
    }
    let mut cursor = 1usize;
    let x = (payload[0] & 0x80) != 0;
    let s = (payload[0] & 0x10) != 0;
    let partition_id = payload[0] & 0x0F;
    if x {
        if payload.len() < cursor + 1 {
            return None;
        }
        let i = (payload[cursor] & 0x80) != 0;
        let l = (payload[cursor] & 0x40) != 0;
        let t = (payload[cursor] & 0x20) != 0;
        let k = (payload[cursor] & 0x10) != 0;
        cursor += 1;
        if i {
            if payload.len() < cursor + 1 {
                return None;
            }
            let m = (payload[cursor] & 0x80) != 0;
            cursor += if m { 2 } else { 1 };
        }
        if l {
            cursor += 1;
        }
        if t || k {
            cursor += 1;
        }
    }
    if payload.len() < cursor {
        return None;
    }
    Some((cursor, s && partition_id == 0))
}

fn h264_annexb_is_keyframe(frame: &[u8]) -> bool {
    let mut index = 0usize;
    while index + 4 <= frame.len() {
        if frame[index..].starts_with(&[0, 0, 0, 1]) {
            let nal_start = index + 4;
            if let Some(byte) = frame.get(nal_start) {
                let nal_type = byte & 0x1F;
                if matches!(nal_type, 5 | 7) {
                    return true;
                }
            }
            index = nal_start;
        } else if frame[index..].starts_with(&[0, 0, 1]) {
            let nal_start = index + 3;
            if let Some(byte) = frame.get(nal_start) {
                let nal_type = byte & 0x1F;
                if matches!(nal_type, 5 | 7) {
                    return true;
                }
            }
            index = nal_start;
        } else {
            index += 1;
        }
    }
    false
}

fn normalize_stream_type(stream_type: Option<String>) -> Option<String> {
    stream_type
        .map(|stream_type| stream_type.trim().to_ascii_lowercase())
        .filter(|stream_type| !stream_type.is_empty())
}

fn convert_video_stream_descriptor(
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

fn update_current_video_codec(codec_state: &Arc<Mutex<Option<String>>>, codec: Option<String>) {
    if let Some(codec) = codec.filter(|codec| !codec.trim().is_empty()) {
        let normalized = VideoCodecKind::from_name(&codec)
            .map(|codec| codec.as_str().to_string())
            .unwrap_or(codec);
        *codec_state.lock() = Some(normalized);
    }
}

async fn apply_remote_video_state(
    payload: RemoteVideoStatePayload,
    event_tx: &mpsc::Sender<VoiceEvent>,
    video_ssrc_map: &Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>>,
    current_video_codec: &Arc<Mutex<Option<String>>>,
) {
    let Some(user_id) = payload
        .user_id
        .as_deref()
        .and_then(|user_id| parse_user_id(user_id, "video_state"))
    else {
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
    let _ = event_tx
        .send(VoiceEvent::VideoStateUpdate {
            user_id,
            audio_ssrc,
            video_ssrc,
            codec,
            streams,
        })
        .await;
}

fn build_select_protocol_payload(
    external_ip: &str,
    external_port: u16,
    mode: &str,
    experiments: &[String],
) -> Value {
    let video_codecs = [VideoCodecKind::H264, VideoCodecKind::Vp8]
        .into_iter()
        .enumerate()
        .map(|(idx, codec)| {
            json!({
                "name": codec.as_str(),
                "type": "video",
                "priority": 900u32.saturating_sub(idx as u32 * 10),
                "payload_type": codec.payload_type(),
                "rtx_payload_type": codec.rtx_payload_type(),
                "encode": false,
                "decode": true,
            })
        })
        .collect::<Vec<_>>();

    let mut codecs = vec![json!({
        "name": "opus",
        "type": "audio",
        "priority": 1000,
        "payload_type": OPUS_PT,
    })];
    codecs.extend(video_codecs);

    json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": {
                "address": external_ip,
                "port": external_port,
                "mode": mode
            },
            "codecs": codecs,
            "experiments": experiments,
        }
    })
}

fn build_media_sink_wants_payload(wants: &[(u32, u8)], pixel_counts: &[(u32, f64)]) -> Value {
    let streams = wants
        .iter()
        .fold(serde_json::Map::new(), |mut acc, (ssrc, quality)| {
            acc.insert(ssrc.to_string(), json!(quality));
            acc
        });
    let pixel_counts =
        pixel_counts
            .iter()
            .fold(serde_json::Map::new(), |mut acc, (ssrc, pixel_count)| {
                acc.insert(ssrc.to_string(), json!(pixel_count));
                acc
            });
    json!({
        "op": 15,
        "d": {
            "streams": streams,
            "pixelCounts": pixel_counts,
        }
    })
}

fn strip_rtp_extension_payload(
    packet: &[u8],
    decrypted: Vec<u8>,
) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let cc = (packet[0] & 0x0F) as usize;
    let aad_size = RTP_HEADER_LEN + cc * 4;
    let has_ext = (packet[0] >> 4) & 0x01 != 0;
    if !has_ext || packet.len() < aad_size + 4 {
        return Some((decrypted, None));
    }

    let profile = &packet[aad_size..aad_size + 2];
    let ext_len = u16::from_be_bytes([packet[aad_size + 2], packet[aad_size + 3]]) as usize;
    let extension_bytes = ext_len * 4;
    if decrypted.len() <= extension_bytes {
        return None;
    }

    let stripped = decrypted[extension_bytes..].to_vec();
    if profile != &[0xbe, 0xde] {
        debug!(profile = ?profile, "UDP: non-BEDE RTP extension profile stripped");
    }
    Some((stripped, Some(decrypted)))
}

// ---------------------------------------------------------------------------
// Transport encryption (AES-256-GCM "rtpsize" mode)
// ---------------------------------------------------------------------------

struct TransportCrypto {
    cipher: TransportCipher,
    send_nonce: AtomicU32,
}

enum TransportCipher {
    Aes256GcmRtpSize(Box<Aes256Gcm>),
    XChaCha20Poly1305RtpSize(XChaCha20Poly1305),
}

pub struct VoiceConnectionParams<'a> {
    pub endpoint: &'a str,
    pub guild_id: u64,
    pub user_id: u64,
    pub session_id: &'a str,
    pub token: &'a str,
    pub channel_id: u64,
}

impl TransportCrypto {
    fn new(secret_key: &[u8], mode: &str) -> Result<Self> {
        let cipher = match mode {
            "aead_aes256_gcm_rtpsize" => TransportCipher::Aes256GcmRtpSize(Box::new(
                Aes256Gcm::new_from_slice(secret_key).context("Invalid AES-256-GCM secret key")?,
            )),
            "aead_xchacha20_poly1305_rtpsize" => TransportCipher::XChaCha20Poly1305RtpSize(
                XChaCha20Poly1305::new_from_slice(secret_key)
                    .context("Invalid XChaCha20-Poly1305 secret key")?,
            ),
            other => bail!("Unsupported transport mode: {other}"),
        };
        Ok(Self {
            cipher,
            send_nonce: AtomicU32::new(0),
        })
    }

    /// Encrypt an Opus payload for sending.
    /// Returns `[ciphertext + 16-byte tag + 4-byte BE nonce]`.
    fn encrypt(&self, rtp_header: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        let nonce_val = self.send_nonce.fetch_add(1, Ordering::SeqCst);
        let ct = match &self.cipher {
            TransportCipher::Aes256GcmRtpSize(cipher) => {
                let mut nonce_12 = [0u8; 12];
                nonce_12[0..4].copy_from_slice(&nonce_val.to_be_bytes());
                cipher
                    .encrypt(
                        Nonce::from_slice(&nonce_12),
                        Payload {
                            msg: payload,
                            aad: rtp_header,
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("AES-GCM encrypt: {e}"))?
            }
            TransportCipher::XChaCha20Poly1305RtpSize(cipher) => {
                let mut nonce_24 = [0u8; 24];
                nonce_24[0..4].copy_from_slice(&nonce_val.to_be_bytes());
                cipher
                    .encrypt(
                        XNonce::from_slice(&nonce_24),
                        Payload {
                            msg: payload,
                            aad: rtp_header,
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt: {e}"))?
            }
        };

        let mut out = ct; // ciphertext + tag
        out.extend_from_slice(&nonce_val.to_be_bytes());
        Ok(out)
    }

    /// Decrypt a received RTP payload.
    /// `packet` is the full UDP datagram, `header_size` is the RTP header length.
    fn decrypt(&self, packet: &[u8], _header_size: usize) -> Result<Vec<u8>> {
        // Layout: [rtp_header_for_aad | ciphertext + 16-byte tag | 4-byte BE nonce]
        if packet.len() < 16 + 4 + 16 {
            bail!("Packet too small for transport decryption");
        }

        // In "aead_aes256_gcm_rtpsize", the AAD is:
        // RTP fixed header + CSRC list, plus 4 bytes of extension header if X is set.
        let cc = (packet[0] & 0x0F) as usize;
        let mut aad_size = RTP_HEADER_LEN + cc * 4;
        if (packet[0] >> 4) & 0x01 != 0 {
            aad_size += 4;
        }
        if packet.len() <= aad_size + 4 {
            bail!("Packet too small for computed AAD size {aad_size}");
        }

        let rtp_header = &packet[..aad_size];
        let nonce_start = packet.len() - 4;
        let nonce_raw = &packet[nonce_start..];
        let ct_with_tag = &packet[aad_size..nonce_start];

        match &self.cipher {
            TransportCipher::Aes256GcmRtpSize(cipher) => {
                let mut nonce_12 = [0u8; 12];
                nonce_12[0..4].copy_from_slice(nonce_raw);

                cipher
                    .decrypt(
                        Nonce::from_slice(&nonce_12),
                        Payload {
                            msg: ct_with_tag,
                            aad: rtp_header,
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("AES-GCM decrypt: {e}"))
            }
            TransportCipher::XChaCha20Poly1305RtpSize(cipher) => {
                let mut nonce_24 = [0u8; 24];
                nonce_24[0..4].copy_from_slice(nonce_raw);

                cipher
                    .decrypt(
                        XNonce::from_slice(&nonce_24),
                        Payload {
                            msg: ct_with_tag,
                            aad: rtp_header,
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt: {e}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UDP IP discovery (Discord voice hole-punch)
// ---------------------------------------------------------------------------

async fn ip_discovery(socket: &UdpSocket, ssrc: u32) -> Result<(String, u16)> {
    let mut buf = [0u8; 74];
    // Type=0x0001, Length=70
    buf[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    buf[2..4].copy_from_slice(&70u16.to_be_bytes());
    buf[4..8].copy_from_slice(&ssrc.to_be_bytes());

    socket.send(&buf).await.context("IP discovery send")?;

    let mut resp = [0u8; 74];
    let timeout = time::timeout(Duration::from_secs(5), socket.recv(&mut resp)).await;
    let n = timeout
        .context("IP discovery timeout")?
        .context("IP discovery recv")?;
    if n < 74 {
        bail!("IP discovery response too short: {n} bytes");
    }

    // Response: [type(2) | length(2) | ssrc(4) | address(64) | port(2)]
    let ip_bytes = &resp[8..72];
    let ip = std::str::from_utf8(ip_bytes)
        .context("IP discovery: invalid UTF-8")?
        .trim_end_matches('\0')
        .to_string();
    let port = u16::from_be_bytes([resp[72], resp[73]]);

    info!("IP discovery: external {ip}:{port}");
    Ok((ip, port))
}

// ---------------------------------------------------------------------------
// VoiceConnection — the public handle
// ---------------------------------------------------------------------------

pub struct VoiceConnection {
    pub ssrc: u32,
    shutdown: Arc<AtomicBool>,
    udp_socket: Arc<UdpSocket>,
    crypto: Arc<TransportCrypto>,
    rtp_sequence: AtomicU32,
    timestamp: AtomicU32,
    ws_cmd_tx: mpsc::Sender<WsCommand>,
    ws_read_task: JoinHandle<()>,
    ws_write_task: JoinHandle<()>,
    udp_recv_task: JoinHandle<()>,
}

impl VoiceConnection {
    /// Perform the full voice WS + UDP handshake, then spawn background tasks.
    #[allow(clippy::too_many_lines)]
    pub async fn connect(
        params: VoiceConnectionParams<'_>,
        event_tx: mpsc::Sender<VoiceEvent>,
        dave: Arc<Mutex<Option<DaveManager>>>,
    ) -> Result<Self> {
        let VoiceConnectionParams {
            endpoint,
            guild_id,
            user_id,
            session_id,
            token,
            channel_id,
        } = params;

        let ep = endpoint.trim_start_matches("wss://").trim_end_matches('/');
        let ws_url = format!("wss://{ep}/?v=9");
        info!("Connecting voice WS: {ws_url}");

        let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .context("Voice WS connect failed")?;
        let (mut ws_write, mut ws_read) = ws.split();

        // ---- OP8 Hello ----
        let heartbeat_interval = recv_hello(&mut ws_read).await?;

        // ---- OP0 Identify (advertise DAVE v1 + v9 channel_id support) ----
        let identify = json!({
            "op": 0,
            "d": {
                "server_id": guild_id.to_string(),
                "user_id": user_id.to_string(),
                "session_id": session_id,
                "token": token,
                "channel_id": channel_id.to_string(),
                "max_dave_protocol_version": 1
            }
        });
        ws_write
            .send(Message::Text(identify.to_string()))
            .await
            .context("Send Identify")?;

        // Handshake overflow buffer: messages that arrive during the handshake
        // but aren't the target opcode (e.g. DAVE OP21/OP25 or video state) get
        // buffered here and replayed into the ws_read_loop once background tasks
        // are spawned.
        let mut handshake_overflow: HandshakeOverflow = Vec::new();

        // ---- OP2 Ready ----
        let ready = recv_ready(&mut ws_read, &mut handshake_overflow).await?;
        info!(
            "Voice Ready: ssrc={} udp={}:{} modes={:?} experiments={:?}",
            ready.ssrc, ready.ip, ready.port, ready.modes, ready.experiments
        );

        // ---- UDP socket + IP discovery ----
        let udp = UdpSocket::bind("0.0.0.0:0").await.context("UDP bind")?;
        let voice_addr: SocketAddr = format!("{}:{}", ready.ip, ready.port)
            .parse()
            .context("Parse voice UDP addr")?;
        udp.connect(voice_addr).await.context("UDP connect")?;

        let (external_ip, external_port) = ip_discovery(&udp, ready.ssrc).await?;

        // ---- Select encryption mode ----
        let mode = if ready.modes.iter().any(|m| m == "aead_aes256_gcm_rtpsize") {
            "aead_aes256_gcm_rtpsize"
        } else if ready
            .modes
            .iter()
            .any(|m| m == "aead_xchacha20_poly1305_rtpsize")
        {
            warn!("AES256-GCM RTP-size unavailable; using XChaCha20-Poly1305 RTP-size fallback");
            "aead_xchacha20_poly1305_rtpsize"
        } else {
            bail!(
                "No supported encryption mode (need aead_aes256_gcm_rtpsize or aead_xchacha20_poly1305_rtpsize), got: {:?}",
                ready.modes
            );
        };

        // ---- OP1 Select Protocol ----
        let select =
            build_select_protocol_payload(&external_ip, external_port, mode, &ready.experiments);
        ws_write
            .send(Message::Text(select.to_string()))
            .await
            .context("Send Select Protocol")?;

        // ---- OP4 Session Description ----
        let session_description =
            recv_session_description(&mut ws_read, &mut handshake_overflow).await?;
        let crypto = Arc::new(TransportCrypto::new(&session_description.secret_key, mode)?);
        info!(
            "Voice session established, transport crypto ready, audio_codec={:?}, video_codec={:?}, media_session_id={:?}",
            session_description.audio_codec,
            session_description.video_codec,
            session_description.media_session_id
        );

        let current_video_codec = Arc::new(Mutex::new(None::<String>));
        update_current_video_codec(
            &current_video_codec,
            session_description.video_codec.clone(),
        );

        if session_description.dave_protocol_version > 0 {
            match DaveManager::new(
                session_description.dave_protocol_version,
                user_id,
                channel_id,
            ) {
                Ok((dm, pkg)) => {
                    *dave.lock() = Some(dm);
                    info!(
                        "DaveManager initialized with protocol version {}",
                        session_description.dave_protocol_version
                    );

                    let mut op26_payload = vec![26u8];
                    op26_payload.extend_from_slice(&pkg);
                    ws_write
                        .send(Message::Binary(op26_payload))
                        .await
                        .context("Send DAVE KeyPackage OP26")?;
                    info!("Sent DAVE OP26 KeyPackage to Discord ({} bytes)", pkg.len());
                }
                Err(e) => {
                    error!("Failed to initialize DaveManager: {e}");
                }
            }
        }

        // ---- Spawn background tasks ----
        let shutdown = Arc::new(AtomicBool::new(false));
        let (ws_cmd_tx, ws_cmd_rx) = mpsc::channel::<WsCommand>(128);
        let udp = Arc::new(udp);
        let ssrc_map: Arc<Mutex<HashMap<u32, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let video_ssrc_map: Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ws_sequence = Arc::new(AtomicI32::new(-1));
        let disconnect_sent = Arc::new(AtomicBool::new(false));

        // WS read loop (handles Speaking updates, DAVE opcodes, video stream metadata, etc.)
        let ws_read_task = {
            let shutdown = shutdown.clone();
            let event_tx = event_tx.clone();
            let dave = dave.clone();
            let ws_cmd_tx = ws_cmd_tx.clone();
            let ssrc_map = ssrc_map.clone();
            let video_ssrc_map = video_ssrc_map.clone();
            let ws_sequence = ws_sequence.clone();
            let disconnect_sent = disconnect_sent.clone();
            let current_video_codec = current_video_codec.clone();
            if !handshake_overflow.is_empty() {
                info!(
                    "Replaying {} buffered handshake messages into read loop",
                    handshake_overflow.len()
                );
            }
            tokio::spawn(async move {
                for (i, msg) in handshake_overflow.into_iter().enumerate() {
                    match msg {
                        Message::Text(ref text) => {
                            if let Ok(v) = serde_json::from_str::<Value>(text) {
                                let op = v["op"].as_u64().unwrap_or(u64::MAX);
                                info!("Replay [{i}]: Text OP={op}");
                                let d = &v["d"];
                                handle_text_opcode(
                                    op,
                                    d,
                                    &event_tx,
                                    &ws_cmd_tx,
                                    &dave,
                                    &ssrc_map,
                                    &video_ssrc_map,
                                    &current_video_codec,
                                    user_id,
                                    channel_id,
                                    &ws_sequence,
                                )
                                .await;
                            } else {
                                info!("Replay [{i}]: Invalid Text");
                            }
                        }
                        Message::Binary(ref data) if data.len() >= 3 => {
                            let seq = u16::from_be_bytes([data[0], data[1]]);
                            let op = data[2];
                            info!(
                                "Replay [{}]: Binary OP={} seq={} len={}",
                                i,
                                op,
                                seq,
                                data.len()
                            );
                            handle_binary_opcode(data, &event_tx, &ws_cmd_tx, &dave, &ws_sequence)
                                .await;
                        }
                        Message::Binary(_) => {
                            info!("Replay [{i}]: Empty Binary");
                        }
                        _ => {
                            info!("Replay [{i}]: Other message type");
                        }
                    }
                }
                ws_read_loop(
                    ws_read,
                    event_tx,
                    ws_cmd_tx,
                    dave,
                    ssrc_map,
                    video_ssrc_map,
                    current_video_codec,
                    shutdown,
                    user_id,
                    channel_id,
                    ws_sequence,
                    disconnect_sent,
                )
                .await;
            })
        };

        // WS write loop (heartbeat + outgoing commands)
        let ws_write_task = {
            let shutdown = shutdown.clone();
            let ws_sequence = ws_sequence.clone();
            let event_tx = event_tx.clone();
            let disconnect_sent = disconnect_sent.clone();
            tokio::spawn(async move {
                ws_write_loop(
                    ws_write,
                    ws_cmd_rx,
                    shutdown,
                    heartbeat_interval,
                    ws_sequence,
                    event_tx,
                    disconnect_sent,
                )
                .await;
            })
        };

        // UDP receive loop
        let udp_recv_task = {
            let shutdown = shutdown.clone();
            let event_tx = event_tx.clone();
            let crypto = crypto.clone();
            let dave = dave.clone();
            let udp = udp.clone();
            let ssrc_map = ssrc_map.clone();
            let video_ssrc_map = video_ssrc_map.clone();
            let ws_cmd_tx = ws_cmd_tx.clone();
            let disconnect_sent = disconnect_sent.clone();
            tokio::spawn(async move {
                udp_recv_loop(
                    udp,
                    crypto,
                    dave,
                    ssrc_map,
                    video_ssrc_map,
                    event_tx,
                    ws_cmd_tx,
                    shutdown,
                    disconnect_sent,
                )
                .await;
            })
        };

        // Set speaking state so Discord knows we may transmit audio.
        let _ = ws_cmd_tx
            .send(WsCommand::SendJson(json!({
                "op": 5,
                "d": { "speaking": 1, "delay": 0, "ssrc": ready.ssrc }
            })))
            .await;

        let _ = event_tx.send(VoiceEvent::Ready { ssrc: ready.ssrc }).await;

        Ok(VoiceConnection {
            ssrc: ready.ssrc,
            shutdown,
            udp_socket: udp,
            crypto,
            rtp_sequence: AtomicU32::new(0),
            timestamp: AtomicU32::new(0),
            ws_cmd_tx,
            ws_read_task,
            ws_write_task,
            udp_recv_task,
        })
    }

    /// Build an RTP packet, transport-encrypt, and send via UDP.
    /// `opus_payload` should already be DAVE-encrypted if DAVE is active.
    pub async fn send_rtp_frame(&self, opus_payload: &[u8]) -> Result<()> {
        let seq = self.rtp_sequence.fetch_add(1, Ordering::SeqCst) as u16;
        let ts = self.timestamp.fetch_add(960, Ordering::SeqCst); // 20ms @ 48kHz
        let header = build_rtp_header(seq, ts, self.ssrc);

        let encrypted = self.crypto.encrypt(&header, opus_payload)?;

        let mut packet = Vec::with_capacity(RTP_HEADER_LEN + encrypted.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&encrypted);

        self.udp_socket.send(&packet).await.context("UDP send")?;
        Ok(())
    }

    pub fn update_media_sink_wants(
        &self,
        wants: &[(u32, u8)],
        pixel_counts: &[(u32, f64)],
    ) -> Result<()> {
        let payload = build_media_sink_wants_payload(wants, pixel_counts);
        self.ws_cmd_tx
            .try_send(WsCommand::SendJson(payload))
            .map_err(|error| anyhow::anyhow!("failed to enqueue media sink wants: {error}"))
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.ws_read_task.abort();
        self.ws_write_task.abort();
        self.udp_recv_task.abort();
    }
}

impl Drop for VoiceConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn send_disconnect_once(
    event_tx: &mpsc::Sender<VoiceEvent>,
    disconnect_sent: &Arc<AtomicBool>,
    reason: impl Into<String>,
) {
    if disconnect_sent
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let _ = event_tx
            .send(VoiceEvent::Disconnected {
                reason: reason.into(),
            })
            .await;
    }
}

// ---------------------------------------------------------------------------
// Handshake helpers (synchronous WS reads during connect)
// ---------------------------------------------------------------------------

/// Messages received during the handshake that weren't the target opcode.
/// These are buffered and replayed into the `ws_read_loop` so DAVE opcodes
/// (OP21 text, OP25/27/29/30 binary) that arrive between Ready and Session
/// Description aren't silently dropped.
type HandshakeOverflow = Vec<Message>;

async fn recv_hello(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Result<f64> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = time::timeout_at(deadline, ws.next())
            .await
            .context("Timeout waiting for OP8 Hello")?
            .context("WS stream ended")?
            .context("WS error")?;
        if let Message::Text(text) = msg {
            let message: VoiceOpcode<Value> = parse_voice_opcode(&text)?;
            if message.op == 8 {
                let payload: HelloPayload =
                    serde_json::from_value(message.d).context("invalid hello payload")?;
                return Ok(payload.heartbeat_interval.unwrap_or(13_750.0));
            }
        }
    }
}

async fn recv_ready(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    overflow: &mut HandshakeOverflow,
) -> Result<ReadyPayload> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = time::timeout_at(deadline, ws.next())
            .await
            .context("Timeout waiting for OP2 Ready")?
            .context("WS stream ended")?
            .context("WS error")?;
        match &msg {
            Message::Text(text) => {
                let message: VoiceOpcode<Value> = parse_voice_opcode(text)?;
                if message.op == 2 {
                    let payload: ReadyPayload =
                        serde_json::from_value(message.d).context("invalid ready payload")?;
                    return Ok(payload);
                }
                debug!(
                    "Handshake (waiting OP2): buffered text op={op}",
                    op = message.op
                );
                overflow.push(msg);
            }
            Message::Binary(data) => {
                debug!(
                    "Handshake (waiting OP2): buffered binary opcode={} ({} bytes)",
                    data.first().copied().unwrap_or(0),
                    data.len()
                );
                overflow.push(msg);
            }
            _ => {}
        }
    }
}

async fn recv_session_description(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    overflow: &mut HandshakeOverflow,
) -> Result<SessionDescriptionPayload> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = time::timeout_at(deadline, ws.next())
            .await
            .context("Timeout waiting for OP4 Session Description")?
            .context("WS stream ended")?
            .context("WS error")?;
        match &msg {
            Message::Text(text) => {
                let message: VoiceOpcode<Value> = parse_voice_opcode(text)?;
                if message.op == 4 {
                    let payload: SessionDescriptionPayload = serde_json::from_value(message.d)
                        .context("invalid session description payload")?;
                    return Ok(payload);
                }
                debug!(
                    "Handshake (waiting OP4): buffered text op={op}",
                    op = message.op
                );
                overflow.push(msg);
            }
            Message::Binary(data) => {
                debug!(
                    "Handshake (waiting OP4): buffered binary opcode={} ({} bytes)",
                    data.first().copied().unwrap_or(0),
                    data.len()
                );
                overflow.push(msg);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn ws_read_loop(
    mut ws_read: futures_util::stream::SplitStream<WsStream>,
    event_tx: mpsc::Sender<VoiceEvent>,
    ws_cmd_tx: mpsc::Sender<WsCommand>,
    dave: Arc<Mutex<Option<DaveManager>>>,
    ssrc_map: Arc<Mutex<HashMap<u32, u64>>>,
    video_ssrc_map: Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>>,
    current_video_codec: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    bot_user_id: u64,
    channel_id: u64,
    ws_sequence: Arc<AtomicI32>,
    disconnect_sent: Arc<AtomicBool>,
) {
    while let Some(msg) = ws_read.next().await {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match msg {
            Ok(Message::Text(text)) => {
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Track WebSocket sequence numbers for OP3 Heartbeat
                if let Some(s) = v["seq"].as_i64() {
                    ws_sequence.store(s as i32, Ordering::Relaxed);
                }

                let op = v["op"].as_u64().unwrap_or(u64::MAX);
                let d = &v["d"];
                handle_text_opcode(
                    op,
                    d,
                    &event_tx,
                    &ws_cmd_tx,
                    &dave,
                    &ssrc_map,
                    &video_ssrc_map,
                    &current_video_codec,
                    bot_user_id,
                    channel_id,
                    &ws_sequence,
                )
                .await;
            }
            Ok(Message::Binary(data)) => {
                if data.is_empty() {
                    continue;
                }
                handle_binary_opcode(&data, &event_tx, &ws_cmd_tx, &dave, &ws_sequence).await;
            }
            Ok(Message::Close(frame)) => {
                let reason = match frame {
                    Some(cf) => format!(
                        "WebSocket closed by server: code={} reason={}",
                        cf.code, cf.reason
                    ),
                    None => "WebSocket closed by server (no close frame)".into(),
                };
                warn!("{reason}");
                send_disconnect_once(&event_tx, &disconnect_sent, reason).await;
                break;
            }
            Err(e) => {
                send_disconnect_once(&event_tx, &disconnect_sent, format!("WS read error: {e}"))
                    .await;
                break;
            }
            _ => {}
        }
    }
    info!("Voice WS read loop exited");
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_text_opcode(
    op: u64,
    d: &Value,
    event_tx: &mpsc::Sender<VoiceEvent>,
    ws_cmd_tx: &mpsc::Sender<WsCommand>,
    dave: &Arc<Mutex<Option<DaveManager>>>,
    ssrc_map: &Arc<Mutex<HashMap<u32, u64>>>,
    video_ssrc_map: &Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>>,
    current_video_codec: &Arc<Mutex<Option<String>>>,
    bot_user_id: u64,
    channel_id: u64,
    _ws_sequence: &Arc<AtomicI32>,
) {
    match op {
        // Heartbeat ACK
        6 => {
            debug!("Voice heartbeat ACK");
        }
        // Speaking state update (OP5) — SSRC map only, speaking detection is audio-driven
        5 => {
            let payload: SpeakingPayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, "ignoring malformed speaking payload");
                    return;
                }
            };
            let Some(uid) = parse_user_id(&payload.user_id, "speaking") else {
                return;
            };

            ssrc_map.lock().insert(payload.ssrc, uid);

            let _ = event_tx
                .send(VoiceEvent::SsrcUpdate {
                    ssrc: payload.ssrc,
                    user_id: uid,
                })
                .await;
        }
        // Video stream metadata (Discord may send this as OP12 or OP18 depending on path)
        12 | 18 if d.get("streams").is_some() || d.get("video_ssrc").is_some() => {
            let payload: RemoteVideoStatePayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, op, "ignoring malformed video state payload");
                    return;
                }
            };
            apply_remote_video_state(payload, event_tx, video_ssrc_map, current_video_codec).await;
        }
        // Client disconnect (OP13 in current Discord docs, but some servers historically used OP12)
        12 | 13 => {
            let payload: UserIdPayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, "ignoring malformed client disconnect payload");
                    return;
                }
            };
            let Some(uid) = parse_user_id(&payload.user_id, "client_disconnect") else {
                return;
            };
            ssrc_map.lock().retain(|_, v| *v != uid);
            video_ssrc_map
                .lock()
                .retain(|_, binding| binding.user_id != uid);
            let _ = event_tx
                .send(VoiceEvent::ClientDisconnect { user_id: uid })
                .await;
        }
        // Session update / codec update
        14 => {
            let payload: SessionUpdatePayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, "ignoring malformed session update payload");
                    return;
                }
            };
            if payload.video_codec.is_some() {
                update_current_video_codec(current_video_codec, payload.video_codec.clone());
            }
            debug!(
                audio_codec = ?payload.audio_codec,
                video_codec = ?payload.video_codec,
                media_session_id = ?payload.media_session_id,
                keyframe_interval = ?payload.keyframe_interval,
                "voice session update"
            );
        }
        // OP21: DavePrepareTransition — a transition is upcoming, respond with OP23
        21 => {
            let payload: TransitionPayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, "ignoring malformed DAVE OP21 payload");
                    return;
                }
            };
            info!(
                "DAVE OP21: prepare transition id={} pv={}",
                payload.transition_id, payload.protocol_version
            );
            let send_ready = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    dm.prepare_transition(payload.transition_id, payload.protocol_version)
                } else {
                    false
                }
            };
            if send_ready {
                send_transition_ready(ws_cmd_tx, payload.transition_id, "prepare").await;
            }
        }
        // OP22: DaveExecuteTransition — finalize the pending transition
        22 => {
            let payload: TransitionPayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, "ignoring malformed DAVE OP22 payload");
                    return;
                }
            };
            info!(
                "DAVE OP22: execute transition received, transition_id={}",
                payload.transition_id
            );
            let transitioned = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    dm.execute_transition(payload.transition_id)
                } else {
                    false
                }
            };
            if transitioned {
                let ready = {
                    let guard = dave.lock();
                    guard.as_ref().is_some_and(DaveManager::is_ready)
                };
                if ready {
                    let _ = event_tx.send(VoiceEvent::DaveReady).await;
                }
            }
        }
        // OP24: DavePrepareEpoch — a new DAVE epoch is upcoming
        24 => {
            let payload: EpochPayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(error = %error, "ignoring malformed DAVE OP24 payload");
                    return;
                }
            };
            info!(
                "DAVE OP24: prepare epoch pv={} epoch={}",
                payload.protocol_version, payload.epoch
            );

            if payload.protocol_version > 0 {
                let pkg_to_send = {
                    let mut guard = dave.lock();
                    if guard.is_none() {
                        match DaveManager::new(payload.protocol_version, bot_user_id, channel_id) {
                            Ok((dm, pkg)) => {
                                *guard = Some(dm);
                                Some(pkg)
                            }
                            Err(e) => {
                                error!("Failed to create DaveManager: {e}");
                                None
                            }
                        }
                    } else {
                        if let Some(ref mut dm) = *guard {
                            match dm.reinit() {
                                Ok(recovery) => Some(recovery.key_package),
                                Err(e) => {
                                    error!("Failed to reinit DaveManager for new epoch: {e}");
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    }
                };

                if let Some(pkg) = pkg_to_send {
                    let mut op26_payload = vec![26u8];
                    op26_payload.extend_from_slice(&pkg);
                    let _ = ws_cmd_tx.send(WsCommand::SendBinary(op26_payload)).await;
                    info!(
                        "OP24: Sent DAVE OP26 KeyPackage to Discord ({} bytes)",
                        pkg.len()
                    );
                }
            }
        }
        _ => {
            debug!("Unknown voice WS opcode: {op}");
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_binary_opcode(
    data: &[u8],
    event_tx: &mpsc::Sender<VoiceEvent>,
    ws_cmd_tx: &mpsc::Sender<WsCommand>,
    dave: &Arc<Mutex<Option<DaveManager>>>,
    ws_sequence: &Arc<AtomicI32>,
) {
    // Incoming binary frames from Discord Voice WebSocket have the format:
    // [ seq (2 bytes, BE) | opcode (1 byte) | payload (N bytes) ]
    if data.len() < 3 {
        warn!("Received truncated binary frame (len {})", data.len());
        return;
    }

    let seq = u16::from_be_bytes([data[0], data[1]]);
    ws_sequence.store(i32::from(seq), Ordering::Relaxed);
    let opcode = data[2];
    let payload = &data[3..];
    info!("Handling binary opcode: {} (seq: {})", opcode, seq);

    match opcode {
        // OP25: MLS External Sender Package (server → client)
        25 => {
            info!(
                "DAVE binary OP25: external sender ({} bytes)",
                payload.len()
            );
            let set_sender_ok = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    if let Err(e) = dm.set_external_sender(payload) {
                        error!("DAVE set_external_sender: {e}");
                        false
                    } else {
                        true
                    }
                } else {
                    false
                }
            };

            // We already sent OP26 when the session/epoch was initialized.
            // Sending a second OP26 here can create an extra transition that drifts
            // decrypt state and yields NoValidCryptorFound on inbound audio.
            if set_sender_ok {
                debug!("DAVE: external sender accepted; skipping duplicate OP26");
            }
        }
        // OP27: MLS Proposals (server → client)
        27 => {
            if payload.is_empty() {
                warn!("DAVE binary OP27: truncated payload");
                return;
            }
            let optype = payload[0];
            let proposals_payload = &payload[1..];
            info!(
                "DAVE binary OP27: proposals (optype: {}, {} bytes)",
                optype,
                proposals_payload.len()
            );

            let operation = if optype == 0 {
                davey::ProposalsOperationType::APPEND
            } else {
                davey::ProposalsOperationType::REVOKE
            };

            let response = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    match dm.process_proposals(operation, proposals_payload, None) {
                        Ok(Some(cr)) => Some(cr.data),
                        Ok(None) => {
                            debug!("DAVE: no commit needed for proposals");
                            None
                        }
                        Err(e) => {
                            error!("DAVE process_proposals: {e}");
                            None
                        }
                    }
                } else {
                    None
                }
            };
            if let Some(commit_data) = response {
                let mut frame = Vec::with_capacity(1 + commit_data.len());
                frame.push(28); // OP28
                frame.extend_from_slice(&commit_data);
                let _ = ws_cmd_tx.send(WsCommand::SendBinary(frame)).await;
                debug!("DAVE: sent commit OP28 ({} bytes)", commit_data.len());
            }
        }
        // OP29: MLS Announce Commit Transition (server → client)
        29 => {
            if payload.len() < 2 {
                warn!("DAVE binary OP29: truncated payload");
                return;
            }
            let transition_id = u16::from_be_bytes([payload[0], payload[1]]);
            let commit_payload = &payload[2..];

            info!(
                "DAVE binary OP29: announce commit (transition_id: {}, {} bytes)",
                transition_id,
                commit_payload.len()
            );

            // Process commit under lock, collect any recovery action, then drop lock
            let (ready, success, recovery_action) =
                {
                    let mut guard = dave.lock();
                    if let Some(ref mut dm) = *guard {
                        match dm.process_commit(commit_payload) {
                            Ok(()) => {
                                dm.store_pending_transition(transition_id);
                                (dm.is_ready(), true, None)
                            }
                            Err(e) => {
                                error!("DAVE process_commit: {e}");
                                let recovery = dm.reinit().map_err(|error| {
                                error!(error = %error, "DAVE reinit failed after commit error");
                                error
                            }).ok();
                                (false, false, recovery)
                            }
                        }
                    } else {
                        (false, false, None)
                    }
                };
            // Lock is dropped — safe to await

            if let Some(recovery) = recovery_action {
                send_recovery_action(ws_cmd_tx, recovery, "failed commit").await;
            }

            // Match discord.js behavior: for non-zero transitions, confirm readiness with OP23.
            if success && transition_id != 0 {
                send_transition_ready(ws_cmd_tx, transition_id, "commit").await;
            }

            if ready {
                let _ = event_tx.send(VoiceEvent::DaveReady).await;
            }
        }
        // OP30: MLS Welcome (server → client)
        30 => {
            if payload.len() < 2 {
                warn!("DAVE binary OP30: truncated payload");
                return;
            }
            let transition_id = u16::from_be_bytes([payload[0], payload[1]]);
            let welcome_payload = &payload[2..];

            info!(
                "DAVE binary OP30: welcome (transition_id: {}, {} bytes)",
                transition_id,
                welcome_payload.len()
            );

            // Process welcome under lock, collect any recovery action, then drop lock
            let (ready, success, recovery_action) = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    match dm.process_welcome(welcome_payload) {
                        Ok(()) => {
                            dm.store_pending_transition(transition_id);
                            (dm.is_ready(), true, None)
                        }
                        Err(e) => {
                            if is_already_in_group_error(&e) {
                                // AlreadyInGroup is only benign when we already processed
                                // the corresponding OP29 for this transition id.
                                if dm.has_pending_transition_id(transition_id) {
                                    debug!(
                                        "DAVE process_welcome: AlreadyInGroup for pending transition {} (expected as committer)",
                                        transition_id
                                    );
                                    dm.store_pending_transition(transition_id);
                                    (dm.is_ready(), true, None)
                                } else {
                                    warn!(
                                        "DAVE process_welcome: AlreadyInGroup for non-pending transition {}; ignoring stale welcome",
                                        transition_id
                                    );
                                    (dm.is_ready(), false, None)
                                }
                            } else {
                                error!("DAVE process_welcome failed: {e}");
                                let recovery = dm.reinit().map_err(|error| {
                                    error!(error = %error, "DAVE reinit failed after welcome error");
                                    error
                                }).ok();
                                (false, false, recovery)
                            }
                        }
                    }
                } else {
                    (false, false, None)
                }
            };
            // Lock is dropped — safe to await

            if let Some(recovery) = recovery_action {
                send_recovery_action(ws_cmd_tx, recovery, "failed welcome").await;
            }

            // Match discord.js behavior: for non-zero transitions, confirm readiness with OP23.
            if success && transition_id != 0 {
                send_transition_ready(ws_cmd_tx, transition_id, "welcome").await;
            }

            if ready {
                let _ = event_tx.send(VoiceEvent::DaveReady).await;
            }
        }
        // OP31: MLS Invalid Commit Welcome
        31 => {
            warn!(
                "DAVE binary OP31: invalid commit welcome ({} bytes)",
                payload.len()
            );
        }
        _ => {
            debug!(
                "Unknown binary voice opcode: {} ({} bytes)",
                opcode,
                payload.len()
            );
        }
    }
}

async fn send_transition_ready(
    ws_cmd_tx: &mpsc::Sender<WsCommand>,
    transition_id: u16,
    reason: &str,
) {
    let _ = ws_cmd_tx
        .send(WsCommand::SendJson(json!({
            "op": 23,
            "d": { "transition_id": transition_id }
        })))
        .await;
    info!(
        "DAVE: sent OP23 transition ready for {} transition {}",
        reason, transition_id
    );
}

async fn send_recovery_action(
    ws_cmd_tx: &mpsc::Sender<WsCommand>,
    recovery: crate::dave::RecoveryAction,
    reason: &str,
) {
    let mut op31 = vec![31u8];
    op31.extend_from_slice(&recovery.transition_id.to_be_bytes());
    let _ = ws_cmd_tx.send(WsCommand::SendBinary(op31)).await;

    let mut op26 = vec![26u8];
    op26.extend_from_slice(&recovery.key_package);
    let _ = ws_cmd_tx.send(WsCommand::SendBinary(op26)).await;

    warn!("DAVE: recovery from {}, sent OP31 + OP26", reason);
}

fn try_reinit_dave(
    dave: &Arc<Mutex<Option<DaveManager>>>,
    reason: &str,
) -> Option<crate::dave::RecoveryAction> {
    let mut guard = dave.lock();
    let dm = guard.as_mut()?;

    match dm.reinit() {
        Ok(recovery) => Some(recovery),
        Err(error) => {
            error!(reason, error = %error, "DAVE reinit failed");
            None
        }
    }
}

#[derive(Clone)]
struct VideoFrameCandidate {
    frame: Vec<u8>,
    depacketizer_keyframe: bool,
    used_fallback_payload: bool,
}

struct VideoFrameDecryptOutcome {
    frame: Option<Vec<u8>>,
    depacketizer_keyframe: bool,
    needs_recovery: bool,
}

fn try_decrypt_video_candidate_for_user(
    dm: &mut DaveManager,
    user_id: u64,
    candidates: &[&VideoFrameCandidate],
    ssrc: u32,
    codec: VideoCodecKind,
) -> Option<(Vec<u8>, bool)> {
    for candidate in candidates {
        if let Ok(frame) = dm.decrypt_video(user_id, &candidate.frame) {
            if candidate.used_fallback_payload {
                debug!(
                    user_id,
                    ssrc,
                    codec = codec.as_str(),
                    "UDP: DAVE video decrypt recovered using alternate RTP ext handling"
                );
            }
            return Some((frame, candidate.depacketizer_keyframe));
        }
    }

    None
}

fn decrypt_video_frame_candidates(
    dave: &Arc<Mutex<Option<DaveManager>>>,
    video_ssrc_map: &Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>>,
    binding: &mut RemoteVideoTrackBinding,
    ssrc: u32,
    codec: VideoCodecKind,
    primary_candidate: Option<VideoFrameCandidate>,
    alternate_candidate: Option<VideoFrameCandidate>,
) -> VideoFrameDecryptOutcome {
    let mut ordered_candidates = Vec::new();
    if let Some(primary_candidate) = primary_candidate.as_ref() {
        ordered_candidates.push(primary_candidate);
    }
    if let Some(alternate_candidate) = alternate_candidate.as_ref() {
        let duplicate_of_primary = primary_candidate
            .as_ref()
            .is_some_and(|primary| primary.frame == alternate_candidate.frame);
        if !duplicate_of_primary {
            ordered_candidates.push(alternate_candidate);
        }
    }

    let fallback_candidate = primary_candidate.as_ref().or(alternate_candidate.as_ref());
    let Some(pass_through_candidate) = fallback_candidate else {
        return VideoFrameDecryptOutcome {
            frame: None,
            depacketizer_keyframe: false,
            needs_recovery: false,
        };
    };

    let mut guard = dave.lock();
    match &mut *guard {
        Some(dm) => {
            dm.maybe_auto_execute_downgrade();
            let current_user_id = binding.user_id;
            let can_decrypt = dm.is_ready()
                && (dm.protocol_version() != 0 || dm.can_passthrough(current_user_id));
            if !can_decrypt {
                return VideoFrameDecryptOutcome {
                    frame: Some(pass_through_candidate.frame.clone()),
                    depacketizer_keyframe: pass_through_candidate.depacketizer_keyframe,
                    needs_recovery: false,
                };
            }

            if let Some((frame, depacketizer_keyframe)) = try_decrypt_video_candidate_for_user(
                dm,
                current_user_id,
                &ordered_candidates,
                ssrc,
                codec,
            ) {
                return VideoFrameDecryptOutcome {
                    frame: Some(frame),
                    depacketizer_keyframe,
                    needs_recovery: false,
                };
            }

            for candidate_user_id in dm.known_user_ids() {
                if candidate_user_id == current_user_id || candidate_user_id == dm.user_id() {
                    continue;
                }
                if let Some((frame, depacketizer_keyframe)) = try_decrypt_video_candidate_for_user(
                    dm,
                    candidate_user_id,
                    &ordered_candidates,
                    ssrc,
                    codec,
                ) {
                    if let Some(remapped_binding) = video_ssrc_map.lock().get_mut(&ssrc) {
                        remapped_binding.user_id = candidate_user_id;
                    }
                    debug!(
                        ssrc,
                        codec = codec.as_str(),
                        old_user_id = current_user_id,
                        new_user_id = candidate_user_id,
                        "UDP: remapped video ssrc after successful DAVE decrypt"
                    );
                    binding.user_id = candidate_user_id;
                    return VideoFrameDecryptOutcome {
                        frame: Some(frame),
                        depacketizer_keyframe,
                        needs_recovery: false,
                    };
                }
            }

            debug!(
                user_id = current_user_id,
                ssrc,
                codec = codec.as_str(),
                "UDP drop: DAVE video decrypt failed for all candidate users"
            );
            VideoFrameDecryptOutcome {
                frame: None,
                depacketizer_keyframe: false,
                needs_recovery: dm.track_decrypt_failure(),
            }
        }
        None => VideoFrameDecryptOutcome {
            frame: Some(pass_through_candidate.frame.clone()),
            depacketizer_keyframe: pass_through_candidate.depacketizer_keyframe,
            needs_recovery: false,
        },
    }
}

fn is_already_in_group_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:?}");
    message.contains("AlreadyInGroup") || message.contains("already")
}

async fn ws_write_loop(
    mut ws_write: futures_util::stream::SplitSink<WsStream, Message>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    shutdown: Arc<AtomicBool>,
    heartbeat_interval_ms: f64,
    ws_sequence: Arc<AtomicI32>,
    event_tx: mpsc::Sender<VoiceEvent>,
    disconnect_sent: Arc<AtomicBool>,
) {
    let hb_dur = Duration::from_millis(heartbeat_interval_ms as u64);
    let mut hb_interval = time::interval(hb_dur);
    // Consume first immediate tick so we don't send a heartbeat instantly.
    // Discord expects the first heartbeat after heartbeat_interval * jitter.
    hb_interval.tick().await;

    loop {
        tokio::select! {
            _ = hb_interval.tick() => {
                if shutdown.load(Ordering::Relaxed) { break; }
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                // Read the latest sequence from shared state (-1 means no sequence yet).
                let seq = ws_sequence.load(Ordering::Relaxed);

                let hb = if seq >= 0 {
                    json!({
                        "op": 3,
                        "d": {
                            "t": ts,
                            "seq_ack": seq
                        }
                    })
                } else {
                    json!({
                        "op": 3,
                        "d": {
                            "t": ts
                        }
                    })
                };
                if let Err(error) = ws_write.send(Message::Text(hb.to_string())).await {
                    send_disconnect_once(
                        &event_tx,
                        &disconnect_sent,
                        format!("WS heartbeat send failed: {error}"),
                    )
                    .await;
                    break;
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCommand::SendJson(v)) => {
                        if let Err(error) = ws_write.send(Message::Text(v.to_string())).await {
                            send_disconnect_once(
                                &event_tx,
                                &disconnect_sent,
                                format!("WS command send failed: {error}"),
                            )
                            .await;
                            break;
                        }
                    }
                    Some(WsCommand::SendBinary(data)) => {
                        if let Err(error) = ws_write.send(Message::Binary(data)).await {
                            send_disconnect_once(
                                &event_tx,
                                &disconnect_sent,
                                format!("WS binary send failed: {error}"),
                            )
                            .await;
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    info!("Voice WS write loop exited");
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn udp_recv_loop(
    socket: Arc<UdpSocket>,
    crypto: Arc<TransportCrypto>,
    dave: Arc<Mutex<Option<DaveManager>>>,
    ssrc_map: Arc<Mutex<HashMap<u32, u64>>>,
    video_ssrc_map: Arc<Mutex<HashMap<u32, RemoteVideoTrackBinding>>>,
    event_tx: mpsc::Sender<VoiceEvent>,
    ws_cmd_tx: mpsc::Sender<WsCommand>,
    shutdown: Arc<AtomicBool>,
    disconnect_sent: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 65_536];
    let mut video_depacketizers = VideoDepacketizers::default();
    let mut fallback_video_depacketizers = VideoDepacketizers::default();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let n = match socket.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                send_disconnect_once(&event_tx, &disconnect_sent, format!("UDP recv error: {e}"))
                    .await;
                break;
            }
        };
        let packet = &buf[..n];

        let Some((sequence, timestamp, ssrc, header_size, marker)) = parse_rtp_header(packet)
        else {
            debug!("UDP drop: failed to parse RTP header");
            continue;
        };

        let payload_type = packet[1] & 0x7F;
        if VideoCodecKind::is_rtx_payload_type(payload_type) {
            trace!(
                payload_type,
                ssrc, "UDP drop: RTX payload not yet supported"
            );
            continue;
        }

        let decrypted = match crypto.decrypt(packet, header_size) {
            Ok(p) => p,
            Err(e) => {
                debug!("UDP drop: Transport crypto decrypt failed: {e}");
                continue;
            }
        };

        let Some((primary_payload, fallback_payload)) =
            strip_rtp_extension_payload(packet, decrypted)
        else {
            debug!("UDP drop: RTP extension body exceeds decrypted payload");
            continue;
        };

        if payload_type == OPUS_PT {
            let user_id = ssrc_map.lock().get(&ssrc).copied();
            let fallback_payload = fallback_payload.as_deref();

            let (opus_frame_opt, needs_recovery) = {
                let mut guard = dave.lock();
                match (&mut *guard, user_id) {
                    (Some(dm), Some(uid)) => {
                        dm.maybe_auto_execute_downgrade();

                        let can_decrypt = dm.is_ready()
                            && (dm.protocol_version() != 0 || dm.can_passthrough(uid));
                        if can_decrypt {
                            match dm.decrypt(uid, &primary_payload) {
                                Ok(decrypted) => (Some(decrypted), false),
                                Err(e) => {
                                    let mut recovered: Option<Vec<u8>> = None;

                                    if let Some(alt_payload) = fallback_payload {
                                        if let Ok(decrypted) = dm.decrypt(uid, alt_payload) {
                                            debug!(
                                                "UDP: DAVE audio decrypt recovered for {} using alternate RTP ext handling",
                                                uid
                                            );
                                            recovered = Some(decrypted);
                                        }
                                    }

                                    if recovered.is_none() {
                                        for candidate_uid in dm.known_user_ids() {
                                            if candidate_uid == uid || candidate_uid == dm.user_id()
                                            {
                                                continue;
                                            }
                                            if let Ok(decrypted) =
                                                dm.decrypt(candidate_uid, &primary_payload)
                                            {
                                                ssrc_map.lock().insert(ssrc, candidate_uid);
                                                debug!(
                                                    "UDP: remapped audio ssrc {} from user {} to {} after successful DAVE decrypt",
                                                    ssrc, uid, candidate_uid
                                                );
                                                recovered = Some(decrypted);
                                                break;
                                            }
                                            if let Some(alt_payload) = fallback_payload {
                                                if let Ok(decrypted) =
                                                    dm.decrypt(candidate_uid, alt_payload)
                                                {
                                                    ssrc_map.lock().insert(ssrc, candidate_uid);
                                                    debug!(
                                                        "UDP: remapped audio ssrc {} from user {} to {} with alternate RTP ext handling",
                                                        ssrc, uid, candidate_uid
                                                    );
                                                    recovered = Some(decrypted);
                                                    break;
                                                }
                                            }
                                        }
                                    }

                                    if let Some(decrypted) = recovered {
                                        (Some(decrypted), false)
                                    } else {
                                        debug!(
                                            "UDP drop: DAVE audio decrypt failed for {uid}: {e}"
                                        );
                                        let recovery = dm.track_decrypt_failure();
                                        (None, recovery)
                                    }
                                }
                            }
                        } else {
                            (Some(primary_payload.clone()), false)
                        }
                    }
                    _ => (Some(primary_payload.clone()), false),
                }
            };

            let Some(opus_frame) = opus_frame_opt else {
                if needs_recovery {
                    let recovery = try_reinit_dave(&dave, "udp audio decrypt failures");
                    if let Some(recovery) = recovery {
                        send_recovery_action(&ws_cmd_tx, recovery, "udp audio decrypt failures")
                            .await;
                        warn!(
                            "DAVE: recovery initiated from UDP recv after {} failures",
                            crate::dave::FAILURE_TOLERANCE
                        );
                    }
                }
                continue;
            };

            let _ = event_tx
                .send(VoiceEvent::OpusReceived { ssrc, opus_frame })
                .await;
            continue;
        }

        let Some(codec) = VideoCodecKind::from_payload_type(payload_type) else {
            trace!(payload_type, ssrc, "UDP drop: unsupported RTP payload type");
            continue;
        };

        let Some(mut binding) = video_ssrc_map.lock().get(&ssrc).cloned() else {
            trace!(
                payload_type,
                ssrc, "UDP drop: video packet from unknown ssrc"
            );
            continue;
        };

        let primary_candidate = video_depacketizers
            .push(ssrc, codec, sequence, timestamp, marker, &primary_payload)
            .map(|(frame, depacketizer_keyframe)| VideoFrameCandidate {
                frame,
                depacketizer_keyframe,
                used_fallback_payload: false,
            });
        let alternate_payload = fallback_payload.as_deref().unwrap_or(&primary_payload);
        let alternate_candidate = fallback_video_depacketizers
            .push(ssrc, codec, sequence, timestamp, marker, alternate_payload)
            .map(|(frame, depacketizer_keyframe)| VideoFrameCandidate {
                frame,
                depacketizer_keyframe,
                used_fallback_payload: fallback_payload.is_some(),
            });

        let VideoFrameDecryptOutcome {
            frame: video_frame_opt,
            depacketizer_keyframe,
            needs_recovery,
        } = decrypt_video_frame_candidates(
            &dave,
            &video_ssrc_map,
            &mut binding,
            ssrc,
            codec,
            primary_candidate,
            alternate_candidate,
        );

        let Some(frame) = video_frame_opt else {
            if needs_recovery {
                let recovery = try_reinit_dave(&dave, "udp video decrypt failures");
                if let Some(recovery) = recovery {
                    send_recovery_action(&ws_cmd_tx, recovery, "udp video decrypt failures").await;
                }
            }
            continue;
        };

        let keyframe = match codec {
            VideoCodecKind::H264 => depacketizer_keyframe || h264_annexb_is_keyframe(&frame),
            VideoCodecKind::Vp8 => {
                depacketizer_keyframe || frame.first().is_some_and(|byte| byte & 0x01 == 0)
            }
        };

        let _ = event_tx
            .send(VoiceEvent::VideoFrameReceived {
                user_id: binding.user_id,
                ssrc,
                codec: codec.as_str().to_string(),
                keyframe,
                frame,
                rtp_timestamp: timestamp,
                stream_type: binding.descriptor.stream_type.clone(),
                rid: binding.descriptor.rid.clone(),
            })
            .await;
    }
    info!("UDP recv loop exited");
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use futures_util::stream;
    use parking_lot::Mutex;
    use tokio::sync::mpsc;

    use super::{
        HelloPayload, OPUS_PT, RTP_HEADER_LEN, RemoteVideoStatePayload, RemoteVideoTrackBinding,
        SessionDescriptionPayload, TransportCrypto, VideoCodecKind, VideoDepacketizers,
        VideoFrameCandidate, VideoStreamDescriptor, VoiceEvent, VoiceOpcode,
        apply_remote_video_state, build_rtp_header, decrypt_video_frame_candidates,
        parse_rtp_header, parse_user_id, parse_voice_opcode, recv_ready, recv_session_description,
    };
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn rtp_header_round_trips() {
        let sequence = 321;
        let timestamp = 123_456;
        let ssrc = 987_654_321;

        let header = build_rtp_header(sequence, timestamp, ssrc);
        let parsed = parse_rtp_header(&header).expect("header should parse");

        assert_eq!(parsed, (sequence, timestamp, ssrc, RTP_HEADER_LEN, false));
    }

    #[test]
    fn rtp_header_parses_csrc_and_extension_words() {
        let mut packet = vec![0u8; RTP_HEADER_LEN + 8 + 4 + 8];
        packet[0] = 0x92; // V=2, X=1, CC=2
        packet[1] = OPUS_PT;
        packet[2..4].copy_from_slice(&42u16.to_be_bytes());
        packet[4..8].copy_from_slice(&99u32.to_be_bytes());
        packet[8..12].copy_from_slice(&7u32.to_be_bytes());

        let extension_start = RTP_HEADER_LEN + 8;
        packet[extension_start..extension_start + 2].copy_from_slice(&0xBEDEu16.to_be_bytes());
        packet[extension_start + 2..extension_start + 4].copy_from_slice(&2u16.to_be_bytes());

        let parsed = parse_rtp_header(&packet).expect("header should parse");
        assert_eq!(parsed, (42, 99, 7, RTP_HEADER_LEN + 8 + 4 + 8, false));
    }

    #[test]
    fn aes256_gcm_transport_crypto_round_trips() {
        let crypto = TransportCrypto::new(&[7u8; 32], "aead_aes256_gcm_rtpsize")
            .expect("crypto should initialize");
        let header = build_rtp_header(1, 960, 77);
        let payload = b"opus-frame";

        let encrypted = crypto.encrypt(&header, payload).expect("encrypt");
        let mut packet = Vec::with_capacity(header.len() + encrypted.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&encrypted);

        let decrypted = crypto
            .decrypt(&packet, header.len())
            .expect("decrypt should succeed");
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn xchacha20_transport_crypto_round_trips() {
        let crypto = TransportCrypto::new(&[9u8; 32], "aead_xchacha20_poly1305_rtpsize")
            .expect("crypto should initialize");
        let header = build_rtp_header(2, 1_920, 88);
        let payload = b"another-opus-frame";

        let encrypted = crypto.encrypt(&header, payload).expect("encrypt");
        let mut packet = Vec::with_capacity(header.len() + encrypted.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&encrypted);

        let decrypted = crypto
            .decrypt(&packet, header.len())
            .expect("decrypt should succeed");
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn parse_voice_opcode_rejects_invalid_secret_key_bytes() {
        let text = r#"{"op":4,"d":{"secret_key":[1,999],"dave_protocol_version":1}}"#;

        let parsed = parse_voice_opcode::<SessionDescriptionPayload>(text);
        assert!(parsed.is_err());
    }

    #[test]
    fn parse_voice_opcode_reads_hello_payload() {
        let text = r#"{"op":8,"d":{"heartbeat_interval":2500.0}}"#;

        let parsed: VoiceOpcode<HelloPayload> = parse_voice_opcode(text).expect("hello payload");
        assert_eq!(parsed.op, 8);
        assert_eq!(parsed.d.heartbeat_interval, Some(2500.0));
    }

    #[test]
    fn parse_user_id_rejects_non_numeric_values() {
        assert_eq!(parse_user_id("42", "test"), Some(42));
        assert_eq!(parse_user_id("bad", "test"), None);
    }

    #[tokio::test]
    async fn recv_ready_buffers_non_target_text_frames() {
        let mut ws = stream::iter(vec![
            Ok(Message::Text(r#"{"op":6,"d":{}}"#.into())),
            Ok(Message::Text(
                r#"{"op":2,"d":{"ssrc":9689,"ip":"104.29.137.71","port":19296,"modes":["aead_aes256_gcm_rtpsize"]}}"#
                    .into(),
            )),
        ]);
        let mut overflow = Vec::new();

        let ready = recv_ready(&mut ws, &mut overflow)
            .await
            .expect("ready payload");

        assert_eq!(ready.ssrc, 9689);
        assert_eq!(ready.ip, "104.29.137.71");
        assert_eq!(ready.port, 19296);
        assert_eq!(ready.modes, vec!["aead_aes256_gcm_rtpsize"]);
        assert_eq!(overflow.len(), 1);
    }

    #[tokio::test]
    async fn recv_session_description_buffers_non_target_text_frames() {
        let mut ws = stream::iter(vec![
            Ok(Message::Text(r#"{"op":18,"d":{"streams":[]}}"#.into())),
            Ok(Message::Text(
                r#"{"op":4,"d":{"secret_key":[1,2,3,4],"dave_protocol_version":1}}"#.into(),
            )),
        ]);
        let mut overflow = Vec::new();

        let session_description = recv_session_description(&mut ws, &mut overflow)
            .await
            .expect("session description payload");

        assert_eq!(session_description.secret_key, vec![1, 2, 3, 4]);
        assert_eq!(session_description.dave_protocol_version, 1);
        assert_eq!(overflow.len(), 1);
    }

    #[test]
    fn h264_video_depacketizer_resets_on_sequence_gap() {
        let mut depacketizers = VideoDepacketizers::default();
        let ssrc = 777u32;
        let timestamp = 90_000u32;

        let start_fragment = [0x7C, 0x85, 0xAA];
        assert_eq!(
            depacketizers.push(
                ssrc,
                VideoCodecKind::H264,
                10,
                timestamp,
                false,
                &start_fragment
            ),
            None
        );

        let end_fragment = [0x7C, 0x45, 0xBB];
        assert_eq!(
            depacketizers.push(
                ssrc,
                VideoCodecKind::H264,
                12,
                timestamp,
                true,
                &end_fragment
            ),
            None
        );

        let next_frame = [0x65, 0xCC];
        let (frame, keyframe) = depacketizers
            .push(
                ssrc,
                VideoCodecKind::H264,
                13,
                timestamp.wrapping_add(3_000),
                true,
                &next_frame,
            )
            .expect("standalone h264 packet should survive after gap reset");

        assert_eq!(frame, vec![0, 0, 0, 1, 0x65, 0xCC]);
        assert!(keyframe);
    }

    #[test]
    fn vp8_video_depacketizer_resets_on_sequence_gap() {
        let mut depacketizers = VideoDepacketizers::default();
        let ssrc = 778u32;
        let timestamp = 45_000u32;

        let start_packet = [0x10, 0x00, 0xAA];
        assert_eq!(
            depacketizers.push(
                ssrc,
                VideoCodecKind::Vp8,
                30,
                timestamp,
                false,
                &start_packet
            ),
            None
        );

        let continuation_packet = [0x00, 0xBB];
        assert_eq!(
            depacketizers.push(
                ssrc,
                VideoCodecKind::Vp8,
                32,
                timestamp,
                true,
                &continuation_packet,
            ),
            None
        );

        let next_frame_packet = [0x10, 0x00, 0xCC];
        let (frame, keyframe) = depacketizers
            .push(
                ssrc,
                VideoCodecKind::Vp8,
                33,
                timestamp.wrapping_add(3_000),
                true,
                &next_frame_packet,
            )
            .expect("single-packet vp8 frame should survive after gap reset");

        assert_eq!(frame, vec![0x00, 0xCC]);
        assert!(keyframe);
    }

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

    #[test]
    fn decrypt_video_frame_candidates_prefers_primary_candidate_without_dave() {
        let descriptor = VideoStreamDescriptor {
            ssrc: 4201,
            rtx_ssrc: None,
            rid: None,
            quality: None,
            stream_type: Some("screen".into()),
            active: Some(true),
            max_bitrate: None,
            max_framerate: None,
            max_resolution: None,
        };
        let dave = Arc::new(Mutex::new(None));
        let video_ssrc_map = Arc::new(Mutex::new(HashMap::from([(
            descriptor.ssrc,
            RemoteVideoTrackBinding {
                user_id: 42,
                descriptor: descriptor.clone(),
            },
        )])));
        let mut binding = RemoteVideoTrackBinding {
            user_id: 42,
            descriptor,
        };

        let outcome = decrypt_video_frame_candidates(
            &dave,
            &video_ssrc_map,
            &mut binding,
            4201,
            VideoCodecKind::H264,
            Some(VideoFrameCandidate {
                frame: vec![1, 2, 3],
                depacketizer_keyframe: true,
                used_fallback_payload: false,
            }),
            Some(VideoFrameCandidate {
                frame: vec![9, 9, 9],
                depacketizer_keyframe: false,
                used_fallback_payload: true,
            }),
        );

        assert_eq!(outcome.frame, Some(vec![1, 2, 3]));
        assert!(outcome.depacketizer_keyframe);
        assert!(!outcome.needs_recovery);
        assert_eq!(binding.user_id, 42);
    }

    #[test]
    fn decrypt_video_frame_candidates_uses_alternate_candidate_without_dave() {
        let descriptor = VideoStreamDescriptor {
            ssrc: 4301,
            rtx_ssrc: None,
            rid: None,
            quality: None,
            stream_type: Some("screen".into()),
            active: Some(true),
            max_bitrate: None,
            max_framerate: None,
            max_resolution: None,
        };
        let dave = Arc::new(Mutex::new(None));
        let video_ssrc_map = Arc::new(Mutex::new(HashMap::from([(
            descriptor.ssrc,
            RemoteVideoTrackBinding {
                user_id: 42,
                descriptor: descriptor.clone(),
            },
        )])));
        let mut binding = RemoteVideoTrackBinding {
            user_id: 42,
            descriptor,
        };

        let outcome = decrypt_video_frame_candidates(
            &dave,
            &video_ssrc_map,
            &mut binding,
            4301,
            VideoCodecKind::Vp8,
            None,
            Some(VideoFrameCandidate {
                frame: vec![7, 8, 9],
                depacketizer_keyframe: true,
                used_fallback_payload: true,
            }),
        );

        assert_eq!(outcome.frame, Some(vec![7, 8, 9]));
        assert!(outcome.depacketizer_keyframe);
        assert!(!outcome.needs_recovery);
        assert_eq!(binding.user_id, 42);
    }
}

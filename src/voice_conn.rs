use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, trace, warn};

use crate::dave::DaveManager;
use crate::h264::{H264Depacketizer, collect_annexb_nal_types, h264_annexb_has_idr_slice, split_h264_annexb_nalus};
use crate::media_sink_wants::build_media_sink_wants_payload;
use crate::rtcp::build_protected_rtcp_packet;
use crate::rtp::{
    RTP_HEADER_LEN, OPUS_PT,
    MAX_VIDEO_RTP_CHUNK_BYTES, VIDEO_RTP_EXTENSION_HEADER, VIDEO_RTP_EXTENSION_PAYLOAD,
    VideoCodecKind, build_rtp_header, build_video_rtp_header, parse_rtp_header,
    strip_rtp_extension_payload,
};
use crate::transport_crypto::TransportCrypto;
use crate::video::{VideoResolution, VideoStreamDescriptor};
use crate::video_state::{
    RemoteVideoStreamPayload, RemoteVideoStatePayload,
    RemoteVideoTrackBinding, apply_remote_video_state, build_video_state_announcement,
    convert_video_stream_descriptor, update_current_video_codec,
};
use crate::vp8::Vp8Depacketizer;

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
    #[serde(default)]
    video_ssrc: Option<u32>,
    #[serde(default)]
    streams: Vec<RemoteVideoStreamPayload>,
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

fn parse_voice_opcode<T>(text: &str) -> Result<VoiceOpcode<T>>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(text).context("invalid voice gateway payload")
}

pub(crate) fn parse_user_id(user_id: &str, context: &str) -> Option<u64> {
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
        role: TransportRole,
        ssrc: u32,
    },
    SsrcUpdate {
        role: TransportRole,
        ssrc: u32,
        user_id: u64,
    },
    VideoStateUpdate {
        role: TransportRole,
        user_id: u64,
        audio_ssrc: Option<u32>,
        video_ssrc: Option<u32>,
        codec: Option<String>,
        streams: Vec<VideoStreamDescriptor>,
    },
    ClientDisconnect {
        role: TransportRole,
        user_id: u64,
    },
    OpusReceived {
        role: TransportRole,
        ssrc: u32,
        opus_frame: Vec<u8>,
    },
    VideoFrameReceived {
        role: TransportRole,
        user_id: u64,
        ssrc: u32,
        codec: String,
        keyframe: bool,
        frame: Vec<u8>,
        rtp_timestamp: u32,
        stream_type: Option<String>,
        rid: Option<String>,
        dave_decrypted: bool,
    },
    DaveReady {
        role: TransportRole,
    },
    Disconnected {
        role: TransportRole,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportRole {
    Voice,
    StreamWatch,
    StreamPublish,
}

impl TransportRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::StreamWatch => "stream_watch",
            Self::StreamPublish => "stream_publish",
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

    /// Prepend cached SPS+PPS from the depacketizer to a frame.
    /// Called AFTER DAVE decrypt so the DAVE trailer's unencrypted ranges
    /// reference the correct byte offsets in the original frame.
    fn prepend_cached_h264_params(&self, ssrc: u32, frame: Vec<u8>) -> Vec<u8> {
        if let Some(state) = self.by_ssrc.get(&ssrc) {
            state.h264.prepend_cached_parameter_sets(frame)
        } else {
            frame
        }
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

fn ready_video_stream_descriptors(ready: &ReadyPayload) -> Vec<VideoStreamDescriptor> {
    ready
        .streams
        .clone()
        .into_iter()
        .filter_map(convert_video_stream_descriptor)
        .collect()
}

fn default_publish_video_stream_descriptor(video_ssrc: u32) -> VideoStreamDescriptor {
    VideoStreamDescriptor {
        ssrc: video_ssrc,
        rtx_ssrc: None,
        rid: Some("100".to_string()),
        quality: Some(100),
        stream_type: Some("screen".to_string()),
        active: Some(true),
        max_bitrate: Some(2_500_000),
        max_framerate: Some(30),
        max_resolution: Some(VideoResolution {
            width: Some(1280),
            height: Some(720),
            resolution_type: Some("fixed".to_string()),
        }),
    }
}

fn ready_publish_video_stream_descriptors(ready: &ReadyPayload) -> Vec<VideoStreamDescriptor> {
    let streams = ready_video_stream_descriptors(ready);
    if !streams.is_empty() {
        return streams;
    }
    ready
        .video_ssrc
        .filter(|ssrc| *ssrc != 0)
        .map(default_publish_video_stream_descriptor)
        .into_iter()
        .collect()
}

fn build_inactive_video_state_announcement(audio_ssrc: u32, ready: &ReadyPayload) -> Option<Value> {
    let streams = ready_video_stream_descriptors(ready);
    build_video_state_announcement(audio_ssrc, &streams, false)
}

fn json_object_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
}

fn build_select_protocol_payload(
    external_ip: &str,
    external_port: u16,
    mode: &str,
    experiments: &[String],
    role: TransportRole,
) -> Value {
    let video_codecs = match role {
        TransportRole::StreamPublish => vec![json!({
            "name": VideoCodecKind::H264.as_str(),
            "type": "video",
            "priority": 900,
            "payload_type": VideoCodecKind::H264.payload_type(),
            "rtx_payload_type": VideoCodecKind::H264.rtx_payload_type(),
            "encode": true,
            "decode": false,
        })],
        TransportRole::Voice | TransportRole::StreamWatch => {
            [VideoCodecKind::H264, VideoCodecKind::Vp8]
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
                .collect::<Vec<_>>()
        }
    };

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

pub struct VoiceConnectionParams<'a> {
    pub endpoint: &'a str,
    pub server_id: u64,
    pub user_id: u64,
    pub session_id: &'a str,
    pub token: &'a str,
    pub dave_channel_id: u64,
    pub role: TransportRole,
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
    role: TransportRole,
    shutdown: Arc<AtomicBool>,
    udp_socket: Arc<UdpSocket>,
    crypto: Arc<TransportCrypto>,
    rtp_sequence: AtomicU32,
    timestamp: AtomicU32,
    video_payload_type: u8,
    video_ssrc: Option<u32>,
    video_streams: Vec<VideoStreamDescriptor>,
    video_sequence: AtomicU32,
    video_timestamp: AtomicU32,
    fir_sequence: AtomicU32,
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
            server_id,
            user_id,
            session_id,
            token,
            dave_channel_id,
            role,
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

        // ---- OP0 Identify (advertise DAVE v1 + v9 channel_id + video receive) ----
        let identify = json!({
            "op": 0,
            "d": {
                "server_id": server_id.to_string(),
                "user_id": user_id.to_string(),
                "session_id": session_id,
                "token": token,
                "channel_id": dave_channel_id.to_string(),
                "max_dave_protocol_version": 1,
                "video": true,
                "streams": [
                    { "type": "screen", "rid": "100", "quality": 100 }
                ]
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
        let ready_stream_ssrcs = ready
            .streams
            .iter()
            .filter_map(|stream| stream.ssrc.filter(|ssrc| *ssrc != 0))
            .collect::<Vec<_>>();
        info!(
            ssrc = ready.ssrc,
            video_ssrc = ready.video_ssrc,
            ready_stream_count = ready_stream_ssrcs.len(),
            ready_stream_ssrcs = ?ready_stream_ssrcs,
            udp_ip = %ready.ip,
            udp_port = ready.port,
            modes = ?ready.modes,
            experiments = ?ready.experiments,
            "clankvox_voice_ready"
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
        let select = build_select_protocol_payload(
            &external_ip,
            external_port,
            mode,
            &ready.experiments,
            role,
        );
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
        if role == TransportRole::StreamPublish
            && session_description
                .video_codec
                .as_deref()
                .is_some_and(|codec| !codec.eq_ignore_ascii_case("h264"))
        {
            bail!(
                "stream publish negotiated unsupported video codec {:?}",
                session_description.video_codec
            );
        }

        let current_video_codec = Arc::new(Mutex::new(None::<String>));
        update_current_video_codec(
            &current_video_codec,
            session_description.video_codec.clone(),
        );

        if session_description.dave_protocol_version > 0 {
            match DaveManager::new(
                session_description.dave_protocol_version,
                user_id,
                dave_channel_id,
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
                                    dave_channel_id,
                                    role,
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
                            handle_binary_opcode(
                                data,
                                &event_tx,
                                &ws_cmd_tx,
                                &dave,
                                role,
                                &ws_sequence,
                            )
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
                    dave_channel_id,
                    role,
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
                    role,
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
                    role,
                    disconnect_sent,
                )
                .await;
            })
        };

        if role == TransportRole::Voice {
            // Set speaking state so Discord knows we may transmit audio.
            let _ = ws_cmd_tx
                .send(WsCommand::SendJson(json!({
                    "op": 5,
                    "d": { "speaking": 1, "delay": 0, "ssrc": ready.ssrc }
                })))
                .await;
        }

        // Announce video capability (OP12) so Discord sends us other users' video states.
        // We declare our streams as inactive (we only receive, not send video).
        if let Some(video_state_announcement) =
            build_inactive_video_state_announcement(ready.ssrc, &ready)
        {
            let announced_video_ssrc = video_state_announcement["d"]["video_ssrc"].as_u64();
            let announced_stream_ssrcs = video_state_announcement["d"]["streams"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|stream| stream["ssrc"].as_u64())
                .collect::<Vec<_>>();
            info!(
                audio_ssrc = ready.ssrc,
                announced_video_ssrc,
                announced_stream_count = announced_stream_ssrcs.len(),
                announced_stream_ssrcs = ?announced_stream_ssrcs,
                "clankvox_sending_inactive_video_state_announcement"
            );
            let _ = ws_cmd_tx
                .send(WsCommand::SendJson(video_state_announcement))
                .await;
        } else {
            info!("No usable stream metadata in OP2 Ready, skipping OP12 video state announcement");
        }

        let _ = event_tx
            .send(VoiceEvent::Ready {
                role,
                ssrc: ready.ssrc,
            })
            .await;

        Ok(VoiceConnection {
            ssrc: ready.ssrc,
            role,
            shutdown,
            udp_socket: udp,
            crypto,
            rtp_sequence: AtomicU32::new(0),
            timestamp: AtomicU32::new(0),
            video_payload_type: VideoCodecKind::H264.payload_type(),
            video_ssrc: ready.video_ssrc.filter(|ssrc| *ssrc != 0).or_else(|| {
                ready_publish_video_stream_descriptors(&ready)
                    .first()
                    .map(|stream| stream.ssrc)
            }),
            video_streams: match role {
                TransportRole::StreamPublish => ready_publish_video_stream_descriptors(&ready),
                TransportRole::Voice | TransportRole::StreamWatch => {
                    ready_video_stream_descriptors(&ready)
                }
            },
            video_sequence: AtomicU32::new(0),
            video_timestamp: AtomicU32::new(0),
            fir_sequence: AtomicU32::new(0),
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

    pub async fn send_h264_frame(
        &self,
        access_unit: &[u8],
        timestamp_increment: u32,
    ) -> Result<()> {
        let Some(video_ssrc) = self.video_ssrc else {
            bail!("stream publish video_ssrc unavailable");
        };

        let nalus = split_h264_annexb_nalus(access_unit);
        if nalus.is_empty() {
            bail!("stream publish frame did not contain Annex-B NAL units");
        }

        let timestamp = self
            .video_timestamp
            .fetch_add(timestamp_increment.max(1), Ordering::SeqCst);

        for (nal_index, nal) in nalus.iter().enumerate() {
            if nal.is_empty() {
                continue;
            }
            let is_last_nal = nal_index + 1 == nalus.len();
            let max_single_nal_payload =
                MAX_VIDEO_RTP_CHUNK_BYTES.saturating_sub(VIDEO_RTP_EXTENSION_PAYLOAD.len());
            if nal.len() <= max_single_nal_payload {
                let seq = self.video_sequence.fetch_add(1, Ordering::SeqCst) as u16;
                let header = build_video_rtp_header(
                    self.video_payload_type,
                    seq,
                    timestamp,
                    video_ssrc,
                    is_last_nal,
                );
                let mut aad = Vec::with_capacity(RTP_HEADER_LEN + VIDEO_RTP_EXTENSION_HEADER.len());
                aad.extend_from_slice(&header);
                aad.extend_from_slice(&VIDEO_RTP_EXTENSION_HEADER);
                let mut payload = Vec::with_capacity(VIDEO_RTP_EXTENSION_PAYLOAD.len() + nal.len());
                payload.extend_from_slice(&VIDEO_RTP_EXTENSION_PAYLOAD);
                payload.extend_from_slice(nal);
                let encrypted = self.crypto.encrypt(&aad, &payload)?;
                let mut packet = Vec::with_capacity(
                    RTP_HEADER_LEN + VIDEO_RTP_EXTENSION_HEADER.len() + encrypted.len(),
                );
                packet.extend_from_slice(&header);
                packet.extend_from_slice(&VIDEO_RTP_EXTENSION_HEADER);
                packet.extend_from_slice(&encrypted);
                self.udp_socket
                    .send(&packet)
                    .await
                    .context("UDP send video packet")?;
                continue;
            }

            let nal_header = nal[0];
            let nal_type = nal_header & 0x1f;
            let fnri = nal_header & 0xe0;
            let fu_indicator = fnri | 28;
            let max_fu_payload = MAX_VIDEO_RTP_CHUNK_BYTES
                .saturating_sub(VIDEO_RTP_EXTENSION_PAYLOAD.len())
                .saturating_sub(2);
            for (chunk_index, chunk) in nal[1..].chunks(max_fu_payload).enumerate() {
                let is_first_chunk = chunk_index == 0;
                let chunk_start = chunk_index * max_fu_payload;
                let is_last_chunk = chunk_start + chunk.len() >= nal.len().saturating_sub(1);
                let marker = is_last_nal && is_last_chunk;
                let seq = self.video_sequence.fetch_add(1, Ordering::SeqCst) as u16;
                let header = build_video_rtp_header(
                    self.video_payload_type,
                    seq,
                    timestamp,
                    video_ssrc,
                    marker,
                );
                let fu_header = (if is_first_chunk { 0x80 } else { 0x00 })
                    | (if is_last_chunk { 0x40 } else { 0x00 })
                    | nal_type;
                let mut aad = Vec::with_capacity(RTP_HEADER_LEN + VIDEO_RTP_EXTENSION_HEADER.len());
                aad.extend_from_slice(&header);
                aad.extend_from_slice(&VIDEO_RTP_EXTENSION_HEADER);
                let mut payload =
                    Vec::with_capacity(VIDEO_RTP_EXTENSION_PAYLOAD.len() + 2 + chunk.len());
                payload.extend_from_slice(&VIDEO_RTP_EXTENSION_PAYLOAD);
                payload.extend_from_slice(&[fu_indicator, fu_header]);
                payload.extend_from_slice(chunk);
                let encrypted = self.crypto.encrypt(&aad, &payload)?;
                let mut packet = Vec::with_capacity(
                    RTP_HEADER_LEN + VIDEO_RTP_EXTENSION_HEADER.len() + encrypted.len(),
                );
                packet.extend_from_slice(&header);
                packet.extend_from_slice(&VIDEO_RTP_EXTENSION_HEADER);
                packet.extend_from_slice(&encrypted);
                self.udp_socket
                    .send(&packet)
                    .await
                    .context("UDP send video FU-A packet")?;
            }
        }

        Ok(())
    }

    pub fn set_stream_publish_speaking(&self, speaking: bool) -> Result<()> {
        if self.role != TransportRole::StreamPublish {
            return Ok(());
        }
        self.ws_cmd_tx
            .try_send(WsCommand::SendJson(json!({
                "op": 5,
                "d": {
                    "speaking": if speaking { 2 } else { 0 },
                    "delay": 0,
                    "ssrc": self.ssrc,
                }
            })))
            .map_err(|error| {
                anyhow::anyhow!("failed to enqueue stream publish speaking update: {error}")
            })
    }

    pub fn set_stream_publish_video_active(&self, active: bool) -> Result<()> {
        if self.role != TransportRole::StreamPublish {
            return Ok(());
        }
        let Some(payload) = build_video_state_announcement(self.ssrc, &self.video_streams, active)
        else {
            return Ok(());
        };
        self.ws_cmd_tx
            .try_send(WsCommand::SendJson(payload))
            .map_err(|error| {
                anyhow::anyhow!("failed to enqueue stream publish video state update: {error}")
            })
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

    fn send_protected_rtcp_packet(
        &self,
        fmt_or_count: u8,
        packet_type: u8,
        body: &[u8],
        packet_label: &'static str,
    ) -> Result<usize> {
        let packet = build_protected_rtcp_packet(&self.crypto, fmt_or_count, packet_type, body)
            .with_context(|| format!("RTCP {packet_label} transport encrypt"))?;
        self.udp_socket
            .try_send(&packet)
            .with_context(|| format!("RTCP {packet_label} send"))?;
        Ok(packet.len())
    }

    /// Send protected RTCP feedback packets containing:
    ///   1. RR (Receiver Report)
    ///   2. PLI (Picture Loss Indication, RFC 4585)
    ///   3. FIR (Full Intra Request, RFC 5104)
    ///
    /// Under Discord's `rtpsize` modes, feedback rides the same transport
    /// protection as media. Each RTCP packet is protected independently so its
    /// header length still matches the on-wire packet bytes.
    pub fn send_rtcp_pli(&self, media_ssrc: u32) -> Result<()> {
        let fir_seq = self.fir_sequence.fetch_add(1, Ordering::Relaxed) as u8;

        let rr_body = self.ssrc.to_be_bytes();

        let mut pli_body = [0u8; 8];
        pli_body[0..4].copy_from_slice(&self.ssrc.to_be_bytes());
        pli_body[4..8].copy_from_slice(&media_ssrc.to_be_bytes());

        let mut fir_body = [0u8; 16];
        fir_body[0..4].copy_from_slice(&self.ssrc.to_be_bytes());
        fir_body[4..8].copy_from_slice(&0u32.to_be_bytes()); // media source = 0 for FIR
        fir_body[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
        fir_body[12] = fir_seq;

        let rr_packet_len = self.send_protected_rtcp_packet(0, 201, &rr_body, "rr")?;
        let pli_packet_len = self.send_protected_rtcp_packet(1, 206, &pli_body, "pli")?;
        let fir_packet_len = self.send_protected_rtcp_packet(4, 206, &fir_body, "fir")?;
        info!(
            sender_ssrc = self.ssrc,
            media_ssrc,
            fir_seq,
            rr_packet_len,
            pli_packet_len,
            fir_packet_len,
            "clankvox_rtcp_pli_sent"
        );
        Ok(())
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
    role: TransportRole,
    reason: impl Into<String>,
) {
    if disconnect_sent
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let _ = event_tx
            .send(VoiceEvent::Disconnected {
                role,
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
    role: TransportRole,
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
                    role,
                    &ws_sequence,
                )
                .await;
            }
            Ok(Message::Binary(data)) => {
                if data.is_empty() {
                    continue;
                }
                handle_binary_opcode(&data, &event_tx, &ws_cmd_tx, &dave, role, &ws_sequence).await;
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
                send_disconnect_once(&event_tx, &disconnect_sent, role, reason).await;
                break;
            }
            Err(e) => {
                send_disconnect_once(
                    &event_tx,
                    &disconnect_sent,
                    role,
                    format!("WS read error: {e}"),
                )
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
    role: TransportRole,
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
                    role,
                    ssrc: payload.ssrc,
                    user_id: uid,
                })
                .await;
        }
        // Video stream metadata (Discord may send this as OP12 or OP18 depending on path)
        12 | 18 => {
            let has_streams = d.get("streams").is_some();
            let has_video_ssrc = d.get("video_ssrc").is_some();
            let has_audio_ssrc = d.get("audio_ssrc").is_some();
            let has_user_id = d.get("user_id").is_some();
            let payload_keys = json_object_keys(d);

            if has_streams || has_video_ssrc {
                let payload: RemoteVideoStatePayload = match serde_json::from_value(d.clone()) {
                    Ok(payload) => payload,
                    Err(error) => {
                        warn!(
                            error = %error,
                            op,
                            has_streams,
                            has_video_ssrc,
                            has_audio_ssrc,
                            has_user_id,
                            payload_keys = ?payload_keys,
                            "ignoring malformed video state payload"
                        );
                        return;
                    }
                };
                apply_remote_video_state(
                    payload,
                    event_tx,
                    video_ssrc_map,
                    current_video_codec,
                    role,
                )
                .await;
                return;
            }

            if op == 18 {
                info!(
                    has_streams,
                    has_video_ssrc,
                    has_audio_ssrc,
                    has_user_id,
                    payload_keys = ?payload_keys,
                    "clankvox_voice_ws_unclassified_op18"
                );
                return;
            }

            // Client disconnect (OP13 in current Discord docs, but some servers historically used OP12)
            let payload: UserIdPayload = match serde_json::from_value(d.clone()) {
                Ok(payload) => payload,
                Err(error) => {
                    warn!(
                        error = %error,
                        op,
                        has_streams,
                        has_video_ssrc,
                        has_audio_ssrc,
                        has_user_id,
                        payload_keys = ?payload_keys,
                        "ignoring malformed client disconnect payload"
                    );
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
                .send(VoiceEvent::ClientDisconnect { role, user_id: uid })
                .await;
        }
        13 => {
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
                .send(VoiceEvent::ClientDisconnect { role, user_id: uid })
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
                    let _ = event_tx.send(VoiceEvent::DaveReady { role }).await;
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
    role: TransportRole,
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
                                info!(
                                    role = role.as_str(),
                                    transition_id,
                                    known_users = ?dm.known_user_ids(),
                                    pv = dm.protocol_version(),
                                    ready = dm.is_ready(),
                                    "DAVE: commit processed, MLS group members"
                                );
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
                let _ = event_tx.send(VoiceEvent::DaveReady { role }).await;
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
                            info!(
                                role = role.as_str(),
                                transition_id,
                                known_users = ?dm.known_user_ids(),
                                pv = dm.protocol_version(),
                                ready = dm.is_ready(),
                                "DAVE: welcome processed, MLS group members"
                            );
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
                let _ = event_tx.send(VoiceEvent::DaveReady { role }).await;
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
    /// True only when DAVE successfully decrypted the frame (not passthrough).
    dave_decrypted: bool,
}

fn ordered_audio_candidate_user_ids(
    current_user_id: Option<u64>,
    bot_user_id: u64,
    known_user_ids: &[u64],
) -> Vec<u64> {
    let mut ordered = Vec::new();
    if let Some(current_user_id) = current_user_id {
        if current_user_id != bot_user_id {
            ordered.push(current_user_id);
        }
    }

    for &candidate_user_id in known_user_ids {
        if candidate_user_id == bot_user_id
            || Some(candidate_user_id) == current_user_id
            || ordered.contains(&candidate_user_id)
        {
            continue;
        }
        ordered.push(candidate_user_id);
    }

    ordered
}

fn try_decrypt_audio_payload_for_user(
    dm: &mut DaveManager,
    user_id: u64,
    primary_payload: &[u8],
    fallback_payload: Option<&[u8]>,
    ssrc: u32,
) -> Option<(Vec<u8>, bool)> {
    let can_decrypt = dm.is_ready() && (dm.protocol_version() != 0 || dm.can_passthrough(user_id));
    if !can_decrypt {
        return None;
    }

    if let Ok(decrypted) = dm.decrypt(user_id, primary_payload) {
        return Some((decrypted, false));
    }

    if let Some(fallback_payload) = fallback_payload {
        if let Ok(decrypted) = dm.decrypt(user_id, fallback_payload) {
            debug!(
                user_id,
                ssrc, "UDP: DAVE audio decrypt recovered using alternate RTP ext handling"
            );
            return Some((decrypted, true));
        }
    }

    None
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
            dave_decrypted: false,
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
                    dave_decrypted: false,
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
                    dave_decrypted: true,
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
                        dave_decrypted: true,
                    };
                }
            }

            let known_users = dm.known_user_ids();
            let candidate_count = ordered_candidates.len();
            let frame_bytes = ordered_candidates
                .first()
                .map(|c| c.frame.len())
                .unwrap_or(0);
            // Check if the frame has DAVE magic marker (last 2 bytes = 0xFA 0xFA)
            let has_dave_marker = ordered_candidates.first().is_some_and(|c| {
                c.frame.len() >= 2
                    && c.frame[c.frame.len() - 2] == 0xFA
                    && c.frame[c.frame.len() - 1] == 0xFA
            });
            debug!(
                user_id = current_user_id,
                ssrc,
                codec = codec.as_str(),
                frame_bytes,
                has_dave_marker,
                candidate_count,
                known_users = ?known_users,
                pv = dm.protocol_version(),
                "UDP drop: DAVE video decrypt failed for all candidate users"
            );
            VideoFrameDecryptOutcome {
                frame: None,
                depacketizer_keyframe: false,
                needs_recovery: dm.track_decrypt_failure(),
                dave_decrypted: false,
            }
        }
        None => VideoFrameDecryptOutcome {
            frame: Some(pass_through_candidate.frame.clone()),
            depacketizer_keyframe: pass_through_candidate.depacketizer_keyframe,
            needs_recovery: false,
            dave_decrypted: false,
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
    role: TransportRole,
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
                        role,
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
                                role,
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
                                role,
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
    role: TransportRole,
    disconnect_sent: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 65_536];
    let mut video_depacketizers = VideoDepacketizers::default();
    let mut fallback_video_depacketizers = VideoDepacketizers::default();
    let mut observed_transport_decrypt_failures = HashSet::<u8>::new();
    let mut video_frame_emit_count: u64 = 0;
    let mut video_keyframe_count: u64 = 0;
    let mut dave_video_decrypt_ok: u64 = 0;
    let mut dave_video_decrypt_fail: u64 = 0;
    let mut dave_video_passthrough: u64 = 0;

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
                send_disconnect_once(
                    &event_tx,
                    &disconnect_sent,
                    role,
                    format!("UDP recv error: {e}"),
                )
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

        // RTCP packets (SR=200, RR=201, SDES=202, BYE=203, APP=204) share
        // the UDP socket via RTP/RTCP mux (RFC 5761).  Their payload-type
        // byte (masked to 7 bits) falls in 72-76.  We don't process inbound
        // RTCP — just skip before attempting RTP transport decryption, which
        // would fail because RTCP has a different AAD layout.
        if (72..=76).contains(&payload_type) {
            trace!(payload_type, ssrc, "UDP skip: inbound RTCP packet");
            continue;
        }

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
                if (payload_type == OPUS_PT
                    || VideoCodecKind::from_payload_type(payload_type).is_some())
                    && observed_transport_decrypt_failures.insert(payload_type)
                {
                    info!(
                        role = role.as_str(),
                        payload_type,
                        header_size,
                        error = %e,
                        "clankvox_transport_decrypt_failed"
                    );
                }
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

            let (opus_frame_opt, remapped_user_id, needs_recovery) = {
                let mut guard = dave.lock();
                match &mut *guard {
                    Some(dm) => {
                        dm.maybe_auto_execute_downgrade();

                        if !dm.is_ready() {
                            (Some(primary_payload.clone()), None, false)
                        } else {
                            let candidate_user_ids = ordered_audio_candidate_user_ids(
                                user_id,
                                dm.user_id(),
                                &dm.known_user_ids(),
                            );
                            let mut recovered: Option<(Vec<u8>, u64)> = None;

                            for candidate_uid in candidate_user_ids {
                                if let Some((decrypted, _used_fallback_payload)) =
                                    try_decrypt_audio_payload_for_user(
                                        dm,
                                        candidate_uid,
                                        &primary_payload,
                                        fallback_payload,
                                        ssrc,
                                    )
                                {
                                    recovered = Some((decrypted, candidate_uid));
                                    break;
                                }
                            }

                            if let Some((decrypted, candidate_uid)) = recovered {
                                if user_id != Some(candidate_uid) {
                                    ssrc_map.lock().insert(ssrc, candidate_uid);
                                    info!(
                                        ssrc,
                                        old_user_id = user_id,
                                        new_user_id = candidate_uid,
                                        "UDP: remapped audio ssrc after successful DAVE decrypt"
                                    );
                                }
                                (Some(decrypted), Some(candidate_uid), false)
                            } else if let Some(uid) = user_id {
                                debug!("UDP drop: DAVE audio decrypt failed for {uid}");
                                let recovery = dm.track_decrypt_failure();
                                (None, None, recovery)
                            } else {
                                debug!(
                                    ssrc,
                                    candidate_user_count = dm.known_user_ids().len(),
                                    "UDP drop: DAVE audio decrypt could not resolve user for unmapped ssrc"
                                );
                                let recovery = dm.track_decrypt_failure();
                                (None, None, recovery)
                            }
                        }
                    }
                    None => (Some(primary_payload.clone()), None, false),
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

            if let Some(remapped_user_id) = remapped_user_id.filter(|uid| Some(*uid) != user_id) {
                let _ = event_tx
                    .send(VoiceEvent::SsrcUpdate {
                        role,
                        ssrc,
                        user_id: remapped_user_id,
                    })
                    .await;
            }

            let _ = event_tx
                .send(VoiceEvent::OpusReceived {
                    role,
                    ssrc,
                    opus_frame,
                })
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

        // For video, try the full-payload (extension body included) depacketizer
        // first.  Discord's DAVE encryption operates on the full RTP payload
        // including extension body bytes — stripping them shifts the DAVE
        // trailer's byte offsets and causes AES-GCM tag mismatch on ~60% of
        // frames.  Audio uses the stripped variant as primary (99%+ success).
        let video_primary_payload = fallback_payload.as_deref().unwrap_or(&primary_payload);
        let primary_candidate = video_depacketizers
            .push(ssrc, codec, sequence, timestamp, marker, video_primary_payload)
            .map(|(frame, depacketizer_keyframe)| VideoFrameCandidate {
                frame,
                depacketizer_keyframe,
                used_fallback_payload: fallback_payload.is_some(),
            });
        let alternate_candidate = fallback_video_depacketizers
            .push(ssrc, codec, sequence, timestamp, marker, &primary_payload)
            .map(|(frame, depacketizer_keyframe)| VideoFrameCandidate {
                frame,
                depacketizer_keyframe,
                used_fallback_payload: false,
            });

        // Skip DAVE decrypt + frame emit entirely when neither depacketizer
        // produced a complete frame — most RTP packets are mid-frame FU-A
        // fragments and calling decrypt_video_frame_candidates on them just
        // burns a mutex lock for a guaranteed None result.
        if primary_candidate.is_none() && alternate_candidate.is_none() {
            continue;
        }

        let VideoFrameDecryptOutcome {
            frame: video_frame_opt,
            depacketizer_keyframe,
            needs_recovery,
            dave_decrypted,
        } = decrypt_video_frame_candidates(
            &dave,
            &video_ssrc_map,
            &mut binding,
            ssrc,
            codec,
            primary_candidate,
            alternate_candidate,
        );

        // Track DAVE video decrypt stats
        if video_frame_opt.is_some() {
            if dave_decrypted {
                dave_video_decrypt_ok += 1;
            } else {
                dave_video_passthrough += 1;
            }
        } else {
            dave_video_decrypt_fail += 1;
        }
        let dave_total = dave_video_decrypt_ok + dave_video_decrypt_fail + dave_video_passthrough;
        if dave_total > 0 && (dave_total <= 5 || dave_total % 100 == 0) {
            let success_pct = if dave_total > 0 {
                dave_video_decrypt_ok * 100 / dave_total
            } else {
                0
            };
            info!(
                dave_video_decrypt_ok,
                dave_video_decrypt_fail,
                dave_video_passthrough,
                dave_total,
                success_pct,
                role = role.as_str(),
                "clankvox_dave_video_decrypt_stats"
            );
            // Also dump the davey-internal per-user decrypt stats
            if let Some(dm) = dave.lock().as_ref() {
                dm.log_decrypt_stats();
            }
        }

        let Some(frame) = video_frame_opt else {
            if needs_recovery {
                let recovery = try_reinit_dave(&dave, "udp video decrypt failures");
                if let Some(recovery) = recovery {
                    send_recovery_action(&ws_cmd_tx, recovery, "udp video decrypt failures").await;
                }
            }
            continue;
        };

        // Prepend cached SPS+PPS AFTER DAVE decrypt so the DAVE trailer's
        // unencrypted ranges reference correct offsets in the original frame.
        let frame = if codec == VideoCodecKind::H264 {
            video_depacketizers.prepend_cached_h264_params(ssrc, frame)
        } else {
            frame
        };

        let keyframe = match codec {
            VideoCodecKind::H264 => {
                // Only IDR (NAL type 5) counts as a keyframe for rate-
                // limiting.  SPS+PPS are prepended to every frame after DAVE
                // decrypt so ffmpeg can always decode, but that prepend must
                // NOT cause every frame to bypass the fps gate.
                depacketizer_keyframe || h264_annexb_has_idr_slice(&frame)
            }
            VideoCodecKind::Vp8 => {
                depacketizer_keyframe || frame.first().is_some_and(|byte| byte & 0x01 == 0)
            }
        };

        video_frame_emit_count += 1;
        if keyframe {
            video_keyframe_count += 1;
        }
        // Log NAL types for the first 5 H264 frames and periodically after that
        if codec == VideoCodecKind::H264
            && (video_frame_emit_count <= 5 || video_frame_emit_count % 100 == 0)
        {
            let nal_types = collect_annexb_nal_types(&frame);
            info!(
                ssrc,
                frame_bytes = frame.len(),
                keyframe,
                depacketizer_keyframe,
                video_frame_emit_count,
                video_keyframe_count,
                nal_types = ?nal_types,
                "clankvox_h264_frame_nal_diagnostic"
            );
        }

        let _ = event_tx
            .send(VoiceEvent::VideoFrameReceived {
                role,
                user_id: binding.user_id,
                ssrc,
                codec: codec.as_str().to_string(),
                keyframe,
                frame,
                rtp_timestamp: timestamp,
                stream_type: binding.descriptor.stream_type.clone(),
                dave_decrypted,
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

    use super::{
        HelloPayload, SessionDescriptionPayload, VideoFrameCandidate, VoiceOpcode,
        decrypt_video_frame_candidates, ordered_audio_candidate_user_ids, parse_user_id,
        parse_voice_opcode, recv_ready, recv_session_description,
    };
    use crate::rtp::VideoCodecKind;
    use crate::video::VideoStreamDescriptor;
    use crate::video_state::RemoteVideoTrackBinding;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn ordered_audio_candidate_user_ids_tries_known_users_when_ssrc_map_is_missing() {
        let ordered = ordered_audio_candidate_user_ids(None, 999, &[999, 42, 43]);
        assert_eq!(ordered, vec![42, 43]);
    }

    #[test]
    fn ordered_audio_candidate_user_ids_prefers_current_mapping_before_other_known_users() {
        let ordered = ordered_audio_candidate_user_ids(Some(42), 999, &[999, 42, 43]);
        assert_eq!(ordered, vec![42, 43]);
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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tracing::{debug, error, info, trace, warn};

use crate::dave::DaveManager;

type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// Events emitted by the voice connection back to the main loop
// ---------------------------------------------------------------------------

pub enum VoiceEvent {
    Ready { ssrc: u32 },
    SsrcUpdate { ssrc: u32, user_id: u64 },
    ClientDisconnect { user_id: u64 },
    OpusReceived { ssrc: u32, opus_frame: Vec<u8> },
    DaveReady,
    Disconnected { reason: String },
    Error { message: String },
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

fn build_rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; RTP_HEADER_LEN] {
    let mut h = [0u8; RTP_HEADER_LEN];
    h[0] = 0x80; // V=2, P=0, X=0, CC=0
    h[1] = OPUS_PT;
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

fn parse_rtp_header(data: &[u8]) -> Option<(u16, u32, u32, usize)> {
    if data.len() < RTP_HEADER_LEN {
        return None;
    }
    let cc = (data[0] & 0x0F) as usize;
    let has_ext = (data[0] >> 4) & 0x01 != 0;
    let seq = u16::from_be_bytes([data[2], data[3]]);
    let ts = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

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
    Some((seq, ts, ssrc, header_size))
}

// ---------------------------------------------------------------------------
// Transport encryption (AES-256-GCM "rtpsize" mode)
// ---------------------------------------------------------------------------

struct TransportCrypto {
    cipher: TransportCipher,
    send_nonce: AtomicU32,
}

enum TransportCipher {
    Aes256GcmRtpSize(Aes256Gcm),
    XChaCha20Poly1305RtpSize(XChaCha20Poly1305),
}

impl TransportCrypto {
    fn new(secret_key: &[u8], mode: &str) -> Result<Self> {
        let cipher = match mode {
            "aead_aes256_gcm_rtpsize" => TransportCipher::Aes256GcmRtpSize(
                Aes256Gcm::new_from_slice(secret_key).context("Invalid AES-256-GCM secret key")?,
            ),
            "aead_xchacha20_poly1305_rtpsize" => TransportCipher::XChaCha20Poly1305RtpSize(
                XChaCha20Poly1305::new_from_slice(secret_key)
                    .context("Invalid XChaCha20-Poly1305 secret key")?,
            ),
            other => bail!("Unsupported transport mode: {}", other),
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
                    .map_err(|e| anyhow::anyhow!("AES-GCM encrypt: {}", e))?
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
                    .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt: {}", e))?
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
            bail!("Packet too small for computed AAD size {}", aad_size);
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
                    .map_err(|e| anyhow::anyhow!("AES-GCM decrypt: {}", e))
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
                    .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt: {}", e))
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
        bail!("IP discovery response too short: {} bytes", n);
    }

    // Response: [type(2) | length(2) | ssrc(4) | address(64) | port(2)]
    let ip_bytes = &resp[8..72];
    let ip = std::str::from_utf8(ip_bytes)
        .context("IP discovery: invalid UTF-8")?
        .trim_end_matches('\0')
        .to_string();
    let port = u16::from_be_bytes([resp[72], resp[73]]);

    info!("IP discovery: external {}:{}", ip, port);
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
}

impl VoiceConnection {
    /// Perform the full voice WS + UDP handshake, then spawn background tasks.
    pub async fn connect(
        endpoint: &str,
        guild_id: u64,
        user_id: u64,
        session_id: &str,
        token: &str,
        channel_id: u64,
        event_tx: mpsc::Sender<VoiceEvent>,
        dave: Arc<Mutex<Option<DaveManager>>>,
    ) -> Result<Self> {
        let ep = endpoint.trim_start_matches("wss://").trim_end_matches('/');
        let ws_url = format!("wss://{}/?v=8", ep);
        info!("Connecting voice WS: {}", ws_url);

        let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .context("Voice WS connect failed")?;
        let (mut ws_write, mut ws_read) = ws.split();

        // ---- OP8 Hello ----
        let heartbeat_interval = recv_hello(&mut ws_read).await?;

        // ---- OP0 Identify (advertise DAVE v1) ----
        let identify = json!({
            "op": 0,
            "d": {
                "server_id": guild_id.to_string(),
                "user_id": user_id.to_string(),
                "session_id": session_id,
                "token": token,
                "max_dave_protocol_version": 1
            }
        });
        ws_write
            .send(Message::Text(identify.to_string()))
            .await
            .context("Send Identify")?;

        // Handshake overflow buffer: messages that arrive during the handshake
        // but aren't the target opcode (e.g. DAVE OP21/OP25) get buffered here
        // and replayed into the ws_read_loop once background tasks are spawned.
        let mut handshake_overflow: HandshakeOverflow = Vec::new();

        // ---- OP2 Ready ----
        let (ssrc, voice_ip, voice_port, modes) =
            recv_ready(&mut ws_read, &mut handshake_overflow).await?;
        info!(
            "Voice Ready: ssrc={} udp={}:{} modes={:?}",
            ssrc, voice_ip, voice_port, modes
        );

        // ---- UDP socket + IP discovery ----
        let udp = UdpSocket::bind("0.0.0.0:0").await.context("UDP bind")?;
        let voice_addr: SocketAddr = format!("{}:{}", voice_ip, voice_port)
            .parse()
            .context("Parse voice UDP addr")?;
        udp.connect(voice_addr).await.context("UDP connect")?;

        let (external_ip, external_port) = ip_discovery(&udp, ssrc).await?;

        // ---- Select encryption mode ----
        let mode = if modes.iter().any(|m| m == "aead_aes256_gcm_rtpsize") {
            "aead_aes256_gcm_rtpsize"
        } else if modes.iter().any(|m| m == "aead_xchacha20_poly1305_rtpsize") {
            warn!("AES256-GCM RTP-size unavailable; using XChaCha20-Poly1305 RTP-size fallback");
            "aead_xchacha20_poly1305_rtpsize"
        } else {
            bail!(
                "No supported encryption mode (need aead_aes256_gcm_rtpsize or aead_xchacha20_poly1305_rtpsize), got: {:?}",
                modes
            );
        };

        // ---- OP1 Select Protocol ----
        let select = json!({
            "op": 1,
            "d": {
                "protocol": "udp",
                "data": {
                    "address": external_ip,
                    "port": external_port,
                    "mode": mode
                },
                "codecs": [{
                    "name": "opus",
                    "type": "audio",
                    "priority": 1000,
                    "payload_type": 120
                }]
            }
        });
        ws_write
            .send(Message::Text(select.to_string()))
            .await
            .context("Send Select Protocol")?;

        // ---- OP4 Session Description ----
        let (secret_key, dave_pv) =
            recv_session_description(&mut ws_read, &mut handshake_overflow).await?;
        let crypto = Arc::new(TransportCrypto::new(&secret_key, mode)?);
        info!("Voice session established, transport crypto ready");

        if dave_pv > 0 {
            match DaveManager::new(dave_pv, user_id, channel_id) {
                Ok((dm, pkg)) => {
                    *dave.lock() = Some(dm);
                    info!("DaveManager initialized with protocol version {}", dave_pv);

                    // Transmit DAVE KeyPackage (`OP26`) to Discord Voice Server
                    // so the Voice Server will initiate `OP25 ExternalSender`
                    let mut op26_payload = vec![26u8];
                    op26_payload.extend_from_slice(&pkg);
                    ws_write
                        .send(Message::Binary(op26_payload))
                        .await
                        .context("Send DAVE KeyPackage OP26")?;
                    info!("Sent DAVE OP26 KeyPackage to Discord ({} bytes)", pkg.len());
                }
                Err(e) => {
                    error!("Failed to initialize DaveManager: {}", e);
                }
            }
        }

        // ---- Spawn background tasks ----
        let shutdown = Arc::new(AtomicBool::new(false));
        let (ws_cmd_tx, ws_cmd_rx) = mpsc::channel::<WsCommand>(128);
        let udp = Arc::new(udp);
        let ssrc_map: Arc<Mutex<HashMap<u32, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let ws_sequence = Arc::new(AtomicI32::new(-1));

        // WS read loop (handles Speaking updates, DAVE opcodes, etc.)
        {
            let shutdown = shutdown.clone();
            let event_tx = event_tx.clone();
            let dave = dave.clone();
            let ws_cmd_tx = ws_cmd_tx.clone();
            let ssrc_map = ssrc_map.clone();
            let ws_sequence = ws_sequence.clone();
            if !handshake_overflow.is_empty() {
                info!(
                    "Replaying {} buffered handshake messages into read loop",
                    handshake_overflow.len()
                );
            }
            tokio::spawn(async move {
                // Replay messages that arrived during handshake but weren't the target
                // opcode (critical for DAVE OP21/OP25 Discord may interleave with Ready/Session Description).
                for (i, msg) in handshake_overflow.into_iter().enumerate() {
                    match msg {
                        Message::Text(ref text) => {
                            if let Ok(v) = serde_json::from_str::<Value>(text) {
                                let op = v["op"].as_u64().unwrap_or(u64::MAX);
                                info!("Replay [{}]: Text OP={}", i, op);
                                let d = &v["d"];
                                handle_text_opcode(
                                    op,
                                    d,
                                    &event_tx,
                                    &ws_cmd_tx,
                                    &dave,
                                    &ssrc_map,
                                    user_id,
                                    channel_id,
                                    &ws_sequence,
                                )
                                .await;
                            } else {
                                info!("Replay [{}]: Invalid Text", i);
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
                            info!("Replay [{}]: Empty Binary", i);
                        }
                        _ => {
                            info!("Replay [{}]: Other message type", i);
                        }
                    }
                }
                ws_read_loop(
                    ws_read,
                    event_tx,
                    ws_cmd_tx,
                    dave,
                    ssrc_map,
                    shutdown,
                    user_id,
                    channel_id,
                    ws_sequence,
                )
                .await;
            });
        }

        // WS write loop (heartbeat + outgoing commands)
        {
            let shutdown = shutdown.clone();
            let ws_sequence = ws_sequence.clone();
            tokio::spawn(async move {
                ws_write_loop(
                    ws_write,
                    ws_cmd_rx,
                    shutdown,
                    heartbeat_interval,
                    ws_sequence,
                )
                .await;
            });
        }

        // UDP receive loop
        {
            let shutdown = shutdown.clone();
            let event_tx = event_tx.clone();
            let crypto = crypto.clone();
            let dave = dave.clone();
            let udp = udp.clone();
            let ssrc_map = ssrc_map.clone();
            let ws_cmd_tx = ws_cmd_tx.clone();
            tokio::spawn(async move {
                udp_recv_loop(udp, crypto, dave, ssrc_map, event_tx, ws_cmd_tx, shutdown).await;
            });
        }

        // Set speaking state so Discord knows we may transmit
        let _ = ws_cmd_tx
            .send(WsCommand::SendJson(json!({
                "op": 5,
                "d": { "speaking": 1, "delay": 0, "ssrc": ssrc }
            })))
            .await;

        // Signal ready to the main loop
        let _ = event_tx.send(VoiceEvent::Ready { ssrc }).await;

        Ok(VoiceConnection {
            ssrc,
            shutdown,
            udp_socket: udp,
            crypto,
            rtp_sequence: AtomicU32::new(0),
            timestamp: AtomicU32::new(0),
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

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Handshake helpers (synchronous WS reads during connect)
// ---------------------------------------------------------------------------

/// Messages received during the handshake that weren't the target opcode.
/// These are buffered and replayed into the ws_read_loop so DAVE opcodes
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
            let v: Value = serde_json::from_str(&text)?;
            if v["op"].as_u64() == Some(8) {
                return Ok(v["d"]["heartbeat_interval"].as_f64().unwrap_or(13750.0));
            }
        }
    }
}

async fn recv_ready(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    overflow: &mut HandshakeOverflow,
) -> Result<(u32, String, u16, Vec<String>)> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = time::timeout_at(deadline, ws.next())
            .await
            .context("Timeout waiting for OP2 Ready")?
            .context("WS stream ended")?
            .context("WS error")?;
        match &msg {
            Message::Text(text) => {
                let v: Value = serde_json::from_str(text)?;
                if v["op"].as_u64() == Some(2) {
                    let d = &v["d"];
                    let ssrc = d["ssrc"].as_u64().context("missing ssrc")? as u32;
                    let ip = d["ip"].as_str().context("missing ip")?.to_string();
                    let port = d["port"].as_u64().context("missing port")? as u16;
                    let modes: Vec<String> = d["modes"]
                        .as_array()
                        .context("missing modes")?
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    return Ok((ssrc, ip, port, modes));
                }
                debug!("Handshake (waiting OP2): buffered text op={}", v["op"]);
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
) -> Result<(Vec<u8>, u16)> {
    let deadline = time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = time::timeout_at(deadline, ws.next())
            .await
            .context("Timeout waiting for OP4 Session Description")?
            .context("WS stream ended")?
            .context("WS error")?;
        match &msg {
            Message::Text(text) => {
                let v: Value = serde_json::from_str(text)?;
                if v["op"].as_u64() == Some(4) {
                    let key: Vec<u8> = v["d"]["secret_key"]
                        .as_array()
                        .context("missing secret_key")?
                        .iter()
                        .map(|b| b.as_u64().unwrap_or(0) as u8)
                        .collect();
                    let dave_pv = v["d"]["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
                    return Ok((key, dave_pv));
                }
                debug!("Handshake (waiting OP4): buffered text op={}", v["op"]);
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
    shutdown: Arc<AtomicBool>,
    bot_user_id: u64,
    channel_id: u64,
    ws_sequence: Arc<AtomicI32>,
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
                warn!("{}", reason);
                let _ = event_tx.send(VoiceEvent::Disconnected { reason }).await;
                break;
            }
            Err(e) => {
                let _ = event_tx
                    .send(VoiceEvent::Error {
                        message: format!("WS read error: {}", e),
                    })
                    .await;
                break;
            }
            _ => {}
        }
    }
    info!("Voice WS read loop exited");
}

#[allow(clippy::too_many_arguments)]
async fn handle_text_opcode(
    op: u64,
    d: &Value,
    event_tx: &mpsc::Sender<VoiceEvent>,
    ws_cmd_tx: &mpsc::Sender<WsCommand>,
    dave: &Arc<Mutex<Option<DaveManager>>>,
    ssrc_map: &Arc<Mutex<HashMap<u32, u64>>>,
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
            let ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
            let uid = d["user_id"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            ssrc_map.lock().insert(ssrc, uid);

            let _ = event_tx
                .send(VoiceEvent::SsrcUpdate { ssrc, user_id: uid })
                .await;
        }
        // Client disconnect
        12 => {
            if let Some(uid) = d["user_id"].as_str().and_then(|s| s.parse::<u64>().ok()) {
                ssrc_map.lock().retain(|_, v| *v != uid);
                let _ = event_tx
                    .send(VoiceEvent::ClientDisconnect { user_id: uid })
                    .await;
            }
        }
        // OP21: DavePrepareTransition — a transition is upcoming, respond with OP23
        21 => {
            let transition_id = d["transition_id"].as_u64().unwrap_or(0) as u16;
            let pv = d["protocol_version"].as_u64().unwrap_or(0) as u16;
            info!(
                "DAVE OP21: prepare transition id={} pv={}",
                transition_id, pv
            );
            let send_ready = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    dm.prepare_transition(transition_id, pv)
                } else {
                    false
                }
            };
            if send_ready {
                // OP23 = DaveTransitionReady (client → server)
                let _ = ws_cmd_tx
                    .send(WsCommand::SendJson(json!({
                        "op": 23,
                        "d": { "transition_id": transition_id }
                    })))
                    .await;
                info!(
                    "DAVE: sent OP23 transition ready for prepare transition {}",
                    transition_id
                );
            }
        }
        // OP22: DaveExecuteTransition — finalize the pending transition
        22 => {
            let transition_id = d["transition_id"].as_u64().unwrap_or(0) as u16;
            info!(
                "DAVE OP22: execute transition received, transition_id={}",
                transition_id
            );
            let transitioned = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    dm.execute_transition(transition_id)
                } else {
                    false
                }
            };
            if transitioned {
                let ready = {
                    let guard = dave.lock();
                    guard.as_ref().map_or(false, |dm| dm.is_ready())
                };
                if ready {
                    let _ = event_tx.send(VoiceEvent::DaveReady).await;
                }
            }
        }
        // OP24: DavePrepareEpoch — a new DAVE epoch is upcoming
        24 => {
            let pv = d["protocol_version"].as_u64().unwrap_or(0) as u16;
            let epoch = d["epoch"].as_u64().unwrap_or(0);
            info!("DAVE OP24: prepare epoch pv={} epoch={}", pv, epoch);

            if pv > 0 {
                let pkg_to_send = {
                    let mut guard = dave.lock();
                    if guard.is_none() {
                        match DaveManager::new(pv, bot_user_id, channel_id) {
                            Ok((dm, pkg)) => {
                                *guard = Some(dm);
                                Some(pkg)
                            }
                            Err(e) => {
                                error!("Failed to create DaveManager: {}", e);
                                None
                            }
                        }
                    } else {
                        // DaveManager already exists — reinit for new epoch
                        // (matches discord.js prepareEpoch which calls reinit())
                        if let Some(ref mut dm) = *guard {
                            match dm.reinit() {
                                Ok(recovery) => Some(recovery.key_package),
                                Err(e) => {
                                    error!("Failed to reinit DaveManager for new epoch: {}", e);
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    }
                };

                // Now that the MutexGuard is fully dropped, send the payload if we have one
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
            debug!("Unknown voice WS opcode: {}", op);
        }
    }
}

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
                        error!("DAVE set_external_sender: {}", e);
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
            if payload.len() < 1 {
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
                            error!("DAVE process_proposals: {}", e);
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
            let (ready, success, recovery_action) = {
                let mut guard = dave.lock();
                if let Some(ref mut dm) = *guard {
                    match dm.process_commit(commit_payload) {
                        Ok(()) => {
                            dm.store_pending_transition(transition_id);
                            (dm.is_ready(), true, None)
                        }
                        Err(e) => {
                            error!("DAVE process_commit: {}", e);
                            let recovery = dm.reinit().ok();
                            (false, false, recovery)
                        }
                    }
                } else {
                    (false, false, None)
                }
            };
            // Lock is dropped — safe to await

            if let Some(recovery) = recovery_action {
                let mut op31 = vec![31u8];
                op31.extend_from_slice(&recovery.transition_id.to_be_bytes());
                let _ = ws_cmd_tx.send(WsCommand::SendBinary(op31)).await;
                let mut op26 = vec![26u8];
                op26.extend_from_slice(&recovery.key_package);
                let _ = ws_cmd_tx.send(WsCommand::SendBinary(op26)).await;
                warn!("DAVE: recovery from failed commit, sent OP31 + OP26");
            }

            // Match discord.js behavior: for non-zero transitions, confirm readiness with OP23.
            if success && transition_id != 0 {
                // OP23 = DaveTransitionReady (client → server)
                let _ = ws_cmd_tx
                    .send(WsCommand::SendJson(json!({
                        "op": 23,
                        "d": { "transition_id": transition_id }
                    })))
                    .await;
                info!(
                    "DAVE: sent OP23 transition ready for commit transition {}",
                    transition_id
                );
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
                            let err_msg = format!("{:?}", e);
                            if err_msg.contains("AlreadyInGroup") || err_msg.contains("already") {
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
                                error!("DAVE process_welcome failed: {}", e);
                                let recovery = dm.reinit().ok();
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
                let mut op31 = vec![31u8];
                op31.extend_from_slice(&recovery.transition_id.to_be_bytes());
                let _ = ws_cmd_tx.send(WsCommand::SendBinary(op31)).await;
                let mut op26 = vec![26u8];
                op26.extend_from_slice(&recovery.key_package);
                let _ = ws_cmd_tx.send(WsCommand::SendBinary(op26)).await;
                warn!("DAVE: recovery from failed welcome, sent OP31 + OP26");
            }

            // Match discord.js behavior: for non-zero transitions, confirm readiness with OP23.
            if success && transition_id != 0 {
                // OP23 = DaveTransitionReady (client → server)
                let _ = ws_cmd_tx
                    .send(WsCommand::SendJson(json!({
                        "op": 23,
                        "d": { "transition_id": transition_id }
                    })))
                    .await;
                info!(
                    "DAVE: sent OP23 transition ready for welcome transition {}",
                    transition_id
                );
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

async fn ws_write_loop(
    mut ws_write: futures_util::stream::SplitSink<WsStream, Message>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    shutdown: Arc<AtomicBool>,
    heartbeat_interval_ms: f64,
    ws_sequence: Arc<AtomicI32>,
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
                if ws_write.send(Message::Text(hb.to_string())).await.is_err() {
                    break;
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCommand::SendJson(v)) => {
                        if ws_write.send(Message::Text(v.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(WsCommand::SendBinary(data)) => {
                        if ws_write.send(Message::Binary(data)).await.is_err() {
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

async fn udp_recv_loop(
    socket: Arc<UdpSocket>,
    crypto: Arc<TransportCrypto>,
    dave: Arc<Mutex<Option<DaveManager>>>,
    ssrc_map: Arc<Mutex<HashMap<u32, u64>>>,
    event_tx: mpsc::Sender<VoiceEvent>,
    ws_cmd_tx: mpsc::Sender<WsCommand>,
    shutdown: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 2048];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let n = match socket.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                debug!("UDP recv error: {}", e);
                continue;
            }
        };
        let packet = &buf[..n];

        let (_seq, _ts, ssrc, header_size) = match parse_rtp_header(packet) {
            Some(h) => h,
            None => {
                debug!("UDP drop: failed to parse RTP header");
                continue;
            }
        };

        // Only handle Opus RTP packets (PT=120). Drop RTCP/other media payloads.
        let payload_type = packet[1] & 0x7F;
        if payload_type != OPUS_PT {
            trace!("UDP drop: non-Opus RTP payload type {}", payload_type);
            continue;
        }

        // Transport decrypt
        let decrypted = match crypto.decrypt(packet, header_size) {
            Ok(p) => p,
            Err(e) => {
                debug!("UDP drop: Transport crypto decrypt failed: {}", e);
                continue;
            }
        };

        let cc = (packet[0] & 0x0F) as usize;
        let aad_size = RTP_HEADER_LEN + cc * 4;
        let has_ext = (packet[0] >> 4) & 0x01 != 0;

        let original_payload = decrypted;
        let mut stripped_payload: Option<Vec<u8>> = None;

        // Strip RTP Header Extension if present and matches the one-byte profile (0xBEDE).
        // The extension body lives at the beginning of the `decrypted` payload.
        if has_ext && packet.len() >= aad_size + 4 {
            let profile = &packet[aad_size..aad_size + 2];
            let ext_len = u16::from_be_bytes([packet[aad_size + 2], packet[aad_size + 3]]) as usize;
            let extension_bytes = ext_len * 4;

            if profile == [0xbe, 0xde] {
                if original_payload.len() > extension_bytes {
                    stripped_payload = Some(original_payload[extension_bytes..].to_vec());
                } else {
                    debug!("UDP drop: RTP extension body exceeds decrypted payload");
                    continue;
                }
            } else {
                // Not 0xBEDE, but it is an extension. discord.js does NOT strip this.
                // Let's print what profile it is to see if we're missing something.
                debug!("UDP: Unknown RTP extension profile: {:x?}", profile);
                // The RTP spec says we should strip it anyway, but discord.js doesn't.
                // Let's strip it to be safe, because it's part of the extension body.
                if original_payload.len() > extension_bytes {
                    stripped_payload = Some(original_payload[extension_bytes..].to_vec());
                }
            }
        }

        let primary_payload = stripped_payload.as_ref().unwrap_or(&original_payload);
        let fallback_payload = stripped_payload.as_ref().map(|_| &original_payload);

        // DAVE decrypt (if session active) with failure tracking + recovery
        // Mirrors discord.js: canDecrypt = session.ready && (protocolVersion !== 0 || session.canPassthrough(userId))
        let user_id = ssrc_map.lock().get(&ssrc).copied();

        let (opus_frame_opt, needs_recovery) = {
            let mut guard = dave.lock();
            match (&mut *guard, user_id) {
                (Some(dm), Some(uid)) => {
                    // Safety net: auto-execute pending pv=0 downgrades if OP22 hasn't
                    // arrived in time (e.g. due to network delays).
                    dm.maybe_auto_execute_downgrade();

                    let can_decrypt =
                        dm.is_ready() && (dm.protocol_version() != 0 || dm.can_passthrough(uid));
                    if can_decrypt {
                        match dm.decrypt(uid, primary_payload) {
                            Ok(decrypted) => (Some(decrypted), false),
                            Err(e) => {
                                let mut recovered: Option<Vec<u8>> = None;

                                // Fallback 1: try alternate payload variant (with/without RTP ext strip).
                                if let Some(alt_payload) = fallback_payload {
                                    if let Ok(decrypted) = dm.decrypt(uid, alt_payload) {
                                        debug!(
                                            "UDP: DAVE decrypt recovered for {} using alternate RTP ext handling",
                                            uid
                                        );
                                        recovered = Some(decrypted);
                                    }
                                }

                                // Fallback 2: if SSRC→user mapping is stale, try other known MLS members.
                                if recovered.is_none() {
                                    for candidate_uid in dm.known_user_ids() {
                                        if candidate_uid == uid || candidate_uid == dm.user_id() {
                                            continue;
                                        }
                                        if let Ok(decrypted) =
                                            dm.decrypt(candidate_uid, primary_payload)
                                        {
                                            ssrc_map.lock().insert(ssrc, candidate_uid);
                                            debug!(
                                                "UDP: remapped ssrc {} from user {} to {} after successful DAVE decrypt",
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
                                                    "UDP: remapped ssrc {} from user {} to {} with alternate RTP ext handling",
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
                                    debug!("UDP drop: DAVE decrypt failed for {}: {}", uid, e);
                                    let recovery = dm.track_decrypt_failure();
                                    (None, recovery)
                                }
                            }
                        }
                    } else {
                        // Bypass DAVE and pass payload to Opus directly
                        (Some(primary_payload.to_vec()), false)
                    }
                }
                _ => (Some(primary_payload.to_vec()), false),
            }
        };

        let opus_frame = match opus_frame_opt {
            Some(frame) => frame,
            None => {
                // DAVE decrypt failed — trigger recovery if threshold exceeded
                if needs_recovery {
                    let recovery = {
                        let mut guard = dave.lock();
                        guard.as_mut().and_then(|dm| dm.reinit().ok())
                    };
                    if let Some(recovery) = recovery {
                        let mut op31 = vec![31u8];
                        op31.extend_from_slice(&recovery.transition_id.to_be_bytes());
                        let _ = ws_cmd_tx.send(WsCommand::SendBinary(op31)).await;
                        let mut op26 = vec![26u8];
                        op26.extend_from_slice(&recovery.key_package);
                        let _ = ws_cmd_tx.send(WsCommand::SendBinary(op26)).await;
                        warn!("DAVE: recovery initiated from UDP recv ({} failures), sent OP31 + OP26",
                            crate::dave::FAILURE_TOLERANCE);
                    }
                }
                continue;
            }
        };

        let _ = event_tx
            .send(VoiceEvent::OpusReceived { ssrc, opus_frame })
            .await;
    }
    info!("UDP recv loop exited");
}

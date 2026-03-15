use tracing::debug;

// ---------------------------------------------------------------------------
// RTP header (minimal, Discord voice)
// ---------------------------------------------------------------------------

pub(crate) const RTP_HEADER_LEN: usize = 12;
pub(crate) const OPUS_PT: u8 = 0x78; // payload type 120
pub(crate) const H264_PT: u8 = 103;
pub(crate) const H264_RTX_PT: u8 = 104;
pub(crate) const VP8_PT: u8 = 105;
pub(crate) const VP8_RTX_PT: u8 = 106;
pub(crate) const VIDEO_RTP_EXTENSION_HEADER: [u8; 4] = [0xbe, 0xde, 0x00, 0x01];
pub(crate) const VIDEO_RTP_EXTENSION_PAYLOAD: [u8; 4] = [0x51, 0x00, 0x00, 0x00];
pub(crate) const MAX_VIDEO_RTP_CHUNK_BYTES: usize = 1_100;

// ---------------------------------------------------------------------------
// Video codec kind
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodecKind {
    H264,
    Vp8,
}

impl VideoCodecKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Vp8 => "VP8",
        }
    }

    pub(crate) fn payload_type(self) -> u8 {
        match self {
            Self::H264 => H264_PT,
            Self::Vp8 => VP8_PT,
        }
    }

    pub(crate) fn rtx_payload_type(self) -> u8 {
        match self {
            Self::H264 => H264_RTX_PT,
            Self::Vp8 => VP8_RTX_PT,
        }
    }

    pub(crate) fn from_payload_type(payload_type: u8) -> Option<Self> {
        match payload_type {
            H264_PT => Some(Self::H264),
            VP8_PT => Some(Self::Vp8),
            _ => None,
        }
    }

    pub(crate) fn is_rtx_payload_type(payload_type: u8) -> bool {
        matches!(payload_type, H264_RTX_PT | VP8_RTX_PT)
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "H264" | "H.264" => Some(Self::H264),
            "VP8" => Some(Self::Vp8),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// RTP header building / parsing
// ---------------------------------------------------------------------------

pub(crate) fn build_rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; RTP_HEADER_LEN] {
    let mut h = [0u8; RTP_HEADER_LEN];
    h[0] = 0x80; // V=2, P=0, X=0, CC=0
    h[1] = OPUS_PT;
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

pub(crate) fn build_video_rtp_header(
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    marker: bool,
) -> [u8; RTP_HEADER_LEN] {
    let mut h = [0u8; RTP_HEADER_LEN];
    h[0] = 0x90; // V=2, X=1
    h[1] = payload_type | if marker { 0x80 } else { 0x00 };
    h[2..4].copy_from_slice(&sequence.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    h[8..12].copy_from_slice(&ssrc.to_be_bytes());
    h
}

pub(crate) fn parse_rtp_header(data: &[u8]) -> Option<(u16, u32, u32, usize, bool)> {
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

pub(crate) fn strip_rtp_extension_payload(
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
        debug!(profile = ?profile, ext_len, extension_bytes, "UDP: non-BEDE RTP extension profile stripped");
    }
    Some((stripped, Some(decrypted)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}

use anyhow::Result;

use crate::transport_crypto::{TRANSPORT_NONCE_LEN, TRANSPORT_TAG_LEN, TransportCrypto};

// ---------------------------------------------------------------------------
// RTCP header (RFC 3550)
// ---------------------------------------------------------------------------

pub(crate) const RTCP_HEADER_LEN: usize = 4;

/// Build a 4-byte RTCP fixed header.
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |V=2|P|  FMT/RC |      PT       |           length              |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// `packet_len_bytes` is the total on-wire packet length in bytes (must be a
/// multiple of 4). The RTCP `length` field is set to `(packet_len_bytes / 4) - 1`.
pub(crate) fn build_rtcp_header(
    fmt_or_count: u8,
    packet_type: u8,
    packet_len_bytes: usize,
) -> [u8; RTCP_HEADER_LEN] {
    let mut header = [0u8; RTCP_HEADER_LEN];
    let word_count = packet_len_bytes / 4;
    let length_field = word_count
        .checked_sub(1)
        .expect("rtcp packet length must include at least one 32-bit word");
    header[0] = 0x80 | (fmt_or_count & 0x1f); // V=2, P=0
    header[1] = packet_type;
    header[2..4].copy_from_slice(&(length_field as u16).to_be_bytes());
    header
}

/// Build a transport-protected RTCP packet.
///
/// This is the free-function equivalent of the former
/// `TransportCrypto::build_protected_rtcp_packet` method. It constructs the
/// RTCP header, encrypts the body under the transport AEAD using the header as
/// AAD, and returns the complete on-wire packet.
pub(crate) fn build_protected_rtcp_packet(
    crypto: &TransportCrypto,
    fmt_or_count: u8,
    packet_type: u8,
    body: &[u8],
) -> Result<Vec<u8>> {
    let packet_len = RTCP_HEADER_LEN + body.len() + TRANSPORT_TAG_LEN + TRANSPORT_NONCE_LEN;
    let header = build_rtcp_header(fmt_or_count, packet_type, packet_len);
    let encrypted = crypto.encrypt(&header, body)?;
    let mut packet = Vec::with_capacity(header.len() + encrypted.len());
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&encrypted);
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_rtcp_feedback_packets_round_trip() {
        let crypto = TransportCrypto::new(&[5u8; 32], "aead_aes256_gcm_rtpsize")
            .expect("crypto should initialize");
        let sender_ssrc = 0x1122_3344u32;
        let media_ssrc = 0x5566_7788u32;

        let rr_packet = build_protected_rtcp_packet(&crypto, 0, 201, &sender_ssrc.to_be_bytes())
            .expect("rr packet");

        let mut pli_body = [0u8; 8];
        pli_body[0..4].copy_from_slice(&sender_ssrc.to_be_bytes());
        pli_body[4..8].copy_from_slice(&media_ssrc.to_be_bytes());
        let pli_packet =
            build_protected_rtcp_packet(&crypto, 1, 206, &pli_body).expect("pli packet");

        let mut fir_body = [0u8; 16];
        fir_body[0..4].copy_from_slice(&sender_ssrc.to_be_bytes());
        fir_body[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
        let fir_packet =
            build_protected_rtcp_packet(&crypto, 4, 206, &fir_body).expect("fir packet");

        // RR: 4 (header) + 4 (body) + 16 (tag) + 4 (nonce) = 28
        assert_eq!(rr_packet.len(), 28);
        assert_eq!(rr_packet[0], 0x80);
        assert_eq!(rr_packet[1], 201);
        assert_eq!(u16::from_be_bytes([rr_packet[2], rr_packet[3]]), 6);
        let rr_body = crypto
            .decrypt_with_aad(&rr_packet, RTCP_HEADER_LEN)
            .expect("rr decrypt");
        assert_eq!(rr_body, sender_ssrc.to_be_bytes());

        // PLI: 4 + 8 + 16 + 4 = 32
        assert_eq!(pli_packet.len(), 32);
        assert_eq!(pli_packet[0], 0x81);
        assert_eq!(pli_packet[1], 206);
        assert_eq!(u16::from_be_bytes([pli_packet[2], pli_packet[3]]), 7);
        let pli_body = crypto
            .decrypt_with_aad(&pli_packet, RTCP_HEADER_LEN)
            .expect("pli decrypt");
        assert_eq!(&pli_body[0..4], &sender_ssrc.to_be_bytes());
        assert_eq!(&pli_body[4..8], &media_ssrc.to_be_bytes());

        // FIR: 4 + 16 + 16 + 4 = 40
        assert_eq!(fir_packet.len(), 40);
        assert_eq!(fir_packet[0], 0x84);
        assert_eq!(fir_packet[1], 206);
        assert_eq!(u16::from_be_bytes([fir_packet[2], fir_packet[3]]), 9);
        let fir_body = crypto
            .decrypt_with_aad(&fir_packet, RTCP_HEADER_LEN)
            .expect("fir decrypt");
        assert_eq!(&fir_body[0..4], &sender_ssrc.to_be_bytes());
        assert_eq!(&fir_body[4..8], &0u32.to_be_bytes());
        assert_eq!(&fir_body[8..12], &media_ssrc.to_be_bytes());
        assert_eq!(fir_body[12], 0);
        assert_eq!(&fir_body[13..16], &[0, 0, 0]);
    }
}

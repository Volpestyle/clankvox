use std::sync::atomic::{AtomicU32, Ordering};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, bail};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::rtp::RTP_HEADER_LEN;

pub(crate) const TRANSPORT_TAG_LEN: usize = 16;
pub(crate) const TRANSPORT_NONCE_LEN: usize = 4;

pub(crate) enum TransportCipher {
    Aes256GcmRtpSize(Box<Aes256Gcm>),
    XChaCha20Poly1305RtpSize(XChaCha20Poly1305),
}

pub(crate) struct TransportCrypto {
    pub(crate) cipher: TransportCipher,
    pub(crate) send_nonce: AtomicU32,
}

impl TransportCrypto {
    pub(crate) fn new(secret_key: &[u8], mode: &str) -> Result<Self> {
        use anyhow::Context;
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

    /// Encrypt a transport payload for sending under Discord's `rtpsize` modes.
    /// Returns `[ciphertext + 16-byte tag + 4-byte BE nonce]`.
    pub(crate) fn encrypt(&self, aad: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
        let nonce_val = self.send_nonce.fetch_add(1, Ordering::SeqCst);
        let ct = match &self.cipher {
            TransportCipher::Aes256GcmRtpSize(cipher) => {
                let mut nonce_12 = [0u8; 12];
                nonce_12[0..4].copy_from_slice(&nonce_val.to_be_bytes());
                cipher
                    .encrypt(Nonce::from_slice(&nonce_12), Payload { msg: payload, aad })
                    .map_err(|e| anyhow::anyhow!("AES-GCM encrypt: {e}"))?
            }
            TransportCipher::XChaCha20Poly1305RtpSize(cipher) => {
                let mut nonce_24 = [0u8; 24];
                nonce_24[0..4].copy_from_slice(&nonce_val.to_be_bytes());
                cipher
                    .encrypt(XNonce::from_slice(&nonce_24), Payload { msg: payload, aad })
                    .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 encrypt: {e}"))?
            }
        };

        let mut out = ct; // ciphertext + tag
        out.extend_from_slice(&nonce_val.to_be_bytes());
        Ok(out)
    }

    /// Decrypt a received RTP media packet.
    ///
    /// Under Discord's `rtpsize` AEAD modes the AAD covers the RTP fixed
    /// header + CSRC list + the 4-byte extension header prefix (profile +
    /// length) but **not** the extension body.  `header_size` from
    /// `parse_rtp_header` includes the full extension (header + body), so we
    /// must recompute the AAD boundary from the raw packet bytes.
    pub(crate) fn decrypt(&self, packet: &[u8], _header_size: usize) -> Result<Vec<u8>> {
        let cc = (packet[0] & 0x0F) as usize;
        let mut aad_size = RTP_HEADER_LEN + cc * 4;
        if (packet[0] >> 4) & 0x01 != 0 {
            aad_size += 4;
        }
        self.decrypt_with_aad(packet, aad_size)
    }

    pub(crate) fn decrypt_with_aad(&self, packet: &[u8], aad_size: usize) -> Result<Vec<u8>> {
        // Layout: [aad | ciphertext + 16-byte tag | 4-byte BE nonce]
        if packet.len() < aad_size + TRANSPORT_TAG_LEN + TRANSPORT_NONCE_LEN {
            bail!("Packet too small for transport decryption");
        }
        if packet.len() <= aad_size + 4 {
            bail!("Packet too small for computed AAD size {aad_size}");
        }

        let aad = &packet[..aad_size];
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
                            aad,
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
                            aad,
                        },
                    )
                    .map_err(|e| anyhow::anyhow!("XChaCha20-Poly1305 decrypt: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{OPUS_PT, build_rtp_header, parse_rtp_header};

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

    /// Regression: the rtpsize AEAD AAD covers the RTP fixed header + CSRC
    /// list + the 4-byte extension header prefix, but NOT the extension body.
    /// `parse_rtp_header` returns a `header_size` that includes the full
    /// extension (header + body).  If `decrypt` naively used `header_size` as
    /// the AAD boundary, every packet with an RTP extension would fail
    /// decryption — making the bot completely deaf.
    ///
    /// Discord's on-wire layout for rtpsize modes:
    ///   [rtp_fixed_header | ext_prefix | encrypt(ext_body + opus) | tag | nonce]
    /// AAD = rtp_fixed_header + ext_prefix (16 bytes for CC=0)
    /// Ciphertext = ext_body + opus payload
    #[test]
    fn rtp_decrypt_uses_correct_aad_when_extension_is_present() {
        let crypto = TransportCrypto::new(&[0xABu8; 32], "aead_aes256_gcm_rtpsize")
            .expect("crypto should initialize");

        let ssrc = 4284u32;
        let sequence = 10u16;
        let timestamp = 960u32;
        let opus_payload = b"real-opus-frame-data";

        // RTP fixed header: V=2, P=0, X=1, CC=0
        let mut rtp_header = [0u8; RTP_HEADER_LEN];
        rtp_header[0] = 0x90; // V=2, X=1
        rtp_header[1] = OPUS_PT;
        rtp_header[2..4].copy_from_slice(&sequence.to_be_bytes());
        rtp_header[4..8].copy_from_slice(&timestamp.to_be_bytes());
        rtp_header[8..12].copy_from_slice(&ssrc.to_be_bytes());

        // Extension prefix: profile=0xBEDE, length=2 (two 32-bit words of body)
        let ext_prefix: [u8; 4] = [0xBE, 0xDE, 0x00, 0x02];
        let ext_body: [u8; 8] = [0x51, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        // AAD = fixed header (12) + extension prefix (4) = 16 bytes
        let mut aad = Vec::new();
        aad.extend_from_slice(&rtp_header);
        aad.extend_from_slice(&ext_prefix);
        assert_eq!(aad.len(), 16);

        // Plaintext under encryption = ext_body + opus payload
        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(&ext_body);
        plaintext.extend_from_slice(opus_payload);

        // Encrypt with the correct (small) AAD
        let encrypted = crypto.encrypt(&aad, &plaintext).expect("encrypt");

        // Assemble on-wire packet: [aad | ciphertext+tag | nonce]
        let mut packet = Vec::new();
        packet.extend_from_slice(&aad);
        packet.extend_from_slice(&encrypted);

        // parse_rtp_header sees the ciphertext starting at offset 16 (right
        // after the AAD), reads the first 4 bytes of ciphertext as if they
        // were the extension prefix, and interprets the "length" field to
        // compute a header_size that is LARGER than the true AAD.
        let (_, _, _, parsed_header_size, _) =
            parse_rtp_header(&packet).expect("rtp header should parse");
        assert!(
            parsed_header_size > aad.len(),
            "parse_rtp_header should report a header_size ({parsed_header_size}) \
             larger than the true AAD ({}), exposing the mismatch",
            aad.len()
        );

        // decrypt() must recompute the correct AAD boundary from the packet
        // bytes, ignoring the too-large header_size.
        let decrypted = crypto
            .decrypt(&packet, parsed_header_size)
            .expect("decrypt must succeed with extension present");
        assert_eq!(decrypted, plaintext);

        // Verify that using header_size directly as AAD would fail — this is
        // the exact bug that made the bot deaf.
        let wrong_aad_result = crypto.decrypt_with_aad(&packet, parsed_header_size);
        assert!(
            wrong_aad_result.is_err(),
            "using full header_size as AAD should fail decryption"
        );
    }
}

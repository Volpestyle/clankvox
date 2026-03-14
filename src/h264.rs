/// H.264 RTP depacketization and Annex-B bitstream utilities.

pub(crate) const MAX_VIDEO_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct H264Depacketizer {
    pub(crate) timestamp: Option<u32>,
    pub(crate) buffer: Vec<u8>,
    pub(crate) keyframe: bool,
    pub(crate) in_fu: bool,
    /// Cached SPS NAL unit (without start code) from the most recent SPS seen.
    pub(crate) cached_sps: Option<Vec<u8>>,
    /// Cached PPS NAL unit (without start code) from the most recent PPS seen.
    pub(crate) cached_pps: Option<Vec<u8>>,
}

impl H264Depacketizer {
    pub(crate) fn push(
        &mut self,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<(Vec<u8>, bool)> {
        if payload.is_empty() {
            return None;
        }
        self.prepare_timestamp(timestamp);
        let nal_type = payload[0] & 0x1F;
        match nal_type {
            1..=23 => {
                self.cache_parameter_set(nal_type, payload);
                self.append_start_code();
                self.extend(payload)?;
                if nal_type == 5 {
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
                        self.cache_parameter_set(stap_nal_type, nalu);
                        if stap_nal_type == 5 {
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
                    if nal_type == 5 {
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
            let keyframe = self.keyframe || h264_annexb_has_idr_slice(&self.buffer);
            let frame = std::mem::take(&mut self.buffer);
            // NOTE: Do NOT prepend cached SPS+PPS here.  The depacketized
            // frame goes to DAVE decrypt first, and prepending would shift
            // the byte offsets that the DAVE trailer's unencrypted ranges
            // reference, causing decrypt to fail.  SPS+PPS prepend happens
            // AFTER DAVE decrypt in the UDP recv loop.
            self.timestamp = None;
            self.keyframe = false;
            self.in_fu = false;
            return Some((frame, keyframe));
        }

        None
    }

    /// Cache SPS (NAL type 7) and PPS (NAL type 8) NAL units so they can be
    /// prepended to keyframes that arrive without inline parameter sets.
    pub(crate) fn cache_parameter_set(&mut self, nal_type: u8, nalu: &[u8]) {
        match nal_type {
            7 => {
                self.cached_sps = Some(nalu.to_vec());
            }
            8 => {
                self.cached_pps = Some(nalu.to_vec());
            }
            _ => {}
        }
    }

    /// If the assembled frame is a keyframe but doesn't contain SPS/PPS inline,
    /// prepend the cached parameter sets so ffmpeg can decode it standalone.
    pub(crate) fn prepend_cached_parameter_sets(&self, frame: Vec<u8>) -> Vec<u8> {
        let has_sps = Self::annexb_contains_nal_type(&frame, 7);
        let has_pps = Self::annexb_contains_nal_type(&frame, 8);
        if has_sps && has_pps {
            return frame;
        }

        let sps = if !has_sps {
            self.cached_sps.as_deref()
        } else {
            None
        };
        let pps = if !has_pps {
            self.cached_pps.as_deref()
        } else {
            None
        };
        if sps.is_none() && pps.is_none() {
            return frame;
        }

        let start_code: &[u8] = &[0, 0, 0, 1];
        let extra_len = sps.map_or(0, |s| 4 + s.len()) + pps.map_or(0, |p| 4 + p.len());
        let mut out = Vec::with_capacity(extra_len + frame.len());
        if let Some(s) = sps {
            out.extend_from_slice(start_code);
            out.extend_from_slice(s);
        }
        if let Some(p) = pps {
            out.extend_from_slice(start_code);
            out.extend_from_slice(p);
        }
        out.extend_from_slice(&frame);
        out
    }

    /// Scan an Annex-B bitstream for the presence of a specific NAL type.
    pub(crate) fn annexb_contains_nal_type(buf: &[u8], target: u8) -> bool {
        let mut i = 0;
        while i < buf.len().saturating_sub(3) {
            if buf[i] == 0 && buf[i + 1] == 0 {
                let nal_start = if buf[i + 2] == 1 {
                    i + 3
                } else if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                    i + 4
                } else {
                    i += 1;
                    continue;
                };
                if nal_start < buf.len() && (buf[nal_start] & 0x1F) == target {
                    return true;
                }
                i = nal_start;
            } else {
                i += 1;
            }
        }
        false
    }

    pub(crate) fn prepare_timestamp(&mut self, timestamp: u32) {
        if self.timestamp != Some(timestamp) {
            self.timestamp = Some(timestamp);
            self.buffer.clear();
            self.keyframe = false;
            self.in_fu = false;
        }
    }

    pub(crate) fn append_start_code(&mut self) {
        self.buffer.extend_from_slice(&[0, 0, 0, 1]);
    }

    pub(crate) fn extend(&mut self, bytes: &[u8]) -> Option<()> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_VIDEO_FRAME_BYTES {
            self.reset();
            return None;
        }
        self.buffer.extend_from_slice(bytes);
        Some(())
    }

    pub(crate) fn reset(&mut self) {
        self.timestamp = None;
        self.buffer.clear();
        self.keyframe = false;
        self.in_fu = false;
    }
}

pub(crate) fn find_next_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

pub(crate) fn h264_annexb_has_idr_slice(frame: &[u8]) -> bool {
    let mut index = 0usize;
    while index + 4 <= frame.len() {
        if frame[index..].starts_with(&[0, 0, 0, 1]) {
            let nal_start = index + 4;
            if let Some(byte) = frame.get(nal_start) {
                let nal_type = byte & 0x1F;
                if nal_type == 5 {
                    return true;
                }
            }
            index = nal_start;
        } else if frame[index..].starts_with(&[0, 0, 1]) {
            let nal_start = index + 3;
            if let Some(byte) = frame.get(nal_start) {
                let nal_type = byte & 0x1F;
                if nal_type == 5 {
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

pub(crate) fn collect_annexb_nal_types(frame: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut index = 0usize;
    while index + 4 <= frame.len() {
        if frame[index..].starts_with(&[0, 0, 0, 1]) {
            if let Some(byte) = frame.get(index + 4) {
                types.push(byte & 0x1F);
            }
            index += 4;
        } else if frame[index..].starts_with(&[0, 0, 1]) {
            if let Some(byte) = frame.get(index + 3) {
                types.push(byte & 0x1F);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    types
}

pub(crate) fn split_h264_annexb_nalus(frame: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut search_from = 0usize;
    while let Some((start, start_code_len)) = find_next_start_code(frame, search_from) {
        let nal_start = start + start_code_len;
        let nal_end = find_next_start_code(frame, nal_start)
            .map(|(next_start, _)| next_start)
            .unwrap_or(frame.len());
        if nal_start < nal_end {
            nalus.push(&frame[nal_start..nal_end]);
        }
        search_from = nal_end;
    }
    nalus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_video_depacketizer_resets_on_sequence_gap() {
        let mut depacketizer = H264Depacketizer::default();
        let timestamp = 90_000u32;

        // FU-A start fragment (NAL type 5 IDR via fu_header)
        let start_fragment = [0x7C, 0x85, 0xAA];
        assert_eq!(depacketizer.push(timestamp, false, &start_fragment), None);

        // Simulate sequence gap by calling reset(), as VideoDepacketizerState
        // does when it detects a sequence gap before forwarding to H264Depacketizer.
        depacketizer.reset();

        // FU-A end fragment — should be dropped because the depacketizer was reset
        // and in_fu is now false.
        let end_fragment = [0x7C, 0x45, 0xBB];
        assert_eq!(depacketizer.push(timestamp, true, &end_fragment), None);

        // Next standalone frame should succeed after the gap reset.
        let next_frame = [0x65, 0xCC]; // NAL type 5 (IDR)
        let (frame, keyframe) = depacketizer
            .push(timestamp.wrapping_add(3_000), true, &next_frame)
            .expect("standalone h264 packet should survive after gap reset");

        assert_eq!(frame, vec![0, 0, 0, 1, 0x65, 0xCC]);
        assert!(keyframe);
    }

    #[test]
    fn h264_video_depacketizer_does_not_mark_parameter_sets_only_access_unit_as_keyframe() {
        let mut depacketizer = H264Depacketizer::default();
        let timestamp = 120_000u32;

        // SPS (NAL type 7)
        assert_eq!(
            depacketizer.push(timestamp, false, &[0x67, 0x4D, 0x00, 0x33, 0xAB, 0x40]),
            None
        );
        // PPS (NAL type 8)
        assert_eq!(
            depacketizer.push(timestamp, false, &[0x68, 0xEE, 0x3C, 0x80]),
            None
        );
        // SEI (NAL type 6) with marker — completes the access unit
        let (frame, keyframe) = depacketizer
            .push(timestamp, true, &[0x06, 0x05])
            .expect("parameter-set access unit should still emit a frame");

        assert_eq!(
            frame,
            vec![
                0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x33, 0xAB, 0x40, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80,
                0, 0, 0, 1, 0x06, 0x05
            ]
        );
        assert!(!keyframe);
        assert!(!h264_annexb_has_idr_slice(&frame));
    }
}

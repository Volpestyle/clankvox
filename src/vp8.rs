use crate::h264::MAX_VIDEO_FRAME_BYTES;

#[derive(Default)]
pub(crate) struct Vp8Depacketizer {
    timestamp: Option<u32>,
    buffer: Vec<u8>,
    keyframe: bool,
    saw_partition_start: bool,
}

impl Vp8Depacketizer {
    pub(crate) fn push(
        &mut self,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<(Vec<u8>, bool)> {
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

    pub(crate) fn reset(&mut self) {
        self.timestamp = None;
        self.buffer.clear();
        self.keyframe = false;
        self.saw_partition_start = false;
    }
}

pub(crate) fn parse_vp8_payload_descriptor(payload: &[u8]) -> Option<(usize, bool)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp8_video_depacketizer_resets_on_sequence_gap() {
        let mut depacketizer = Vp8Depacketizer::default();
        let timestamp = 45_000u32;

        // First packet: start-of-partition, not marker (frame not complete yet)
        let start_packet = [0x10, 0x00, 0xAA];
        assert_eq!(depacketizer.push(timestamp, false, &start_packet), None);

        // Simulate a sequence gap by calling reset (as VideoDepacketizerState
        // would do on detecting a sequence number gap), then push a
        // continuation packet — it should be dropped because reset cleared state.
        depacketizer.reset();
        let continuation_packet = [0x00, 0xBB];
        assert_eq!(
            depacketizer.push(timestamp, true, &continuation_packet),
            None
        );

        // Next frame on a new timestamp should succeed as a single-packet frame.
        let next_timestamp = timestamp.wrapping_add(3_000);
        let next_frame_packet = [0x10, 0x00, 0xCC];
        let (frame, keyframe) = depacketizer
            .push(next_timestamp, true, &next_frame_packet)
            .expect("single-packet vp8 frame should survive after gap reset");

        assert_eq!(frame, vec![0x00, 0xCC]);
        assert!(keyframe);
    }
}

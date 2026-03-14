use serde_json::{json, Value};

pub(crate) fn build_media_sink_wants_payload(
    wants: &[(u32, u8)],
    pixel_counts: &[(u32, f64)],
) -> Value {
    let any_quality = wants.iter().map(|(_, q)| *q).max().unwrap_or(100);
    let streams = wants
        .iter()
        .fold(serde_json::Map::new(), |mut acc, (ssrc, quality)| {
            acc.insert(ssrc.to_string(), json!(quality));
            acc
        });
    let pixel_counts_map =
        pixel_counts
            .iter()
            .fold(serde_json::Map::new(), |mut acc, (ssrc, pixel_count)| {
                acc.insert(ssrc.to_string(), json!(pixel_count));
                acc
            });
    json!({
        "op": 15,
        "d": {
            "any": any_quality,
            "streams": streams,
            "pixelCounts": pixel_counts_map,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_sink_wants_payload_includes_streams_and_pixel_counts() {
        let payload = build_media_sink_wants_payload(
            &[(4088, 100), (4099, 0)],
            &[(4088, 921_600.0), (4099, 230_400.0)],
        );

        assert_eq!(payload["op"].as_u64(), Some(15));
        assert_eq!(payload["d"]["any"].as_u64(), Some(100));
        assert_eq!(payload["d"]["streams"]["4088"].as_u64(), Some(100));
        assert_eq!(payload["d"]["streams"]["4099"].as_u64(), Some(0));
        assert_eq!(
            payload["d"]["pixelCounts"]["4088"].as_f64(),
            Some(921_600.0)
        );
        assert_eq!(
            payload["d"]["pixelCounts"]["4099"].as_f64(),
            Some(230_400.0)
        );
    }
}

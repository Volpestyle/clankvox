# Go Live: Native Screen Watch And Self Publish

This document consolidates the `clankvox` view of Discord Go Live.

It covers:

- inbound native screen watch (`stream_watch`)
- outbound native self publish (`stream_publish`)
- how Bun and the selfbot gateway feed stream credentials into the subprocess

## Mental Model

Discord Go Live is not “extra fields on the normal voice socket.”

The main voice connection and the Go Live stream connection are separate legs:

- main voice leg: normal audio send/receive, speaking, voice session identity
- stream leg: video receive or send, stream-specific SSRCs, stream-server credentials

That is why `clankvox` models Go Live as extra transport roles instead of trying to force everything through the primary `voice` slot.

## Control Plane Vs Media Plane

Bun and the selfbot gateway own the control plane:

- raw gateway dispatch handling
- stream discovery
- OP18 `STREAM_CREATE`
- OP19 `STREAM_DELETE`
- OP20 `STREAM_WATCH`
- OP22 `STREAM_SET_PAUSED`
- deciding which session should attach to which stream

`clankvox` owns the media plane:

- stream-server WebSocket connection
- UDP media send/receive
- codec advertisement and selection
- DAVE and transport encryption
- inbound frame forwarding
- outbound H264 packetization

## Shared Stream Facts

For both watch and publish, Bun eventually supplies:

- stream endpoint
- stream token
- `rtc_server_id`
- main voice `session_id`
- self user id
- DAVE channel id

The current DAVE channel derivation for stream connections is:

```text
BigInt(rtc_server_id) - 1
```

That value is computed in Bun and passed to `clankvox` over IPC.

## `stream_watch` Flow

Inbound native watch currently works like this:

1. Bun discovers an active Go Live stream for a target user
2. Bun sends OP20 `STREAM_WATCH`
3. Discord returns `STREAM_CREATE` and `STREAM_SERVER_UPDATE`
4. Bun calls `stream_watch_connect`
5. `clankvox` opens the stream-server transport
6. Discord sends video state and media
7. `clankvox` decrypts/depacketizes frames and emits:
   - `user_video_state`
   - `user_video_frame`
   - `user_video_end`
8. Bun decodes sampled VP8 keyframes and H264 IDR access units to JPEG and feeds the higher-level screen-watch pipeline

The receiver path supports H264 and VP8 receive in the current code.

## `stream_publish` Flow

Outbound self publish currently works like this:

1. Bun decides to publish a self-owned stream
2. if needed, Bun sends OP18 `STREAM_CREATE`
3. Bun sends OP22 `STREAM_SET_PAUSED { paused: false }`
4. Discord returns self stream discovery and credentials
5. Bun calls:
   - `stream_publish_connect`
   - `stream_publish_play` for URL-backed publish, or
   - `stream_publish_browser_start` followed by repeated `stream_publish_browser_frame`
6. `clankvox` opens the sender-side stream transport
7. `clankvox` advertises H264 sender capability and announces active video state
8. `clankvox` turns the active source into H264 access units:
   - URL-backed publish uses ffmpeg/yt-dlp
   - browser-session publish feeds PNG frames into ffmpeg over stdin
9. each access unit is DAVE-encrypted, RTP-packetized, and sent over UDP

Pause/resume/stop are split cleanly:

- pause: Bun sends OP22 paused true and `stream_publish_pause`
- resume: Bun reuses the existing stream when possible and sends OP22 paused false plus `stream_publish_resume`
- stop: Bun sends OP19 `STREAM_DELETE` and `stream_publish_stop` / `stream_publish_disconnect`

## Current Sender Boundary

The sender path exists, but it is not yet a general-purpose arbitrary video publisher.

Current rollout:

- publish lifecycle is currently tied to Bun-owned source orchestration
- source support is intentionally narrow and currently centered on:
  - YouTube-backed music/video URLs
  - browser-session PNG frames captured from `BrowserManager`
- sender codec is H264
- transport is the native Discord stream server path, not the share-link fallback path

## Why `voice_conn.rs` Is So Large

[../src/voice_conn.rs](../src/voice_conn.rs) owns the protocol-heavy work for both normal voice and Go Live:

- role-aware identify and select-protocol payloads
- READY parsing and stream SSRC extraction
- OP12/OP18/OP15 handling
- speaking and video-state announcements
- RTP packetization for outbound video
- inbound video depacketization handoff
- transport encryption mode handling

That file is effectively the protocol core of the crate.

## Evidence From Reference Packages

The sibling reference repos were useful for the shape of the solution:

- `../Discord-video-stream`
  - modern Go Live control-plane and sender-side shape
  - speaking flag `2` on the stream connection
  - stream-specific SSRC handling and video announcements
- `../Discord-video-selfbot`
  - older sender-side UDP implementation
  - strong evidence that Go Live still uses a separate stream-server connection and shared main voice `session_id`

`clankvox` does not copy those projects directly. They were used as transport evidence while the implementation stayed aligned with this repo’s DAVE-aware runtime.

## Current Status

### Inbound watch

- integrated end to end
- live validated through the selfbot runtime
- Bun already consumes encoded frames and feeds the screen-watch system

### Outbound publish

- implemented in code
- Rust sender path is covered by crate tests
- Bun control-plane lifecycle has focused tests for create/resume/switch behavior
- Bun browser-session share path forwards live browser frames into the sender transport with focused test coverage
- live Discord sender validation is still the remaining gap

## Key Files

- [../src/voice_conn.rs](../src/voice_conn.rs): role-aware Discord voice/stream transport
- [../src/stream_publish.rs](../src/stream_publish.rs): sender pipeline and H264 frame feed
- [../src/video.rs](../src/video.rs): inbound depacketization helpers
- [../src/capture_supervisor.rs](../src/capture_supervisor.rs): watch-ready handling and subscriptions
- [../src/connection_supervisor.rs](../src/connection_supervisor.rs): role-specific connect/disconnect
- [../src/ipc.rs](../src/ipc.rs): `stream_watch_*` and `stream_publish_*` IPC messages

## Transport Crypto: rtpsize AAD Rules

Discord's `aead_aes256_gcm_rtpsize` and `aead_xchacha20_poly1305_rtpsize` modes
authenticate different slices of the packet depending on packet type:

- **RTP media packets:** AAD = RTP fixed header (12 bytes) + CSRC list (cc * 4
  bytes) + 4-byte extension header prefix (profile + length). The extension
  body is part of the ciphertext, not the AAD. `parse_rtp_header` returns a
  `header_size` that includes the full extension body — this value is correct
  for locating the payload start but must NOT be used as the AAD boundary.
  `decrypt()` recomputes the AAD from the raw packet bytes.
- **RTCP packets:** AAD = the 4-byte RTCP fixed header. `decrypt_with_aad()`
  is used directly with `RTCP_HEADER_LEN`.

Inbound RTCP packets (payload types 72-76 after masking, corresponding to RTCP
types 200-204 per RFC 5761 mux) are filtered early in the UDP recv loop before
any decrypt attempt. They are silently skipped because we do not process
inbound RTCP feedback.

## H264 Keyframe and SPS Strategy

Discord's raw UDP protocol path does not honour PLI or FIR RTCP feedback for
keyframe requests. PLI/FIR only works through the WebRTC protocol path used by
reference implementations like `Discord-video-stream`. Since clankvox uses
`protocol: "udp"`, we cannot request keyframes on demand. PLI/FIR packets are
still sent as a best-effort hint in three scenarios: periodic reassertion
(every 2s), after decoder reset (50 consecutive errors), and after DAVE ready.

To compensate:

- Cached SPS+PPS are prepended to every emitted frame after DAVE decrypt.
  `prepend_cached_parameter_sets` is a no-op when the frame already contains
  inline parameter sets. The prepend happens after DAVE decrypt (not during
  depacketization) so that DAVE trailer byte offsets remain correct.
- Only IDR slices (NAL type 5) are treated as keyframes for rate-limiting and
  forwarding purposes.
- The persistent OpenH264 decoder processes all frames (IDR + P-frames) for
  reference state accumulation with error concealment enabled. The first
  decoded frame may have visual artifacts, but subsequent frames improve as
  P-slice prediction converges. After 50 consecutive decode errors the decoder
  auto-resets and requests PLI.

## DAVE Video Decrypt

DAVE video decrypt works at near 100% on the **main voice connection**. The
root cause of the earlier ~50-60% failure rate was RTP padding bytes not being
stripped after transport decryption: `strip_rtp_padding()` in `rtp.rs` now
removes padding before depacketization, which fixed AES-GCM tag verification
failures on padded FU-A mid-fragments.

**Go Live streams are a separate, still-open problem.** The `davey` crate
reports `UnencryptedWhenPassthroughDisabled` for every Go Live video frame,
but the frame bodies are actually encrypted — feeding them to a decoder
produces `deblocking_filter_idc out of range` and `reference count overflow`
errors characteristic of encrypted data being parsed as H264.

Current behavior: video frames that fail DAVE decrypt with
`UnencryptedWhenPassthroughDisabled` are validated by `looks_like_valid_h264()`
and **dropped** when they appear to be encrypted. Audio passthrough is still
allowed.

The first few frames after DAVE session commit sometimes arrive genuinely
unencrypted (the sender hasn't started encrypting yet). When those frames
contain a real IDR (`nal_types=[7, 8, 6, 5]`), they decode successfully and
bootstrap the screen watch pipeline. After that initial window, DAVE-encrypted
frames are dropped and no new frames reach Bun until the next unencrypted IDR
or a session reconnect.

Fixing Go Live DAVE decrypt requires either:

- updating the `davey` crate to correctly handle Go Live video DAVE framing
- or connecting to Go Live streams using the WebRTC protocol path instead of
  raw UDP (which would also fix PLI/FIR)

## ffmpeg Video Decode

H264 decode is handled entirely in-process by clankvox's persistent OpenH264
decoder (`video_decoder.rs`). H264 frames do not use ffmpeg.

VP8 still uses per-frame ffmpeg decode on the Bun side. The ffmpeg raw demuxer
hangs on single-frame input; Bun works around this by piping through
`cat | ffmpeg -fflags +genpts -f ivf -i pipe:0` which guarantees clean pipe
close and EOF delivery.

## Open Work

- DAVE video decrypt for Go Live streams (see DAVE Video Decrypt section above)
- live Discord validation for sender path
- broader source support for outbound publish
- RTX receive/retransmission work on the video side

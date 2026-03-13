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
8. Bun decodes sampled keyframes to JPEG and feeds the higher-level screen-watch pipeline

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

## Open Work

- live Discord validation for sender path
- broader source support for outbound publish
- RTX receive/retransmission work on the video side

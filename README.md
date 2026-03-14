# clankvox

`clankvox` is the Rust media-plane subprocess for Clanky.

It owns the Discord voice and Go Live transport work that Bun should not do itself:

- main voice transport for audio send/receive
- Discord DAVE lifecycle and transport encryption
- Opus encode/decode and RTP/UDP packet handling
- capture events for ASR and speaking state
- outbound playback for TTS and music
- native Go Live stream watch receive
- native Go Live stream publish send
- JSON-line IPC with the Bun runtime

Bun still owns the selfbot gateway session, orchestration, tools, prompts, settings, and product behavior. `clankvox` is deliberately the transport/media layer, not a second application runtime.

## Current Roles

`clankvox` currently runs three transport roles:

- `voice`: the normal Discord voice connection for audio capture and playback
- `stream_watch`: a second stream-server connection for inbound Go Live video receive
- `stream_publish`: a second stream-server connection for outbound Go Live video send

The `voice` role is always the anchor session. The stream roles are optional and are started by Bun when Discord stream discovery produces the needed credentials.

## Runtime Shape

The process entrypoint is [src/main.rs](./src/main.rs).

At startup `clankvox`:

1. installs rustls crypto
2. starts a single IPC writer and an IPC reader
3. creates shared `AppState`
4. enters one `tokio::select!` loop
5. multiplexes:
   - inbound IPC commands from Bun
   - voice events from `voice_conn`
   - music events from the ffmpeg/yt-dlp pipeline
   - a reconnect timer
   - a 20ms send tick for live audio/video pacing

Most behavior is split across supervisor-style modules:

- [src/app_state.rs](./src/app_state.rs): shared state and transport slots
- [src/connection_supervisor.rs](./src/connection_supervisor.rs): connect/disconnect and reconnect control
- [src/capture_supervisor.rs](./src/capture_supervisor.rs): inbound audio/video events and subscriptions
- [src/playback_supervisor.rs](./src/playback_supervisor.rs): TTS/music playback and periodic send tick
- [src/stream_publish.rs](./src/stream_publish.rs): outbound Go Live sender pipeline
- [src/voice_conn.rs](./src/voice_conn.rs): Discord voice WebSocket, UDP, RTP, codec negotiation, DAVE, packetization
- [src/dave.rs](./src/dave.rs): `davey` wrapper and role-specific encryption/decryption helpers
- [src/ipc.rs](./src/ipc.rs): Bun <-> Rust message contracts

## What To Read

- [docs/architecture.md](./docs/architecture.md): process model, ownership boundaries, transport roles, IPC, module map
- [docs/audio-pipeline.md](./docs/audio-pipeline.md): capture, TTS, music, playback pacing, telemetry
- [docs/go-live.md](./docs/go-live.md): native screen watch, native self publish, stream discovery, sender/receiver flows
- [docs/development.md](./docs/development.md): build/test commands, logs, and where to make changes

## Build And Test

From this directory:

```sh
cargo test
```

Release build on this repo normally uses static opus:

```sh
OPUS_STATIC=1 OPUS_NO_PKG=1 cargo build --release
```

From the repo root, the Bun wrapper uses:

```sh
bun run build:voice
```

## Current Product Boundaries

The Go Live sender path exists, but current rollout is intentionally narrow:

- outbound publish is driven by Bun orchestration, currently through music video relay or explicit browser-session share
- current source gate is YouTube-backed music URLs plus browser-session PNG frames
- sender transport is H264-only
- live sender validation on Discord is still a separate follow-up from the code and test coverage already in place

Inbound native screen watch is already integrated end to end through `stream_watch`.

## Known Limitations

- **DAVE video decrypt on Go Live.** The `davey` crate's video decrypt returns `UnencryptedWhenPassthroughDisabled` for Go Live video frames that are likely encrypted. Video frames that fail DAVE decrypt are dropped. Screen watch relies on the unencrypted frames that arrive during the DAVE session transition window.
- **PLI/FIR does not work.** Discord's media server does not process RTCP PLI/FIR feedback from raw UDP peers (only from WebRTC peers). Keyframes cannot be requested on demand. The H264 depacketizer compensates by always prepending cached SPS+PPS and treating SPS-containing access units as keyframes.
- **ffmpeg H264 EOF.** The H264 raw demuxer hangs on single-frame input. Bun works around this by piping through `cat | ffmpeg -fflags +genpts -f h264 -i pipe:0`.

## Related Top-Level Docs

These local docs consolidate the transport/media-plane parts of several repo docs. The product-facing and cross-runtime docs still live at the repo root:

- [../../../docs/voice/discord-streaming.md](../../../docs/voice/discord-streaming.md)
- [../../../docs/archive/selfbot-stream-watch.md](../../../docs/archive/selfbot-stream-watch.md)
- [../../../docs/voice/voice-provider-abstraction.md](../../../docs/voice/voice-provider-abstraction.md)
- [../../../docs/architecture/overview.md](../../../docs/architecture/overview.md)
- [../../../docs/notes/rust-submodule-dev.md](../../../docs/notes/rust-submodule-dev.md)

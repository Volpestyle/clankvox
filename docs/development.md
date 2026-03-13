# clankvox Development

This document is the practical guide for editing and validating the crate.

## Build And Test

From `src/voice/clankvox`:

```sh
cargo test
```

Release build:

```sh
OPUS_STATIC=1 OPUS_NO_PKG=1 cargo build --release
```

Format:

```sh
cargo fmt
```

From the repo root, Bun uses:

```sh
bun run build:voice
```

## Logging

`clankvox` writes structured logs to stderr through `tracing`.

Default logging is roughly:

```text
info,davey=warn,davey::cryptor::frame_processors=off
```

Useful knobs:

- `RUST_LOG=debug` when you need transport detail
- `AUDIO_DEBUG=1` when you need more audio-path IPC logging

The subprocess intentionally keeps the Bun IPC channel separate from logs. Stdout is for JSON-line IPC. Stderr is for operator logs.

## High-Value Edit Locations

If you are changing…

- voice/session connect logic:
  - [../src/connection_supervisor.rs](../src/connection_supervisor.rs)
  - [../src/voice_conn.rs](../src/voice_conn.rs)
- inbound speaking/audio capture:
  - [../src/capture_supervisor.rs](../src/capture_supervisor.rs)
  - [../src/voice_conn.rs](../src/voice_conn.rs)
- TTS/music playback:
  - [../src/playback_supervisor.rs](../src/playback_supervisor.rs)
  - [../src/audio_pipeline.rs](../src/audio_pipeline.rs)
  - [../src/music.rs](../src/music.rs)
- native watch transport:
  - [../src/voice_conn.rs](../src/voice_conn.rs)
  - [../src/video.rs](../src/video.rs)
  - [../src/capture_supervisor.rs](../src/capture_supervisor.rs)
- native publish transport:
  - [../src/stream_publish.rs](../src/stream_publish.rs)
  - [../src/voice_conn.rs](../src/voice_conn.rs)
  - [../src/connection_supervisor.rs](../src/connection_supervisor.rs)
- Bun IPC contracts:
  - [../src/ipc.rs](../src/ipc.rs)
  - [../src/ipc_protocol.rs](../src/ipc_protocol.rs)
  - [../../clankvoxClient.ts](../../clankvoxClient.ts)

## Coordination With Bun

`clankvox` almost never changes alone.

Most media-plane changes also require checking:

- [../../clankvoxClient.ts](../../clankvoxClient.ts)
- [../../sessionLifecycle.ts](../../sessionLifecycle.ts)
- [../../voiceSessionManager.ts](../../voiceSessionManager.ts)
- [../../../selfbot/streamDiscovery.ts](../../../selfbot/streamDiscovery.ts) for Go Live control-plane changes

When you add a new transport event or command, update both sides in the same change.

## Manual Validation Checklist

### Audio changes

1. run `cargo test`
2. run repo tests that cover `clankvoxClient` / voice lifecycle
3. validate:
   - join still works
   - inbound capture still arrives
   - TTS still drains
   - music still starts, pauses, and resumes cleanly

### Go Live changes

1. run `cargo test`
2. run repo tests for:
   - `src/voice/clankvoxClient.test.ts`
   - stream discovery / screen watch / publish lifecycle tests
3. validate which role changed:
   - `stream_watch`
   - `stream_publish`
4. confirm the right transport-state events are still emitted

## Known Sharp Edges

- DAVE transition handling is protocol-sensitive and easy to break with “small” changes
- stream-server behavior is role-specific; a fix for `voice` is not automatically correct for `stream_watch` or `stream_publish`
- sender and receiver video paths share protocol code, but not identical lifecycle expectations
- Bun may look logically correct while `clankvox` still has buffered playback; trust subprocess telemetry for floor state

## Related Context

For historical findings and older debugging notes, keep these nearby:

- [../../../../docs/notes/rust-submodule-dev.md](../../../../docs/notes/rust-submodule-dev.md)
- [../../../../docs/notes/NATIVE_SCREENSHARE_NOTES.md](../../../../docs/notes/NATIVE_SCREENSHARE_NOTES.md)

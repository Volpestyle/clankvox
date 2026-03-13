# clankvox Architecture

This document is the transport/media-plane view of the system.

## Ownership Boundary

Bun owns:

- the selfbot gateway session
- stream discovery dispatch handling
- session orchestration and product logic
- tools, prompts, settings, and commentary decisions
- decoding native watch frames into JPEG for the higher-level screen-watch pipeline

`clankvox` owns:

- Discord voice and stream-server sockets
- UDP/RTP send and receive
- codec advertisement and media framing
- DAVE lifecycle and media encryption/decryption
- capture events and media telemetry
- TTS/music playback pacing
- outbound native Go Live video packetization

That split is important. `clankvox` should stay transport-native and deterministic. Bun should stay agentic and product-facing.

## Process Model

The entrypoint in [../src/main.rs](../src/main.rs) creates one long-lived `AppState` and drives it from a single async event loop.

The loop reacts to five sources:

- inbound IPC messages from Bun
- `VoiceEvent` messages from active transport connections
- `MusicEvent` messages from the ffmpeg/yt-dlp music pipeline
- reconnect deadlines
- a 20ms tick used for audio send cadence and publish-frame draining

That shape keeps transport logic serialized through `AppState` even though lower-level tasks are running concurrently behind channels.

## Core State

[../src/app_state.rs](../src/app_state.rs) is the shared spine. It holds:

- primary voice connection and pending voice connect inputs
- `stream_watch` connection state and its own DAVE slot
- `stream_publish` connection state and its own DAVE slot
- audio send state for outbound voice playback
- per-user capture state and speaking state
- remote video state and active video subscriptions
- music pipeline state
- stream publish runtime state
- reconnect bookkeeping

The important architectural choice is that each transport role has its own connection slot and its own DAVE manager. Go Live is not modeled as “extra fields on the main voice socket.”

## Transport Roles

### `voice`

The main voice leg for:

- join/leave
- speaking state
- inbound user audio capture
- outbound TTS and music

### `stream_watch`

A separate stream-server connection used only for inbound Go Live receive:

- connects with `rtc_server_id` and stream credentials from Bun
- receives remote OP12/OP18 video state
- decrypts video and forwards encoded frames to Bun over IPC
- never owns the main audio session

### `stream_publish`

A separate stream-server connection used only for outbound Go Live send:

- connects with self-owned stream credentials from Bun
- advertises sender-side H264 support
- announces video state to Discord
- packetizes and transmits outbound H264 access units

## Supervisor Split

The code is organized around four operational surfaces:

### Connection Supervisor

[../src/connection_supervisor.rs](../src/connection_supervisor.rs)

Owns:

- join / destroy
- connect and disconnect commands for all roles
- role-specific reconnect handling
- connection teardown when session metadata changes

### Capture Supervisor

[../src/capture_supervisor.rs](../src/capture_supervisor.rs)

Owns:

- inbound speaking and audio events
- video subscription state
- remote video state merge/update logic
- transport-ready hooks for `stream_watch` and `stream_publish`

### Playback Supervisor

[../src/playback_supervisor.rs](../src/playback_supervisor.rs)

Owns:

- audio playback commands from Bun
- music lifecycle events
- queue draining on the 20ms tick
- buffer depth and TTS playback telemetry
- dispatch of one pending stream-publish frame per tick

### Stream Publish Runtime

[../src/stream_publish.rs](../src/stream_publish.rs)

Owns:

- ffmpeg/yt-dlp sender pipeline
- raw H264 access-unit extraction
- pause/resume/stop handling for the sender subprocess
- sender runtime events and frame queueing into `AppState`

## IPC Contract

[../src/ipc.rs](../src/ipc.rs) is the Bun contract.

Inbound commands are grouped into four conceptual families:

- connection: join, voice server/state updates, stream-watch connect/disconnect, stream-publish connect/disconnect, destroy
- capture: subscribe/unsubscribe user audio and user video
- playback: TTS audio, stop playback, music play/pause/resume/stop/gain
- stream publish runtime: play, pause, resume, stop

Outbound events are also grouped:

- adapter / connection state
- transport state per role
- speaking and user audio capture
- user video state, frames, and end events
- playback and music lifecycle
- buffer depth and TTS playback telemetry
- structured IPC errors

## Module Map

- [../src/voice_conn.rs](../src/voice_conn.rs): Discord transport and protocol heavy lifting
- [../src/dave.rs](../src/dave.rs): DAVE session management and codec-aware encrypt/decrypt helpers
- [../src/audio_pipeline.rs](../src/audio_pipeline.rs): PCM buffering and playback helpers
- [../src/music.rs](../src/music.rs): music pipeline subprocess management
- [../src/video.rs](../src/video.rs): inbound video depacketization and state helpers
- [../src/ipc_protocol.rs](../src/ipc_protocol.rs): routing inbound IPC into command groups
- [../src/ipc_router.rs](../src/ipc_router.rs): dispatches routed commands into supervisors

## Why The Architecture Looks This Way

The big driver is DAVE and Go Live.

Songbird-level abstractions were not sufficient because:

- DAVE control opcodes live on the voice WebSocket
- media encryption/decryption has to be coordinated with codec framing
- Go Live uses a second stream-server connection with different state and lifecycle needs
- sender and receiver roles need different codec and announcement behavior

That is why `clankvox` is a custom transport layer instead of a thin wrapper around an off-the-shelf Discord voice library.

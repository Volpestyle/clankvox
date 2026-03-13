# Audio Pipeline

This document covers the audio path inside `clankvox`: inbound user capture, outbound TTS/music playback, and the telemetry Bun relies on for floor control.

## Scope

Audio in `clankvox` has two big jobs:

- turn Discord voice packets into Bun-visible user audio events
- turn Bun-visible TTS/music/media commands into Discord voice playback

Go Live video send/receive is documented separately in [go-live.md](./go-live.md).

## Inbound Audio Receive

The main `voice` transport receives RTP packets from Discord and processes them in this order:

1. transport decrypt using the negotiated RTP-size AEAD mode
2. RTP parse and SSRC lookup
3. DAVE decrypt for the speaking user
4. Opus decode to PCM
5. channel conversion / resampling into Bun-facing capture format
6. speaking and user-audio IPC emission

At the Bun boundary, capture is exposed through events like:

- `speaking_start`
- `speaking_end`
- `user_audio`
- `user_audio_end`
- `client_disconnect`

Those events are what the higher-level voice session manager uses to decide when a speaker has actually taken the floor and when ASR input is ready to finalize.

## Outbound Playback

Outbound playback is paced on the 20ms tick from [../src/main.rs](../src/main.rs).

Sources:

- live TTS audio pushed from Bun over IPC
- music PCM produced by the local ffmpeg/yt-dlp pipeline

The normal send path is:

1. Bun sends PCM to `clankvox`
2. `clankvox` buffers and normalizes it for Discord send
3. on each 20ms tick, the next frame is encoded to Opus
4. DAVE encrypt runs when the session is in encrypted mode
5. transport AEAD encrypt wraps the RTP payload
6. packet is sent over UDP

This is why Bun does not send Opus frames directly. `clankvox` keeps the pacing and encryption truth local to the transport layer.

## Music Playback

Music is implemented as a local subprocess pipeline in [../src/music.rs](../src/music.rs).

For the current repo shape, music playback typically:

- resolves media with `yt-dlp`
- decodes to raw PCM with `ffmpeg`
- pushes PCM chunks into the same outbound playback path used for TTS

Music also emits lifecycle events back into the main loop, including:

- `music_idle`
- `music_error`
- `music_gain_reached`

Those events let Bun coordinate reply handoff, ducking, and the current Go Live sender lifecycle.

## TTS Buffering And Telemetry

Bun intentionally does not assume audio is “done” as soon as it has sent all PCM to the subprocess.

`clankvox` emits playback telemetry so Bun can reason about actual floor occupancy:

- `buffer_depth`
- `tts_playback_state`
- `player_state`
- `playback_armed`

That telemetry is used for:

- output lock decisions
- barge-in timing
- safe music resume timing
- draining queued assistant utterances only when the subprocess really has headroom

## Capture And Floor Semantics

`clankvox` reports low-level transport truth. It does not decide whether the agent should answer.

Examples:

- it reports that a user started speaking
- it reports PCM bytes and end-of-capture boundaries
- it reports that buffered TTS still exists

Bun then decides:

- whether the capture promotes into a turn
- whether it interrupts current playback
- whether the agent answers or stays silent

That boundary is deliberate. The subprocess should not become a policy engine.

## Key Files

- [../src/voice_conn.rs](../src/voice_conn.rs): RTP receive/send, Opus, packet encryption
- [../src/dave.rs](../src/dave.rs): DAVE encrypt/decrypt for audio and video codecs
- [../src/audio_pipeline.rs](../src/audio_pipeline.rs): outbound audio buffer state
- [../src/playback_supervisor.rs](../src/playback_supervisor.rs): playback commands, tick-driven draining, telemetry
- [../src/capture_supervisor.rs](../src/capture_supervisor.rs): speaking and capture state
- [../src/music.rs](../src/music.rs): music subprocess lifecycle

## Important Constraints

- playback pacing is owned locally by `clankvox`, not Bun
- audio transport truth is ultimately the subprocess state, not just Bun-side queued bytes
- DAVE transitions can temporarily change whether frames are encrypted or passthrough
- buffer telemetry is operational truth but not durable forever; Bun still ages stale positive samples on its side

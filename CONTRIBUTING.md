# Contributing to clankvox

Thanks for your interest in contributing to clankvox. This document covers the basics for getting a PR merged.

## Prerequisites

- **Rust 1.85+** (edition 2024)
- **System libraries:** libopus, libturbojpeg (see build notes below)
- Familiarity with async Rust / tokio is helpful but not required for all areas

## Build

```sh
cargo build
```

Release build with static Opus:

```sh
OPUS_STATIC=1 OPUS_NO_PKG=1 cargo build --release
```

## Test

```sh
cargo test
```

## Format and Lint

```sh
cargo fmt
cargo clippy
```

All PRs should pass `cargo fmt --check` and `cargo clippy` with no warnings. The project enables `clippy::pedantic` — see `Cargo.toml` for the specific lint config and allowed exceptions.

## What's In Scope

clankvox is the transport and media layer. Good contribution targets:

- **Bug fixes** in voice transport, DAVE, codec handling, or IPC
- **Performance improvements** to audio/video pipelines
- **New transport capabilities** (new codec support, protocol improvements)
- **Documentation** improvements to the existing docs in `docs/`
- **Test coverage** for transport edge cases

Out of scope for this repo (these belong in the parent bot repo):

- Agent behavior, prompts, or LLM integration
- Discord gateway / selfbot logic
- Dashboard or UI

## PR Process

1. Fork the repo and create a branch from `master`
2. Make your changes
3. Run `cargo test`, `cargo fmt --check`, and `cargo clippy`
4. Open a PR with a clear description of what changed and why
5. One approval required to merge

## Commit Style

Write concise commit messages that describe the change. Use imperative mood:

- `Fix DAVE video decrypt for multi-frame packets`
- `Add configurable JPEG quality for video decode`
- `Split voice_conn.rs into focused modules`

## Architecture Overview

Before diving in, read these docs to understand the codebase:

- [docs/architecture.md](docs/architecture.md) — process model, module map, ownership boundaries
- [docs/audio-pipeline.md](docs/audio-pipeline.md) — capture, playback, TTS, music
- [docs/go-live.md](docs/go-live.md) — screen watch and stream publish transport
- [docs/development.md](docs/development.md) — build commands, logging, edit locations

## Sharp Edges

A few things to be aware of:

- DAVE transition handling is protocol-sensitive. Small changes can break encryption handoffs.
- Stream-server behavior differs by role (`voice` vs `stream_watch` vs `stream_publish`). A fix for one role is not automatically correct for others.
- The Bun IPC contract (`src/ipc.rs`) must stay in sync with the TypeScript side. If you change IPC messages, note that in your PR.

## License

By contributing to clankvox, you agree that your contributions will be licensed under the [AGPL-3.0](LICENSE).

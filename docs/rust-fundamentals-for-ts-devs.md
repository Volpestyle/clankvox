# Rust Fundamentals for TypeScript Developers

A guide to Rust concepts using real examples from this codebase (`clankvox`). If you have a CS degree and write JS/TS daily, you already know most of the *ideas* — Rust just enforces them at compile time.

---

## Table of Contents

1. [The Big Picture: What Is clankvox?](#the-big-picture)
2. [Modules — Rust's Version of File Imports](#modules)
3. [Types, Structs, and Enums — Interfaces on Steroids](#types-structs-and-enums)
4. [Ownership & Borrowing — The One Thing That Makes Rust Different](#ownership--borrowing)
5. [Option and Result — No More null/undefined](#option-and-result)
6. [Pattern Matching — switch On Steroids](#pattern-matching)
7. [Traits — Like Interfaces, But Better](#traits)
8. [Error Handling — The ? Operator](#error-handling)
9. [Closures and Iterators — Feels Like .map() and .filter()](#closures-and-iterators)
10. [Async/Await — Looks Familiar, Works Different](#asyncawait)
11. [Concurrency Primitives — Arc, Mutex, Channels](#concurrency-primitives)
12. [Smart Pointers — Box, Arc, Rc](#smart-pointers)
13. [Lifetimes — The Scary Part (That's Actually Not Bad)](#lifetimes)
14. [The Module System in Detail](#the-module-system-in-detail)
15. [Cargo — npm but for Rust](#cargo)
16. [Testing — Built In, No Jest Needed](#testing)
17. [Macros — Code That Writes Code](#macros)
18. [Glossary: Rust ↔ TypeScript](#glossary)

---

## The Big Picture

`clankvox` is a **media transport subprocess** — it handles Discord voice connections, audio/video encoding, encryption (DAVE E2EE), and RTP packet handling. It communicates with a Bun/TS parent process over JSON-line IPC on stdin/stdout.

Think of it like this:
```
┌─────────────────────┐      stdin/stdout JSON       ┌──────────────────┐
│   Bun/TS Runtime    │  ◄──────────────────────────► │    clankvox      │
│   (orchestration,   │                               │    (Rust)        │
│    gateway, tools)  │                               │    audio/video   │
│                     │                               │    encryption    │
└─────────────────────┘                               │    UDP/RTP       │
                                                      └──────────────────┘
```

The TS side handles business logic; clankvox handles the real-time media plane where Rust's performance and safety guarantees matter.

---

## Modules

**In TypeScript** you do:
```typescript
import { something } from './myfile';
export function doThing() { ... }
```

**In Rust**, the module system works differently. Look at `src/main.rs`:

```rust
// src/main.rs
mod app_state;       // ← "load src/app_state.rs as a module"
mod audio_pipeline;
mod capture;
mod dave;
mod ipc;
// ...

use crate::app_state::AppState;  // ← "import AppState from this module"
use crate::ipc::{spawn_ipc_reader, spawn_ipc_writer};
```

**Key differences from TS:**
- `mod foo;` is like declaring "this module exists" — it tells the compiler to look for `src/foo.rs`
- `use crate::foo::Bar` is the actual import (like `import { Bar } from './foo'`)
- `crate` means "this project" (like `@/` in a TS monorepo)
- Everything is **private by default** — you need `pub` to export. In TS, everything is public unless you don't export it

**Visibility modifiers you'll see everywhere:**
```rust
pub(crate) struct AppState { ... }  // visible within this crate only (like "internal")
pub fn send_msg(msg: &OutMsg) { ... }  // visible to anyone (like "export")
fn helper() { ... }  // private to this file/module
```

`pub(crate)` is the most common — it means "public within clankvox but not to outside consumers." Since clankvox is a binary (not a library), `pub` and `pub(crate)` behave the same way, but `pub(crate)` signals intent.

---

## Types, Structs, and Enums

### Structs = TypeScript Interfaces + Classes

**TypeScript:**
```typescript
interface SpeakingState {
  lastPacketAt: Date | null;
  isSpeaking: boolean;
}
```

**Rust** (`src/capture.rs`):
```rust
#[derive(Clone, Debug)]
pub(crate) struct SpeakingState {
    pub(crate) last_packet_at: Option<time::Instant>,
    pub(crate) is_speaking: bool,
}
```

Same concept, but:
- `struct` combines data AND behavior (methods go in `impl` blocks)
- `Option<time::Instant>` is Rust for `Date | null` (more on `Option` later)
- `#[derive(Clone, Debug)]` auto-generates `.clone()` and debug printing (like implementing interfaces automatically)

### Enums = TypeScript Discriminated Unions (But Way Better)

This is where Rust really shines. Look at `src/ipc.rs`:

**TypeScript** (how you'd write the IPC protocol):
```typescript
type InMsg =
  | { type: 'join'; guildId: string; channelId: string; selfMute: boolean }
  | { type: 'audio'; pcmBase64: string; sampleRate: number }
  | { type: 'music_play'; url: string }
  | { type: 'destroy' };
```

**Rust:**
```rust
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum InMsg {
    Join {
        #[serde(rename = "guildId")]
        guild_id: String,
        #[serde(rename = "channelId")]
        channel_id: String,
        #[serde(rename = "selfMute", default)]
        self_mute: bool,
    },
    Audio {
        #[serde(rename = "pcmBase64")]
        pcm_base64: String,
        #[serde(rename = "sampleRate", default = "default_sample_rate")]
        sample_rate: u32,
    },
    MusicPlay {
        url: String,
    },
    Destroy,
}
```

**Key insight:** Rust enums can carry data *inside each variant*. Each variant is like a different shape of data. The compiler **forces** you to handle every variant — you can't forget one.

The `#[serde(...)]` attributes are metadata that tell the serialization library how to convert between JSON and Rust types. Like decorators in TS but resolved at compile time.

### Enum Without Data = TypeScript String Literal Union

```rust
// src/rtp.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodecKind {
    H264,
    Vp8,
}
```

This is like `type VideoCodecKind = 'H264' | 'VP8'` in TS, but you can attach methods:

```rust
impl VideoCodecKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Vp8 => "VP8",
        }
    }

    pub(crate) fn payload_type(self) -> u8 {
        match self {
            Self::H264 => H264_PT,
            Self::Vp8 => VP8_PT,
        }
    }
}
```

---

## Ownership & Borrowing

This is **the** thing that makes Rust different from every other language you've used. In JS/TS, the garbage collector handles memory. In Rust, the compiler tracks who "owns" data and when it can be freed.

### The Three Rules

1. **Every value has exactly one owner**
2. **When the owner goes out of scope, the value is dropped (freed)**
3. **You can borrow a reference to the value without taking ownership**

### Borrowing in This Codebase

Look at `src/app_state.rs`:

```rust
pub(crate) fn schedule_reconnect(&mut self, reason: &str) {
    schedule_reconnect(
        &mut self.reconnect_deadline,   // mutable borrow
        &mut self.reconnect_attempt,    // mutable borrow
        self.guild_id,                  // Copy (u64 is cheap to copy)
        self.channel_id,               // Copy
        self.self_mute,                // Copy
        reason,                        // immutable borrow (already a &str)
    );
}
```

**Translation to TS thinking:**
- `&self` = "I'm borrowing this struct, read-only" (like `readonly this`)
- `&mut self` = "I'm borrowing this struct, and I might modify it"
- `&str` = "I'm borrowing a string, read-only" (very cheap, no copy)
- `&mut self.reconnect_deadline` = "I'm lending you write access to this specific field"

**Why this matters:** In TS, you can have ten variables all pointing to the same object and mutate it from anywhere. In Rust, the compiler guarantees that either:
- Many things can **read** a value at once (`&T`), OR
- Exactly **one** thing can **write** to it (`&mut T`)
- **Never both at the same time**

This eliminates entire classes of bugs (data races, use-after-free) at compile time.

### Owned vs. Borrowed Strings

You'll see two string types everywhere:

```rust
// OWNED string — like `let s: string = "hello"` in TS
// This struct owns the memory. When it's dropped, the string is freed.
let owned: String = "hello".to_string();

// BORROWED string — like a reference/pointer to someone else's string
// Cheap to pass around, doesn't allocate.
let borrowed: &str = "hello";  // points to static memory
let also_borrowed: &str = &owned;  // points to the owned string
```

In this codebase, you'll see `String` for data that needs to be stored in structs, and `&str` for function parameters where you just need to read the string.

```rust
// src/ipc.rs — function takes a borrowed string, doesn't need to own it
pub fn send_tts_playback_state(status: &str, reason: &str) { ... }

// src/app_state.rs — struct field owns the string
pub(crate) struct PendingConnection {
    pub(crate) endpoint: Option<String>,  // owns the data
    pub(crate) token: Option<String>,
}
```

**Rule of thumb:** Use `&str` for function parameters, `String` for struct fields.

---

## Option and Result

### Option<T> — Rust's Replacement for null/undefined

**TypeScript:**
```typescript
let endpoint: string | null = null;
if (endpoint !== null) {
  connect(endpoint);
}
```

**Rust:**
```rust
let endpoint: Option<String> = None;
if let Some(ep) = endpoint {
    connect(&ep);
}
```

`Option<T>` is an enum with exactly two variants:
```rust
enum Option<T> {
    Some(T),    // has a value
    None,       // no value
}
```

This codebase uses `Option` everywhere. Look at `AppState`:
```rust
pub(crate) struct AppState {
    pub(crate) guild_id: Option<u64>,       // might not be set yet
    pub(crate) voice_conn: Option<VoiceConnection>,  // might not be connected
    pub(crate) reconnect_deadline: Option<time::Instant>,  // might not need reconnect
}
```

**Common patterns you'll see:**

```rust
// Pattern 1: if let — "if this is Some, do something"
if let Some(ref conn) = self.voice_conn {
    conn.shutdown();
}

// Pattern 2: .unwrap_or() — "give me the value or a default"
let rate = sample_rate.unwrap_or(24000);

// Pattern 3: .map() — "transform the inner value if it exists"
// Just like optional chaining ?. in TS
let name = codec.map(|c| c.as_str());

// Pattern 4: ? operator — "return None if this is None" (more later)
let endpoint = self.pending_conn.endpoint.as_ref()?;
```

### Result<T, E> — Typed Errors Instead of try/catch

**TypeScript:**
```typescript
function connect(): Connection {  // might throw, you just have to know
  if (bad) throw new Error("failed");
  return new Connection();
}
```

**Rust:**
```rust
fn connect() -> Result<Connection, anyhow::Error> {
    if bad {
        return Err(anyhow::anyhow!("failed"));
    }
    Ok(Connection::new())
}
```

`Result<T, E>` is an enum:
```rust
enum Result<T, E> {
    Ok(T),     // success with value
    Err(E),    // failure with error
}
```

The function signature **tells you** it can fail. No surprises. The compiler **forces** you to handle the error.

---

## Pattern Matching

`match` is like `switch` but exhaustive and can destructure data.

**TypeScript:**
```typescript
switch (msg.type) {
  case 'join': handleJoin(msg.guildId); break;
  case 'audio': handleAudio(msg.pcmBase64); break;
  // oops, forgot 'destroy' — no compiler error!
}
```

**Rust** (from `src/ipc_protocol.rs` / `src/ipc_router.rs`):
```rust
match msg {
    InMsg::Join { guild_id, channel_id, self_mute, .. } => {
        // guild_id, channel_id, self_mute are destructured here
        handle_join(&guild_id, &channel_id, self_mute);
    }
    InMsg::Audio { pcm_base64, sample_rate } => {
        handle_audio(&pcm_base64, sample_rate);
    }
    InMsg::Destroy => {
        // handle destroy
    }
    // If you forget a variant → COMPILE ERROR
}
```

**The killer feature:** If you add a new variant to `InMsg`, the compiler will error on every `match` that doesn't handle it. In TS, you add a new type to the union and nothing tells you about the 15 places that need updating.

**Pattern matching with `Option`:**
```rust
// src/main.rs
match deadline {
    Some(deadline) => time::sleep_until(deadline).await,
    None => std::future::pending::<()>().await,  // sleep forever
}
```

**Guards and `matches!` macro:**
```rust
// src/rtp.rs — quick boolean check
pub(crate) fn is_rtx_payload_type(payload_type: u8) -> bool {
    matches!(payload_type, H264_RTX_PT | VP8_RTX_PT)
}
```

`matches!` is a macro that returns `true` if the value matches the pattern. Like a one-liner `switch` that returns a bool.

---

## Traits

Traits are like TypeScript interfaces, but they can also provide default implementations and work with generics.

### Derive Macros — Auto-implementing Traits

You'll see these everywhere in the codebase:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodecKind { H264, Vp8 }

#[derive(Deserialize, Debug)]
pub enum InMsg { ... }

#[derive(Serialize, Debug, Clone)]
pub enum OutMsg { ... }

#[derive(Default, Clone)]
pub(crate) struct PendingConnection { ... }
```

**What each one means:**

| Derive | TS Equivalent | What It Does |
|--------|-------------|-------------|
| `Clone` | spread operator `{...obj}` | Generates `.clone()` for deep copies |
| `Debug` | `JSON.stringify()` | Generates debug string representation |
| `PartialEq, Eq` | `===` | Generates `==` comparison |
| `Default` | `= {}` with defaults | Generates `::default()` with zero/empty values |
| `Serialize` | — | Generates JSON serialization (from `serde`) |
| `Deserialize` | — | Generates JSON deserialization (from `serde`) |
| `Copy` | primitive pass-by-value | Implicit copy on assignment (only for small types) |

### impl Blocks — Adding Methods to Types

```rust
// src/capture.rs
impl UserCaptureState {
    pub(crate) fn new(sample_rate: u32, silence_duration_ms: u32) -> Self {
        Self {
            sample_rate: normalize_sample_rate(sample_rate),
            silence_duration_ms: normalize_silence_duration_ms(silence_duration_ms),
            stream_active: false,
            last_audio_at: None,
        }
    }

    pub(crate) fn touch_audio(&mut self, now: time::Instant) {
        self.stream_active = true;
        self.last_audio_at = Some(now);
    }
}
```

- `fn new(...)` → constructor (by convention, not a keyword)
- `Self` → the type being implemented (like `this` in a constructor)
- `&mut self` → methods that modify the struct
- `&self` → methods that only read

---

## Error Handling

### The ? Operator — Rust's Most Beautiful Feature

**TypeScript** (with lots of try/catch):
```typescript
async function initDave(pv: number, userId: bigint, channelId: bigint) {
  try {
    const session = new DaveSession(pv, userId, channelId);
    const pkg = session.createKeyPackage();
    return { session, pkg };
  } catch (e) {
    throw new Error(`Failed to init DAVE: ${e}`);
  }
}
```

**Rust** (`src/dave.rs`):
```rust
pub fn new(protocol_version: u16, user_id: u64, channel_id: u64) -> Result<(Self, Vec<u8>)> {
    let pv = NonZeroU16::new(protocol_version)
        .context("DAVE protocol version must be non-zero")?;  // ← returns Err early if None

    let mut session = DaveSession::new(pv, user_id, channel_id, None)
        .map_err(|e| anyhow::anyhow!("DaveSession::new failed: {e:?}"))?;  // ← returns Err early

    let pkg = session.create_key_package()
        .map_err(|e| anyhow::anyhow!("create_key_package: {e:?}"))?;  // ← returns Err early

    Ok((Self { session, ready: false, /* ... */ }, pkg))
}
```

The `?` operator means: "If this is `Err`, return the error immediately. If it's `Ok`, unwrap the value and continue."

It's like if every function call automatically did `if (err) return err;` — but type-checked. This eliminates callback hell AND try/catch nesting.

### anyhow — The "Any Error" Crate

This codebase uses `anyhow::Result<T>` which is `Result<T, anyhow::Error>`. It's like having a generic `Error` type that can wrap anything. The `.context()` method adds a human-readable message.

```rust
use anyhow::{Result, bail, Context};

fn do_thing() -> Result<()> {
    // bail! is a macro that returns Err immediately
    bail!("Unsupported transport mode: {other}");

    // .context() adds a message to the error chain
    let cipher = Aes256Gcm::new_from_slice(key)
        .context("Invalid AES-256-GCM secret key")?;

    Ok(())
}
```

---

## Closures and Iterators

Closures and iterators in Rust feel very similar to JS/TS arrow functions and array methods.

**TypeScript:**
```typescript
const ssrcsToRemove = Object.entries(ssrcMap)
  .filter(([_, uid]) => uid === userId)
  .map(([ssrc, _]) => Number(ssrc));
```

**Rust** (`src/app_state.rs`):
```rust
let removed_ssrcs = self.ssrc_map
    .iter()
    .filter_map(|(&ssrc, &mapped_uid)| (mapped_uid == user_id).then_some(ssrc))
    .collect::<Vec<_>>();
```

**Key differences:**
- `|args| body` instead of `(args) => body`
- `.collect::<Vec<_>>()` is needed because iterators are lazy — nothing runs until you collect
- `filter_map` = filter + map in one step (returns `Some(value)` to keep, `None` to skip)
- The `_` in `Vec<_>` means "compiler, you figure out the type"

**More iterator examples from the codebase:**

```rust
// src/dave.rs — find a specific entry in a HashMap
let tid = self.pending_transitions
    .iter()
    .find(|&(_, &pv)| pv == 0)     // find first where protocol_version == 0
    .map(|(&tid, _)| tid);          // extract just the transition_id

// src/app_state.rs — retain (like .filter() that mutates in place)
self.ssrc_map.retain(|_, v| *v != user_id);
```

---

## Async/Await

Rust's async/await looks almost identical to TypeScript, but the runtime is explicit.

**TypeScript:**
```typescript
async function main() {
  const msg = await readMessage();
  await handleMessage(msg);
}
```

**Rust** (`src/main.rs`):
```rust
#[tokio::main]               // ← "use tokio as the async runtime"
async fn main() {
    // ...
    loop {
        tokio::select! {      // ← like Promise.race() but for multiple futures
            msg = inbound_ipc.recv() => {
                // handle IPC message
            }
            Some(event) = voice_event_rx.recv() => {
                // handle voice event
            }
            _ = send_interval.tick() => {
                // 20ms audio send tick
            }
        }
    }
}
```

### tokio::select! — The Event Loop

`tokio::select!` is **the core pattern** of this app. It's like `Promise.race()` but:
- It runs inside a loop, so it keeps racing
- When one branch "wins", the others are cancelled
- The `biased;` keyword (seen in `src/ipc.rs`) means "check branches in order" (prioritize control messages over audio)

**This is the main event loop pattern** — it multiplexes IPC messages, voice events, music events, reconnect timers, and audio ticks. This is exactly what Node's event loop does for you automatically, but here it's explicit.

### Spawning Tasks = Starting Promises

```rust
// Like starting a promise that runs "in the background"
tokio::spawn(async move {
    // this runs concurrently
});
```

The `move` keyword means the closure **takes ownership** of any variables it captures (like how TS closures capture by reference, but Rust needs to know who owns what).

---

## Concurrency Primitives

### Arc — Shared Ownership Across Threads

**Problem:** Rust's ownership system says one thing owns the data. But what if multiple async tasks need access?

**Solution:** `Arc<T>` (Atomic Reference Counted). It's like a shared pointer with a reference count.

```rust
// src/main.rs
let dave: Arc<Mutex<Option<DaveManager>>> = Arc::new(Mutex::new(None));
let audio_send_state = Arc::new(Mutex::new(None::<AudioSendState>));
```

**TypeScript mental model:**
```typescript
// Arc<Mutex<Option<DaveManager>>> is like:
let dave: { current: DaveManager | null } = { current: null };
// ...except thread-safe with a lock
```

### Mutex — One Writer at a Time

`Mutex<T>` provides exclusive access to data. You `.lock()` it, do your work, and the lock is released automatically when it goes out of scope.

```rust
// src/app_state.rs
pub(crate) fn clear_voice_connection(&mut self) {
    if let Some(ref conn) = self.voice_conn {
        conn.shutdown();
    }
    self.voice_conn = None;
    *self.dave.lock() = None;           // lock, set to None, unlock
    *self.audio_send_state.lock() = None;
}
```

Note: This codebase uses `parking_lot::Mutex` instead of `std::sync::Mutex` — it's a faster implementation with a nicer API (no `.unwrap()` needed on `.lock()`).

### Channels — Like EventEmitter or Message Passing

This codebase uses channels extensively for communicating between threads/tasks:

```rust
// src/main.rs — tokio channels for async tasks
let (voice_event_tx, mut voice_event_rx) = tokio::sync::mpsc::channel::<VoiceEvent>(256);

// src/main.rs — crossbeam channels for OS threads
let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(100);
```

**TypeScript mental model:**
```typescript
// mpsc::channel is like:
const { readable, writable } = new TransformStream();
// or like EventEmitter with typed events and backpressure
```

- `tx` = transmitter (sender) — like `emitter.emit()`
- `rx` = receiver — like `emitter.on()`
- `mpsc` = "multiple producer, single consumer" — many senders, one reader
- `bounded(100)` = max 100 items in queue before senders block (backpressure!)
- `try_send` = non-blocking send, returns error if full (used for real-time audio where dropping is better than blocking)

---

## Smart Pointers

### Box<T> — Heap Allocation

```rust
// src/transport_crypto.rs
pub(crate) enum TransportCipher {
    Aes256GcmRtpSize(Box<Aes256Gcm>),     // ← Box means "heap-allocated"
    XChaCha20Poly1305RtpSize(XChaCha20Poly1305),
}
```

`Box<T>` puts data on the heap instead of the stack. Used when:
- The data is large (like a cipher object)
- You need the size to be known at compile time (Box is always pointer-sized)

In TS, everything is on the heap. In Rust, you choose.

---

## Lifetimes

Lifetimes tell the compiler "this reference is valid for at least this long." Most of the time, the compiler figures them out automatically (called "lifetime elision").

You'll see the `'static` lifetime in this codebase:

```rust
pub(crate) fn as_str(self) -> &'static str {
    match self {
        Self::H264 => "H264",  // these string literals live forever
        Self::Vp8 => "VP8",
    }
}
```

`'static` means "this reference lives for the entire program." String literals are `'static` because they're embedded in the binary.

**When do you write lifetimes explicitly?** Rarely in application code. Libraries and complex data structures need them more. In clankvox, you'll barely see them because the code prefers owned types (`String` over `&str` in structs) which sidesteps lifetime complexity.

---

## The Module System in Detail

The codebase is organized into focused modules:

```
src/
├── main.rs                    # entry point, event loop
├── app_state.rs               # shared state struct + methods
├── ipc.rs                     # stdin/stdout IPC (InMsg, OutMsg enums)
├── ipc_protocol.rs            # routes InMsg to typed command enums
├── ipc_router.rs              # dispatches commands to handlers
├── voice_conn.rs              # Discord voice WebSocket + UDP
├── dave.rs                    # DAVE E2EE encryption/decryption
├── transport_crypto.rs        # RTP payload AEAD (AES-GCM, XChaCha20)
├── rtp.rs                     # RTP header build/parse, padding strip
├── h264.rs                    # H264 depacketizer
├── vp8.rs                     # VP8 depacketizer
├── audio_pipeline.rs          # Opus encode/decode, PCM mixing
├── capture.rs                 # speaking detection, user audio state
├── capture_supervisor.rs      # inbound audio/video event routing
├── playback_supervisor.rs     # TTS/music playback, 20ms send tick
├── music.rs                   # yt-dlp/ffmpeg music pipeline
├── video.rs                   # video subscription state
├── video_decoder.rs           # persistent OpenH264 decoder
├── video_state.rs             # video SSRC tracking
├── stream_publish.rs          # outbound Go Live sender
├── media_sink_wants.rs        # codec negotiation helpers
├── connection_supervisor.rs   # connect/reconnect logic
├── ipc_log_layer.rs           # tracing → IPC log bridge
└── rtcp.rs                    # RTCP feedback packets
```

Each module is a single `.rs` file. Larger Rust projects might use directories with `mod.rs`, but flat files work great here.

---

## Cargo

`Cargo.toml` is `package.json` for Rust.

```toml
[package]
name = "clankvox"
version = "0.3.0"
edition = "2024"           # Rust edition (like TS target: "ES2024")

[dependencies]
tokio = { version = "1", features = ["full"] }   # async runtime
serde = { version = "1.0", features = ["derive"] }  # JSON serialization
serde_json = "1.0"
anyhow = "1"               # error handling
parking_lot = "0.12"        # fast Mutex
crossbeam-channel = "0.5"   # thread-safe channels

[profile.release]
opt-level = 3              # maximum optimization
lto = "thin"               # link-time optimization
```

**Cargo ↔ npm mapping:**

| Cargo | npm |
|-------|-----|
| `Cargo.toml` | `package.json` |
| `Cargo.lock` | `package-lock.json` |
| `cargo build` | `npm run build` |
| `cargo test` | `npm test` |
| `cargo run` | `npm start` |
| `crates.io` | `npmjs.com` |
| `features = [...]` | optional peer dependencies |

### Lints Section

```toml
[lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
cast_possible_truncation = "allow"  # noisy in real-time audio code
```

Clippy is Rust's linter (like ESLint). This config says "warn on almost everything, but allow some casts that are normal in audio code."

---

## Testing

Tests live right next to the code, inside `#[cfg(test)]` blocks:

```rust
// src/rtp.rs — tests at the bottom of the same file
#[cfg(test)]
mod tests {
    use super::*;  // import everything from the parent module

    #[test]
    fn rtp_header_round_trips() {
        let header = build_rtp_header(321, 123_456, 987_654_321);
        let parsed = parse_rtp_header(&header).expect("header should parse");
        assert_eq!(parsed, (321, 123_456, 987_654_321, RTP_HEADER_LEN, false));
    }

    #[test]
    fn strip_rtp_padding_removes_trailing_pad_bytes() {
        let mut packet = [0u8; RTP_HEADER_LEN];
        packet[0] = 0xA0; // V=2, P=1
        let decrypted = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x03];
        let result = strip_rtp_padding(&packet, decrypted);
        assert_eq!(result, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
```

**Key differences from Jest/Vitest:**
- `#[cfg(test)]` = "only compile this when running tests" (dead code elimination)
- `use super::*` = import the parent module (the actual code being tested)
- `assert_eq!(a, b)` instead of `expect(a).toBe(b)`
- `.expect("message")` unwraps a Result/Option or panics with the message (like an assertion)
- Run with `cargo test` — no test framework config needed

---

## Macros

Macros are "code that writes code." You'll see them throughout:

```rust
// tracing::info! — structured logging (like console.log but with fields)
tracing::info!(
    attempt = *reconnect_attempt,
    backoff_ms = backoff_ms,
    reason = reason,
    "scheduled clankvox voice reconnect"
);

// serde_json::json! — build JSON values inline
serde_json::json!({
    "op": 4,
    "d": {
        "guild_id": guild_id.to_string(),
        "channel_id": channel_id.to_string(),
    }
});

// vec! — create a vector (like array literal)
let data = vec![0u8; 32];  // 32 zeros

// anyhow::anyhow! — create an error with formatting
anyhow::anyhow!("DaveSession::new failed: {e:?}")

// matches! — boolean pattern match
matches!(msg, InMsg::Audio { .. })  // is this an Audio message?
```

Macros end with `!` to distinguish them from functions. They're expanded at compile time, so they're zero-cost.

---

## Glossary

| Rust | TypeScript | Notes |
|------|-----------|-------|
| `fn` | `function` | |
| `let` | `const` | Rust `let` is immutable by default |
| `let mut` | `let` | Mutable binding |
| `struct` | `interface` + `class` | Data + methods via `impl` |
| `enum` | discriminated union | Can carry data per variant |
| `impl Foo` | `class Foo { methods }` | Methods on a type |
| `trait` | `interface` | Can have default implementations |
| `Option<T>` | `T \| null` | `Some(value)` or `None` |
| `Result<T, E>` | try/catch with typed errors | `Ok(value)` or `Err(error)` |
| `Vec<T>` | `Array<T>` | Growable array |
| `HashMap<K, V>` | `Map<K, V>` | |
| `String` | `string` (owned) | Heap-allocated, growable |
| `&str` | `string` (borrowed ref) | Cheap reference to string data |
| `Box<T>` | (everything is boxed in JS) | Explicit heap allocation |
| `Arc<T>` | shared reference | Thread-safe reference counting |
| `Mutex<T>` | (no equivalent) | Exclusive access lock |
| `clone()` | spread / structuredClone | Explicit deep copy |
| `pub` | `export` | |
| `pub(crate)` | (no equivalent) | Internal to package |
| `use` | `import` | |
| `mod` | (file = module in TS) | Declare a submodule |
| `crate` | `@/` or `~/` | Root of current package |
| `self` | `this` | |
| `Self` | `typeof this` / class name | The current type |
| `u8, u16, u32, u64` | `number` | Unsigned integers (specific sizes!) |
| `i16, i32` | `number` | Signed integers |
| `f32, f64` | `number` | Floats |
| `usize` | `number` | Pointer-sized unsigned int |
| `bool` | `boolean` | |
| `()` | `void` | The "unit type" (empty tuple) |
| `todo!()` | `throw new Error("TODO")` | Panics at runtime |
| `unreachable!()` | (no equivalent) | "This should never happen" |
| `panic!()` | `throw new Error()` | Unrecoverable error |
| `?` | early return on error | Propagates Err/None |
| `::` | `.` (static method) | Associated function call |
| `.` | `.` (instance method) | Method call |

---

## What To Read Next

Now that you have the mental model, try reading the files in this order:

1. **`src/capture.rs`** — Simplest file. Small structs, basic methods.
2. **`src/rtp.rs`** — Pure functions, bit manipulation, good tests.
3. **`src/ipc.rs`** — Big enums with serde, IPC read/write threads, channels.
4. **`src/transport_crypto.rs`** — Enum dispatch, crypto, nice round-trip tests.
5. **`src/dave.rs`** — More complex state management, error handling patterns.
6. **`src/app_state.rs`** — The big state struct, see how everything connects.
7. **`src/main.rs`** — The event loop that ties it all together.

The [Rust Book](https://doc.rust-lang.org/book/) is the canonical learning resource. Chapters 4 (ownership), 6 (enums/matching), and 10 (generics/traits/lifetimes) are the most important for understanding this codebase.

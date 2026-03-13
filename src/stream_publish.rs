use std::collections::VecDeque;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crossbeam_channel as crossbeam;
use tracing::{info, warn};

use crate::app_state::AppState;
use crate::ipc_protocol::StreamPublishCommand;
use crate::voice_conn::TransportRole;

const STREAM_PUBLISH_STDERR_TAIL_LINES: usize = 24;
pub(crate) const STREAM_PUBLISH_TARGET_FPS: u32 = 30;
const STREAM_PUBLISH_TARGET_WIDTH: u32 = 1280;
const STREAM_PUBLISH_TARGET_HEIGHT: u32 = 720;
const STREAM_PUBLISH_VIDEO_BITRATE_KBPS: u32 = 2_500;
const STREAM_PUBLISH_BROWSER_FRAME_MAX_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct StreamPublishFrame {
    pub(crate) access_unit: Vec<u8>,
    pub(crate) timestamp_increment: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamPublishSource {
    Url {
        url: String,
        resolved_direct_url: bool,
    },
    BrowserFrames {
        mime_type: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum StreamPublishEvent {
    Idle,
    Error(String),
    FirstFrame { startup_ms: u64, fps: u32 },
}

#[derive(Default)]
pub(crate) struct StreamPublishState {
    pub(crate) player: Option<StreamPublishPlayer>,
    pub(crate) pending_source: Option<StreamPublishSource>,
    pub(crate) active_source: Option<StreamPublishSource>,
    pub(crate) active: bool,
    pub(crate) paused: bool,
}

impl StreamPublishState {
    pub(crate) fn queue_pending_start(&mut self, source: StreamPublishSource) {
        self.pending_source = Some(source);
    }

    pub(crate) fn clear_pending_start(&mut self) {
        self.pending_source = None;
    }

    pub(crate) fn stop_player(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.stop_player();
        self.pending_source = None;
        self.active_source = None;
        self.active = false;
        self.paused = false;
    }
}

enum StreamPublishPlayerMode {
    Url,
    BrowserFrames {
        mime_type: String,
        stdin: Arc<parking_lot::Mutex<Option<std::process::ChildStdin>>>,
        timestamp_increments: Arc<parking_lot::Mutex<VecDeque<u32>>>,
        last_captured_at_ms: Arc<AtomicU64>,
    },
}

pub(crate) struct StreamPublishPlayer {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    child_pid: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
    mode: StreamPublishPlayerMode,
}

fn kill_stream_publish_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    if pid == 0 {
        return Ok(());
    }
    #[allow(unsafe_code, clippy::cast_possible_wrap)]
    let rc = unsafe { libc::killpg(pid as libc::pid_t, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn terminate_stream_publish_child(child: &mut std::process::Child, signal: libc::c_int) {
    if let Err(error) = kill_stream_publish_process_group(child.id(), signal) {
        if error.kind() != io::ErrorKind::NotFound {
            warn!(
                pid = child.id(),
                error = %error,
                "failed to signal stream publish process group"
            );
        }
    }
}

fn find_next_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

fn find_next_aud_start(data: &[u8], from: usize) -> Option<usize> {
    let mut search_from = from;
    while let Some((index, start_code_len)) = find_next_start_code(data, search_from) {
        let nal_start = index + start_code_len;
        if data.get(nal_start).is_some_and(|byte| (byte & 0x1f) == 9) {
            return Some(index);
        }
        search_from = nal_start;
    }
    None
}

fn drain_h264_access_units(buffer: &mut Vec<u8>, flush_tail: bool) -> Vec<Vec<u8>> {
    let Some(first_aud) = find_next_aud_start(buffer, 0) else {
        return Vec::new();
    };
    if first_aud > 0 {
        buffer.drain(..first_aud);
    }

    let mut out = Vec::new();
    while let Some(next_aud) = find_next_aud_start(buffer, 4) {
        if next_aud == 0 {
            break;
        }
        let access_unit = buffer.drain(..next_aud).collect::<Vec<_>>();
        if !access_unit.is_empty() {
            out.push(access_unit);
        }
    }

    if flush_tail && !buffer.is_empty() {
        out.push(std::mem::take(buffer));
    }

    out
}

pub(crate) fn build_stream_publish_pipeline_command(
    url: &str,
    resolved_direct_url: bool,
) -> String {
    let quoted_url = url.replace('\'', "'\\''");
    let ffmpeg_tail = format!(
        "ffmpeg -nostdin -loglevel error -re -i {{input}} -an -sn -dn -vf \"scale=w={STREAM_PUBLISH_TARGET_WIDTH}:h={STREAM_PUBLISH_TARGET_HEIGHT}:force_original_aspect_ratio=decrease:flags=lanczos,pad={STREAM_PUBLISH_TARGET_WIDTH}:{STREAM_PUBLISH_TARGET_HEIGHT}:(ow-iw)/2:(oh-ih)/2:black,fps={STREAM_PUBLISH_TARGET_FPS}\" -c:v libx264 -preset veryfast -tune zerolatency -pix_fmt yuv420p -profile:v baseline -level 3.1 -g {STREAM_PUBLISH_TARGET_FPS} -keyint_min {STREAM_PUBLISH_TARGET_FPS} -sc_threshold 0 -b:v {STREAM_PUBLISH_VIDEO_BITRATE_KBPS}k -maxrate {STREAM_PUBLISH_VIDEO_BITRATE_KBPS}k -bufsize {}k -f h264 -bsf:v h264_metadata=aud=insert pipe:1",
        STREAM_PUBLISH_VIDEO_BITRATE_KBPS * 2
    );

    if resolved_direct_url {
        ffmpeg_tail.replace("{input}", &format!("'{quoted_url}'"))
    } else {
        format!(
            "yt-dlp --no-warnings --quiet --no-playlist --extractor-args 'youtube:player_client=android' -f \"bestvideo[ext=mp4][vcodec*=avc1]/bestvideo[vcodec*=avc1]/bestvideo/best\" -o - '{quoted_url}' | {}",
            ffmpeg_tail.replace("{input}", "pipe:0")
        )
    }
}

pub(crate) fn build_stream_publish_browser_pipeline_command(mime_type: &str) -> String {
    let codec = match normalize_browser_frame_mime_type(mime_type) {
        Some("image/png") => "png",
        _ => "png",
    };
    format!(
        "ffmpeg -nostdin -loglevel error -f image2pipe -codec:v {codec} -framerate {STREAM_PUBLISH_TARGET_FPS} -i pipe:0 -an -sn -dn -vf \"scale=w={STREAM_PUBLISH_TARGET_WIDTH}:h={STREAM_PUBLISH_TARGET_HEIGHT}:force_original_aspect_ratio=decrease:flags=lanczos,pad={STREAM_PUBLISH_TARGET_WIDTH}:{STREAM_PUBLISH_TARGET_HEIGHT}:(ow-iw)/2:(oh-ih)/2:black\" -c:v libx264 -preset veryfast -tune zerolatency -pix_fmt yuv420p -profile:v baseline -level 3.1 -g {STREAM_PUBLISH_TARGET_FPS} -keyint_min {STREAM_PUBLISH_TARGET_FPS} -sc_threshold 0 -b:v {STREAM_PUBLISH_VIDEO_BITRATE_KBPS}k -maxrate {STREAM_PUBLISH_VIDEO_BITRATE_KBPS}k -bufsize {}k -f h264 -bsf:v h264_metadata=aud=insert pipe:1",
        STREAM_PUBLISH_VIDEO_BITRATE_KBPS * 2
    )
}

fn normalize_browser_frame_mime_type(mime_type: &str) -> Option<&'static str> {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        _ => None,
    }
}

fn decode_stream_publish_browser_frame(frame_base64: &str) -> Result<Vec<u8>, String> {
    let normalized = frame_base64.trim();
    if normalized.is_empty() {
        return Err("stream_publish_browser_frame_missing_bytes".to_string());
    }
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, normalized)
        .map_err(|error| format!("stream_publish_browser_frame_invalid_base64: {error}"))?;
    if decoded.is_empty() {
        return Err("stream_publish_browser_frame_empty".to_string());
    }
    if decoded.len() > STREAM_PUBLISH_BROWSER_FRAME_MAX_BYTES {
        return Err(format!(
            "stream_publish_browser_frame_too_large:{}",
            decoded.len()
        ));
    }
    Ok(decoded)
}

fn compute_browser_frame_timestamp_increment(
    last_captured_at_ms: &AtomicU64,
    captured_at_ms: u64,
) -> u32 {
    let default_increment = 90_000 / STREAM_PUBLISH_TARGET_FPS;
    if captured_at_ms == 0 {
        return default_increment;
    }
    let previous = last_captured_at_ms.swap(captured_at_ms, Ordering::SeqCst);
    if previous == 0 || captured_at_ms <= previous {
        return default_increment;
    }
    let delta_ms = captured_at_ms.saturating_sub(previous).clamp(1, 5_000);
    let increment = ((delta_ms as u128) * 90_000u128) / 1_000u128;
    increment
        .clamp(1, u128::from(u32::MAX))
        .try_into()
        .unwrap_or(default_increment)
}

impl StreamPublishPlayer {
    pub(crate) fn start_url(
        url: &str,
        frame_tx: crossbeam::Sender<StreamPublishFrame>,
        event_tx: crossbeam::Sender<StreamPublishEvent>,
        resolved_direct_url: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let paused_thread = paused.clone();
        let child_pid = Arc::new(AtomicU32::new(0));
        let child_pid_thread = child_pid.clone();
        let url = url.to_string();

        let thread = std::thread::spawn(move || {
            let pipeline_command = build_stream_publish_pipeline_command(&url, resolved_direct_url);
            let pipeline_started_at = tokio::time::Instant::now();
            let child = std::process::Command::new("sh")
                .process_group(0)
                .args(["-c", &pipeline_command])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    let _ = event_tx.send(StreamPublishEvent::Error(format!(
                        "stream publish yt-dlp/ffmpeg spawn failed: {error}"
                    )));
                    return;
                }
            };
            child_pid_thread.store(child.id(), Ordering::SeqCst);

            let stderr_tail = Arc::new(parking_lot::Mutex::new(VecDeque::<String>::new()));
            let mut stderr_thread = child.stderr.take().map(|stderr| {
                let stderr_tail = stderr_tail.clone();
                std::thread::spawn(move || {
                    let reader = io::BufReader::new(stderr);
                    for line_result in reader.lines() {
                        let line = match line_result {
                            Ok(value) => value.trim().to_string(),
                            Err(_) => break,
                        };
                        if line.is_empty() {
                            continue;
                        }
                        let mut tail = stderr_tail.lock();
                        if tail.len() >= STREAM_PUBLISH_STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })
            });

            let Some(mut stdout) = child.stdout.take() else {
                let _ = event_tx.send(StreamPublishEvent::Error(
                    "stream publish pipeline missing stdout".to_string(),
                ));
                terminate_stream_publish_child(&mut child, libc::SIGTERM);
                let _ = child.wait();
                if let Some(handle) = stderr_thread.take() {
                    let _ = handle.join();
                }
                child_pid_thread.store(0, Ordering::SeqCst);
                return;
            };

            let mut first_frame_reported = false;
            let mut read_buffer = [0u8; 16 * 1024];
            let mut h264_buffer = Vec::<u8>::with_capacity(256 * 1024);

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }

                match stdout.read(&mut read_buffer) {
                    Ok(0) => break,
                    Ok(bytes_read) => {
                        h264_buffer.extend_from_slice(&read_buffer[..bytes_read]);
                        for access_unit in drain_h264_access_units(&mut h264_buffer, false) {
                            if !first_frame_reported {
                                first_frame_reported = true;
                                let startup_ms = pipeline_started_at.elapsed().as_millis() as u64;
                                info!(
                                    startup_ms,
                                    fps = STREAM_PUBLISH_TARGET_FPS,
                                    resolved_direct_url,
                                    "stream publish produced first video frame"
                                );
                                let _ = event_tx.send(StreamPublishEvent::FirstFrame {
                                    startup_ms,
                                    fps: STREAM_PUBLISH_TARGET_FPS,
                                });
                            }
                            if frame_tx
                                .send(StreamPublishFrame {
                                    access_unit,
                                    timestamp_increment: 90_000 / STREAM_PUBLISH_TARGET_FPS,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(StreamPublishEvent::Error(format!(
                            "stream publish stdout read failed: {error}"
                        )));
                        break;
                    }
                }
            }

            if !stop_clone.load(Ordering::Relaxed) {
                for access_unit in drain_h264_access_units(&mut h264_buffer, true) {
                    let _ = frame_tx.send(StreamPublishFrame {
                        access_unit,
                        timestamp_increment: 90_000 / STREAM_PUBLISH_TARGET_FPS,
                    });
                }
            }

            terminate_stream_publish_child(&mut child, libc::SIGTERM);
            let wait_result = child.wait();
            if let Some(handle) = stderr_thread.take() {
                let _ = handle.join();
            }
            child_pid_thread.store(0, Ordering::SeqCst);
            paused_thread.store(false, Ordering::SeqCst);

            let stderr_summary = {
                let tail = stderr_tail.lock();
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(
                        " | stderr tail: {}",
                        tail.iter().cloned().collect::<Vec<_>>().join(" || ")
                    )
                }
            };

            if !stop_clone.load(Ordering::Relaxed) {
                match wait_result {
                    Ok(status) if status.success() => {
                        let _ = event_tx.send(StreamPublishEvent::Idle);
                    }
                    Ok(status) => {
                        let _ = event_tx.send(StreamPublishEvent::Error(format!(
                            "stream publish pipeline exited with status {status}{stderr_summary}"
                        )));
                    }
                    Err(error) => {
                        let _ = event_tx.send(StreamPublishEvent::Error(format!(
                            "stream publish pipeline wait failed: {error}{stderr_summary}"
                        )));
                    }
                }
            }
        });

        Self {
            stop,
            paused,
            child_pid,
            thread: Some(thread),
            mode: StreamPublishPlayerMode::Url,
        }
    }

    pub(crate) fn start_browser_frames(
        mime_type: &str,
        frame_tx: crossbeam::Sender<StreamPublishFrame>,
        event_tx: crossbeam::Sender<StreamPublishEvent>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let paused_thread = paused.clone();
        let child_pid = Arc::new(AtomicU32::new(0));
        let child_pid_thread = child_pid.clone();
        let mime_type = normalize_browser_frame_mime_type(mime_type)
            .unwrap_or("image/png")
            .to_string();
        let stdin = Arc::new(parking_lot::Mutex::new(None));
        let stdin_thread = stdin.clone();
        let timestamp_increments = Arc::new(parking_lot::Mutex::new(VecDeque::<u32>::new()));
        let timestamp_increments_thread = timestamp_increments.clone();
        let last_captured_at_ms = Arc::new(AtomicU64::new(0));
        let browser_mime_type = mime_type.clone();

        let thread = std::thread::spawn(move || {
            let pipeline_command = build_stream_publish_browser_pipeline_command(&browser_mime_type);
            let pipeline_started_at = tokio::time::Instant::now();
            let child = std::process::Command::new("sh")
                .process_group(0)
                .args(["-c", &pipeline_command])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    let _ = event_tx.send(StreamPublishEvent::Error(format!(
                        "stream publish browser ffmpeg spawn failed: {error}"
                    )));
                    return;
                }
            };
            child_pid_thread.store(child.id(), Ordering::SeqCst);
            *stdin_thread.lock() = child.stdin.take();

            let stderr_tail = Arc::new(parking_lot::Mutex::new(VecDeque::<String>::new()));
            let mut stderr_thread = child.stderr.take().map(|stderr| {
                let stderr_tail = stderr_tail.clone();
                std::thread::spawn(move || {
                    let reader = io::BufReader::new(stderr);
                    for line_result in reader.lines() {
                        let line = match line_result {
                            Ok(value) => value.trim().to_string(),
                            Err(_) => break,
                        };
                        if line.is_empty() {
                            continue;
                        }
                        let mut tail = stderr_tail.lock();
                        if tail.len() >= STREAM_PUBLISH_STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })
            });

            let Some(mut stdout) = child.stdout.take() else {
                let _ = event_tx.send(StreamPublishEvent::Error(
                    "stream publish browser pipeline missing stdout".to_string(),
                ));
                *stdin_thread.lock() = None;
                terminate_stream_publish_child(&mut child, libc::SIGTERM);
                let _ = child.wait();
                if let Some(handle) = stderr_thread.take() {
                    let _ = handle.join();
                }
                child_pid_thread.store(0, Ordering::SeqCst);
                return;
            };

            let mut first_frame_reported = false;
            let mut read_buffer = [0u8; 16 * 1024];
            let mut h264_buffer = Vec::<u8>::with_capacity(256 * 1024);

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }

                match stdout.read(&mut read_buffer) {
                    Ok(0) => break,
                    Ok(bytes_read) => {
                        h264_buffer.extend_from_slice(&read_buffer[..bytes_read]);
                        for access_unit in drain_h264_access_units(&mut h264_buffer, false) {
                            if !first_frame_reported {
                                first_frame_reported = true;
                                let startup_ms = pipeline_started_at.elapsed().as_millis() as u64;
                                info!(
                                    startup_ms,
                                    fps = STREAM_PUBLISH_TARGET_FPS,
                                    mime_type = %browser_mime_type,
                                    "stream publish produced first browser video frame"
                                );
                                let _ = event_tx.send(StreamPublishEvent::FirstFrame {
                                    startup_ms,
                                    fps: STREAM_PUBLISH_TARGET_FPS,
                                });
                            }
                            let timestamp_increment = timestamp_increments_thread
                                .lock()
                                .pop_front()
                                .unwrap_or(90_000 / STREAM_PUBLISH_TARGET_FPS);
                            if frame_tx
                                .send(StreamPublishFrame {
                                    access_unit,
                                    timestamp_increment,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(StreamPublishEvent::Error(format!(
                            "stream publish browser stdout read failed: {error}"
                        )));
                        break;
                    }
                }
            }

            if !stop_clone.load(Ordering::Relaxed) {
                for access_unit in drain_h264_access_units(&mut h264_buffer, true) {
                    let timestamp_increment = timestamp_increments_thread
                        .lock()
                        .pop_front()
                        .unwrap_or(90_000 / STREAM_PUBLISH_TARGET_FPS);
                    let _ = frame_tx.send(StreamPublishFrame {
                        access_unit,
                        timestamp_increment,
                    });
                }
            }

            *stdin_thread.lock() = None;
            terminate_stream_publish_child(&mut child, libc::SIGTERM);
            let wait_result = child.wait();
            if let Some(handle) = stderr_thread.take() {
                let _ = handle.join();
            }
            child_pid_thread.store(0, Ordering::SeqCst);
            paused_thread.store(false, Ordering::SeqCst);

            let stderr_summary = {
                let tail = stderr_tail.lock();
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(
                        " | stderr tail: {}",
                        tail.iter().cloned().collect::<Vec<_>>().join(" || ")
                    )
                }
            };

            if !stop_clone.load(Ordering::Relaxed) {
                match wait_result {
                    Ok(status) if status.success() => {
                        let _ = event_tx.send(StreamPublishEvent::Idle);
                    }
                    Ok(status) => {
                        let _ = event_tx.send(StreamPublishEvent::Error(format!(
                            "stream publish browser pipeline exited with status {status}{stderr_summary}"
                        )));
                    }
                    Err(error) => {
                        let _ = event_tx.send(StreamPublishEvent::Error(format!(
                            "stream publish browser pipeline wait failed: {error}{stderr_summary}"
                        )));
                    }
                }
            }
        });

        Self {
            stop,
            paused,
            child_pid,
            thread: Some(thread),
            mode: StreamPublishPlayerMode::BrowserFrames {
                mime_type,
                stdin,
                timestamp_increments,
                last_captured_at_ms,
            },
        }
    }

    pub(crate) fn push_browser_frame(
        &self,
        mime_type: &str,
        frame_bytes: &[u8],
        captured_at_ms: u64,
    ) -> io::Result<()> {
        let StreamPublishPlayerMode::BrowserFrames {
            mime_type: active_mime_type,
            stdin,
            timestamp_increments,
            last_captured_at_ms,
        } = &self.mode
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stream publish player is not a browser frame source",
            ));
        };

        if active_mime_type != mime_type {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stream publish browser frame mime type mismatch",
            ));
        }
        if frame_bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stream publish browser frame was empty",
            ));
        }
        if !self.is_alive() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream publish browser pipeline is not running",
            ));
        }

        let timestamp_increment =
            compute_browser_frame_timestamp_increment(last_captured_at_ms, captured_at_ms);
        timestamp_increments.lock().push_back(timestamp_increment);

        let mut guard = stdin.lock();
        let Some(writer) = guard.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream publish browser pipeline stdin unavailable",
            ));
        };
        if let Err(error) = writer.write_all(frame_bytes) {
            let _ = timestamp_increments.lock().pop_back();
            return Err(error);
        }
        writer.flush()
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.child_pid.load(Ordering::SeqCst) != 0
    }

    pub(crate) fn pause(&self) -> bool {
        if self.paused.load(Ordering::SeqCst) {
            return self.is_alive();
        }
        let pid = self.child_pid.load(Ordering::SeqCst);
        if pid == 0 {
            return false;
        }
        match kill_stream_publish_process_group(pid, libc::SIGSTOP) {
            Ok(()) => {
                self.paused.store(true, Ordering::SeqCst);
                true
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    warn!(pid, error = %error, "failed to pause stream publish process group");
                }
                false
            }
        }
    }

    pub(crate) fn resume(&self) -> bool {
        if !self.paused.load(Ordering::SeqCst) {
            return self.is_alive();
        }
        let pid = self.child_pid.load(Ordering::SeqCst);
        if pid == 0 {
            return false;
        }
        match kill_stream_publish_process_group(pid, libc::SIGCONT) {
            Ok(()) => {
                self.paused.store(false, Ordering::SeqCst);
                true
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    warn!(
                        pid,
                        error = %error,
                        "failed to resume stream publish process group"
                    );
                }
                false
            }
        }
    }

    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let StreamPublishPlayerMode::BrowserFrames { stdin, .. } = &self.mode {
            *stdin.lock() = None;
        }
        if let Some(handle) = self.thread.take() {
            let pid = self.child_pid.load(Ordering::SeqCst);
            if let Err(error) = kill_stream_publish_process_group(pid, libc::SIGTERM) {
                if error.kind() != io::ErrorKind::NotFound {
                    warn!(
                        pid,
                        error = %error,
                        "failed to stop stream publish process group"
                    );
                }
            }
            let _ = handle.join();
        }
    }
}

impl AppState {
    fn clear_stream_publish_runtime_buffers(&self) {
        while self.stream_publish_frame_rx.try_recv().is_ok() {}
        while self.stream_publish_event_rx.try_recv().is_ok() {}
    }

    pub(crate) fn stop_stream_publish_runtime(&mut self, reason: &str) {
        if let Some(conn) = self.stream_publish_conn.as_ref() {
            if let Err(error) = conn.set_stream_publish_speaking(false) {
                warn!(reason = reason, error = %error, "failed to disable stream publish speaking");
            }
            if let Err(error) = conn.set_stream_publish_video_active(false) {
                warn!(reason = reason, error = %error, "failed to disable stream publish video state");
            }
        }
        self.stream_publish.stop_player();
        self.clear_stream_publish_runtime_buffers();
        self.stream_publish.active = false;
        self.stream_publish.paused = false;
        self.stream_publish.active_source = None;
    }

    pub(crate) fn maybe_start_stream_publish_pipeline(&mut self) {
        if self.stream_publish_conn.is_none()
            || self.stream_publish.active
            || self.stream_publish.paused
        {
            return;
        }
        let Some(source) = self.stream_publish.pending_source.clone() else {
            return;
        };

        self.stop_stream_publish_runtime("restart_before_publish_start");
        self.clear_stream_publish_runtime_buffers();

        self.stream_publish.player = Some(match &source {
            StreamPublishSource::Url {
                url,
                resolved_direct_url,
            } => StreamPublishPlayer::start_url(
                url,
                self.stream_publish_frame_tx.clone(),
                self.stream_publish_event_tx.clone(),
                *resolved_direct_url,
            ),
            StreamPublishSource::BrowserFrames { mime_type } => {
                StreamPublishPlayer::start_browser_frames(
                    mime_type,
                    self.stream_publish_frame_tx.clone(),
                    self.stream_publish_event_tx.clone(),
                )
            }
        });
        self.stream_publish.active = true;
        self.stream_publish.paused = false;
        self.stream_publish.active_source = Some(source.clone());
        self.stream_publish.clear_pending_start();

        if let Some(conn) = self.stream_publish_conn.as_ref() {
            if let Err(error) = conn.set_stream_publish_video_active(true) {
                warn!(error = %error, "failed to announce active stream publish video state");
            }
            if let Err(error) = conn.set_stream_publish_speaking(true) {
                warn!(error = %error, "failed to enable stream publish speaking state");
            }
        }
        self.emit_transport_state(TransportRole::StreamPublish, "playing", None);
        match source {
            StreamPublishSource::Url {
                url,
                resolved_direct_url,
            } => {
                info!(
                    url = %url,
                    resolved_direct_url,
                    "started stream publish pipeline"
                );
            }
            StreamPublishSource::BrowserFrames { mime_type } => {
                info!(mime_type = %mime_type, "started browser stream publish pipeline");
            }
        }
    }

    pub(crate) fn handle_stream_publish_command(&mut self, msg: StreamPublishCommand) {
        match msg {
            StreamPublishCommand::Play {
                url,
                resolved_direct_url,
            } => {
                let normalized_url = url.trim().to_string();
                if normalized_url.is_empty() {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "failed",
                        Some("stream_publish_play_missing_url"),
                    );
                    return;
                }
                let source = StreamPublishSource::Url {
                    url: normalized_url,
                    resolved_direct_url,
                };
                if self.stream_publish.active
                    && !self.stream_publish.paused
                    && self.stream_publish.active_source.as_ref() == Some(&source)
                {
                    self.emit_transport_state(TransportRole::StreamPublish, "playing", None);
                    return;
                }
                if self.stream_publish.active_source.as_ref() != Some(&source) {
                    self.stop_stream_publish_runtime("stream_publish_source_switch");
                }
                self.stream_publish.queue_pending_start(source);
                if self.stream_publish_conn.is_some() {
                    self.maybe_start_stream_publish_pipeline();
                } else {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "waiting_for_transport",
                        None,
                    );
                }
            }
            StreamPublishCommand::BrowserStart { mime_type } => {
                let Some(normalized_mime_type) =
                    normalize_browser_frame_mime_type(&mime_type).map(str::to_string)
                else {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "failed",
                        Some("stream_publish_browser_start_unsupported_mime_type"),
                    );
                    return;
                };
                let source = StreamPublishSource::BrowserFrames {
                    mime_type: normalized_mime_type,
                };
                if self.stream_publish.active
                    && !self.stream_publish.paused
                    && self.stream_publish.active_source.as_ref() == Some(&source)
                {
                    self.emit_transport_state(TransportRole::StreamPublish, "playing", None);
                    return;
                }
                if self.stream_publish.active_source.as_ref() != Some(&source) {
                    self.stop_stream_publish_runtime("stream_publish_source_switch");
                }
                self.stream_publish.queue_pending_start(source);
                if self.stream_publish_conn.is_some() {
                    self.maybe_start_stream_publish_pipeline();
                } else {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "waiting_for_transport",
                        None,
                    );
                }
            }
            StreamPublishCommand::BrowserFrame {
                mime_type,
                frame_base64,
                captured_at_ms,
            } => {
                let Some(normalized_mime_type) =
                    normalize_browser_frame_mime_type(&mime_type).map(str::to_string)
                else {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "failed",
                        Some("stream_publish_browser_frame_unsupported_mime_type"),
                    );
                    return;
                };
                let frame_bytes = match decode_stream_publish_browser_frame(&frame_base64) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.emit_transport_state(
                            TransportRole::StreamPublish,
                            "failed",
                            Some(&error),
                        );
                        return;
                    }
                };

                if self.stream_publish_conn.is_some()
                    && !self.stream_publish.active
                    && matches!(
                        self.stream_publish.pending_source.as_ref(),
                        Some(StreamPublishSource::BrowserFrames { mime_type })
                            if mime_type == &normalized_mime_type
                    )
                {
                    self.maybe_start_stream_publish_pipeline();
                }

                let Some(player) = self.stream_publish.player.as_ref() else {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "waiting_for_transport",
                        Some("stream_publish_browser_source_not_started"),
                    );
                    return;
                };
                if !matches!(
                    self.stream_publish.active_source.as_ref(),
                    Some(StreamPublishSource::BrowserFrames { mime_type })
                        if mime_type == &normalized_mime_type
                ) {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "failed",
                        Some("stream_publish_browser_frame_source_mismatch"),
                    );
                    return;
                }
                if let Err(error) =
                    player.push_browser_frame(&normalized_mime_type, &frame_bytes, captured_at_ms)
                {
                    self.emit_transport_state(
                        TransportRole::StreamPublish,
                        "failed",
                        Some(&format!("stream_publish_browser_frame_write_failed: {error}")),
                    );
                }
            }
            StreamPublishCommand::Stop => {
                self.stop_stream_publish_runtime("stream_publish_stop");
                self.stream_publish.clear_pending_start();
                self.emit_transport_state(
                    TransportRole::StreamPublish,
                    "ready",
                    Some("stream_publish_stopped"),
                );
            }
            StreamPublishCommand::Pause => {
                self.stream_publish.paused = true;
                if let Some(player) = self.stream_publish.player.as_ref() {
                    let _ = player.pause();
                }
                if let Some(conn) = self.stream_publish_conn.as_ref() {
                    let _ = conn.set_stream_publish_speaking(false);
                    let _ = conn.set_stream_publish_video_active(false);
                }
                self.emit_transport_state(TransportRole::StreamPublish, "paused", None);
            }
            StreamPublishCommand::Resume => {
                self.stream_publish.paused = false;
                if let Some(player) = self.stream_publish.player.as_ref() {
                    if player.resume() {
                        if let Some(conn) = self.stream_publish_conn.as_ref() {
                            let _ = conn.set_stream_publish_video_active(true);
                            let _ = conn.set_stream_publish_speaking(true);
                        }
                        self.emit_transport_state(TransportRole::StreamPublish, "playing", None);
                        return;
                    }
                }
                if let Some(active_source) = self.stream_publish.active_source.clone() {
                    self.stream_publish.queue_pending_start(active_source);
                    self.stream_publish.active = false;
                    self.stream_publish.active_source = None;
                }
                self.maybe_start_stream_publish_pipeline();
            }
        }
    }

    fn handle_stream_publish_event(&mut self, event: StreamPublishEvent) {
        match event {
            StreamPublishEvent::Idle => {
                self.stop_stream_publish_runtime("stream_publish_idle");
                self.emit_transport_state(
                    TransportRole::StreamPublish,
                    "ready",
                    Some("stream_publish_idle"),
                );
            }
            StreamPublishEvent::Error(message) => {
                self.stop_stream_publish_runtime("stream_publish_error");
                self.emit_transport_state(TransportRole::StreamPublish, "failed", Some(&message));
            }
            StreamPublishEvent::FirstFrame { startup_ms, fps } => {
                info!(startup_ms, fps, "stream publish first frame observed");
            }
        }
    }

    pub(crate) fn drain_stream_publish_runtime_events(&mut self) {
        while let Ok(event) = self.stream_publish_event_rx.try_recv() {
            self.handle_stream_publish_event(event);
        }
    }

    pub(crate) async fn send_pending_stream_publish_frame(&mut self) {
        if !self.stream_publish.active || self.stream_publish.paused {
            return;
        }
        let Ok(frame) = self.stream_publish_frame_rx.try_recv() else {
            return;
        };

        let encrypted_frame = {
            let mut guard = self.stream_publish_dave.lock();
            match *guard {
                Some(ref mut dave_manager) if dave_manager.is_ready() => dave_manager
                    .encrypt_video(&frame.access_unit)
                    .unwrap_or_else(|error| {
                        tracing::debug!("stream publish DAVE encrypt fallback: {}", error);
                        frame.access_unit.clone()
                    }),
                _ => frame.access_unit.clone(),
            }
        };

        if let Some(conn) = self.stream_publish_conn.as_ref() {
            if let Err(error) = conn
                .send_h264_frame(&encrypted_frame, frame.timestamp_increment)
                .await
            {
                warn!(error = %error, "failed to send stream publish video frame");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        STREAM_PUBLISH_TARGET_FPS, build_stream_publish_browser_pipeline_command,
        build_stream_publish_pipeline_command, drain_h264_access_units,
    };

    #[test]
    fn build_stream_publish_pipeline_command_uses_direct_ffmpeg_for_direct_urls() {
        let command =
            build_stream_publish_pipeline_command("https://cdn.example.com/video.mp4", true);
        assert!(command.contains("ffmpeg"));
        assert!(!command.contains("yt-dlp"));
        assert!(command.contains(&format!("fps={STREAM_PUBLISH_TARGET_FPS}")));
    }

    #[test]
    fn build_stream_publish_pipeline_command_uses_ytdlp_for_indirect_urls() {
        let command =
            build_stream_publish_pipeline_command("https://www.youtube.com/watch?v=abc123", false);
        assert!(command.contains("yt-dlp"));
        assert!(command.contains("h264_metadata=aud=insert"));
    }

    #[test]
    fn build_stream_publish_browser_pipeline_command_uses_image2pipe_png_input() {
        let command = build_stream_publish_browser_pipeline_command("image/png");
        assert!(command.contains("-f image2pipe"));
        assert!(command.contains("-codec:v png"));
        assert!(command.contains("h264_metadata=aud=insert"));
    }

    #[test]
    fn drain_h264_access_units_splits_on_aud_boundaries() {
        let mut buffer = vec![
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x01, 0x02, 0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0,
            1, 0x65, 0xaa,
        ];
        let frames = drain_h264_access_units(&mut buffer, false);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].starts_with(&[0, 0, 0, 1, 0x09]));
        assert!(buffer.starts_with(&[0, 0, 0, 1, 0x09]));
    }
}

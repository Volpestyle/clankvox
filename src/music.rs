use std::collections::VecDeque;
use std::io::{self, BufRead};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crossbeam_channel as crossbeam;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{info, warn};

use crate::audio_pipeline::{AudioSendState, clear_audio_send_buffer};

const MUSIC_PIPELINE_STDERR_TAIL_LINES: usize = 24;

#[derive(Debug)]
pub(crate) enum MusicEvent {
    Idle,
    Error(String),
    FirstPcm {
        startup_ms: u64,
        resolved_direct_url: bool,
    },
}

pub(crate) fn drain_music_pcm_queue(music_pcm_rx: &crossbeam::Receiver<Vec<i16>>) {
    while music_pcm_rx.try_recv().is_ok() {}
}

pub(crate) fn is_music_output_drained(
    music_pcm_rx: &crossbeam::Receiver<Vec<i16>>,
    audio_send_state: &Arc<Mutex<Option<AudioSendState>>>,
) -> bool {
    if !music_pcm_rx.is_empty() {
        return false;
    }
    let guard = audio_send_state.lock();
    guard
        .as_ref()
        .is_none_or(|state| state.music_buffer_samples() == 0)
}

#[derive(Clone, Copy)]
pub(crate) struct MusicPipelineRequest<'a> {
    pub(crate) url: &'a str,
    pub(crate) resolved_direct_url: bool,
    pub(crate) clear_output_buffers: bool,
}

pub(crate) struct MusicPipelineContext<'a> {
    pub(crate) music_player: &'a mut Option<MusicPlayer>,
    pub(crate) music_pcm_rx: &'a crossbeam::Receiver<Vec<i16>>,
    pub(crate) music_pcm_tx: &'a crossbeam::Sender<Vec<i16>>,
    pub(crate) music_event_tx: &'a mpsc::Sender<MusicEvent>,
    pub(crate) audio_send_state: &'a Arc<Mutex<Option<AudioSendState>>>,
}

pub(crate) fn start_music_pipeline(
    request: MusicPipelineRequest<'_>,
    context: MusicPipelineContext<'_>,
) {
    let MusicPipelineRequest {
        url,
        resolved_direct_url,
        clear_output_buffers,
    } = request;
    let MusicPipelineContext {
        music_player,
        music_pcm_rx,
        music_pcm_tx,
        music_event_tx,
        audio_send_state,
    } = context;

    if let Some(player) = music_player {
        player.stop();
    }
    *music_player = None;
    drain_music_pcm_queue(music_pcm_rx);
    if clear_output_buffers {
        clear_audio_send_buffer(audio_send_state);
    }
    *music_player = Some(MusicPlayer::start(
        url,
        music_pcm_tx.clone(),
        music_event_tx.clone(),
        resolved_direct_url,
    ));
}

pub(crate) struct MusicPlayer {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    child_pid: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Send a signal to the entire process group of a music pipeline child.
///
/// # Safety
///
/// This calls `libc::killpg` which is inherently unsafe because it sends a
/// signal to an entire process group. The safety invariants are:
///
/// - `pid` must be a valid, non-zero PID obtained from `std::process::Child::id()`.
/// - The child was spawned with `.process_group(0)` (see `MusicPlayer::start`),
///   which places the shell pipeline (sh + yt-dlp + ffmpeg) in its own process
///   group whose PGID equals the child PID.
/// - Callers only use process-group signals that are valid for the music
///   pipeline lifecycle: `SIGTERM` for shutdown plus `SIGSTOP` / `SIGCONT`
///   for in-place pause and resume. We never send `SIGKILL`.
/// - The guard `if pid == 0 { return; }` prevents signaling PID 0, which
///   would signal the calling process's own group.
fn kill_music_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    if pid == 0 {
        return Ok(());
    }
    // SAFETY: All invariants documented above are upheld by the caller.
    // `pid` originates from `Child::id()`, the child uses `.process_group(0)`,
    // and we guard against pid==0.
    #[allow(unsafe_code, clippy::cast_possible_wrap)]
    let rc = unsafe { libc::killpg(pid as libc::pid_t, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn terminate_music_child(child: &mut std::process::Child, signal: libc::c_int) {
    if let Err(error) = kill_music_process_group(child.id(), signal) {
        if error.kind() != io::ErrorKind::NotFound {
            warn!(pid = child.id(), error = %error, "failed to signal music process group");
        }
    }
}

impl MusicPlayer {
    #[allow(clippy::too_many_lines)]
    fn start(
        url: &str,
        pcm_tx: crossbeam::Sender<Vec<i16>>,
        music_event_tx: mpsc::Sender<MusicEvent>,
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
            let pipeline_command = build_music_pipeline_command(&url, resolved_direct_url);
            let pipeline_started_at = time::Instant::now();
            let child = std::process::Command::new("sh")
                .process_group(0)
                .args(["-c", &pipeline_command])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = music_event_tx.blocking_send(MusicEvent::Error(format!(
                        "yt-dlp/ffmpeg spawn failed: {e}"
                    )));
                    return;
                }
            };
            child_pid_thread.store(child.id(), Ordering::SeqCst);

            let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
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
                        if tail.len() >= MUSIC_PIPELINE_STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                })
            });

            let Some(stdout) = child.stdout.take() else {
                let _ = music_event_tx.blocking_send(MusicEvent::Error(
                    "music pipeline missing stdout".to_string(),
                ));
                terminate_music_child(&mut child, libc::SIGTERM);
                let _ = child.wait();
                if let Some(handle) = stderr_thread.take() {
                    let _ = handle.join();
                }
                child_pid_thread.store(0, Ordering::SeqCst);
                return;
            };

            let mut reader = io::BufReader::with_capacity(48000 * 2, stdout);
            let mut chunk = vec![0u8; 960 * 2];
            let mut first_pcm_reported = false;

            loop {
                if stop_clone.load(Ordering::Relaxed) {
                    break;
                }
                match io::Read::read_exact(&mut reader, &mut chunk) {
                    Ok(()) => {
                        if !first_pcm_reported {
                            first_pcm_reported = true;
                            let startup_ms = pipeline_started_at.elapsed().as_millis() as u64;
                            info!(
                                "music pipeline first pcm startup_ms={} direct={}",
                                startup_ms, resolved_direct_url
                            );
                            let _ = music_event_tx.blocking_send(MusicEvent::FirstPcm {
                                startup_ms,
                                resolved_direct_url,
                            });
                        }
                        let mut samples = Vec::with_capacity(960);
                        for i in 0..960 {
                            samples.push(i16::from_le_bytes([chunk[i * 2], chunk[i * 2 + 1]]));
                        }
                        if pcm_tx.send(samples).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            terminate_music_child(&mut child, libc::SIGTERM);
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
                        let _ = music_event_tx.blocking_send(MusicEvent::Idle);
                    }
                    Ok(status) => {
                        let _ = music_event_tx.blocking_send(MusicEvent::Error(format!(
                            "music pipeline exited with status {status}{stderr_summary}"
                        )));
                    }
                    Err(error) => {
                        let _ = music_event_tx.blocking_send(MusicEvent::Error(format!(
                            "music pipeline wait failed: {error}{stderr_summary}"
                        )));
                    }
                }
            }
        });

        MusicPlayer {
            stop,
            paused,
            child_pid,
            thread: Some(thread),
        }
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
        match kill_music_process_group(pid, libc::SIGSTOP) {
            Ok(()) => {
                self.paused.store(true, Ordering::SeqCst);
                true
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    warn!(pid, error = %error, "failed to pause music process group");
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
            self.paused.store(false, Ordering::SeqCst);
            return false;
        }
        match kill_music_process_group(pid, libc::SIGCONT) {
            Ok(()) => {
                self.paused.store(false, Ordering::SeqCst);
                true
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    warn!(pid, error = %error, "failed to resume music process group");
                }
                false
            }
        }
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let was_paused = self.paused.swap(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            if !thread.is_finished() {
                let pid = self.child_pid.load(Ordering::SeqCst);
                // A SIGSTOP'd process won't handle SIGTERM until continued.
                if was_paused {
                    let _ = kill_music_process_group(pid, libc::SIGCONT);
                }
                if let Err(error) = kill_music_process_group(pid, libc::SIGTERM) {
                    if error.kind() != io::ErrorKind::NotFound {
                        warn!(pid, error = %error, "failed to stop music process group");
                    }
                }
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                std::thread::spawn(move || {
                    let _ = thread.join();
                });
            }
        }
    }
}

impl Drop for MusicPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // Music state machine flags are inherently boolean.
pub(crate) struct MusicState {
    pub(crate) player: Option<MusicPlayer>,
    pub(crate) active: bool,
    pub(crate) paused: bool,
    pub(crate) finishing: bool,
    pub(crate) active_url: Option<String>,
    pub(crate) active_resolved_direct_url: bool,
    pub(crate) pending_url: Option<String>,
    pub(crate) pending_received_at: Option<time::Instant>,
    pub(crate) pending_audio_seen: bool,
    pub(crate) pending_last_audio_at: Option<time::Instant>,
    pub(crate) pending_waiting_for_drain: bool,
    pub(crate) pending_drain_started_at: Option<time::Instant>,
    pub(crate) pending_first_pcm_at: Option<time::Instant>,
    pub(crate) pending_resolved_direct_url: bool,
    pub(crate) pending_stop: bool,
}

impl MusicState {
    pub(crate) fn stop_player(&mut self) {
        if let Some(ref mut player) = self.player {
            player.stop();
        }
        self.player = None;
    }

    pub(crate) fn clear_pending_start(&mut self) {
        self.pending_url = None;
        self.pending_received_at = None;
        self.pending_audio_seen = false;
        self.pending_last_audio_at = None;
        self.pending_waiting_for_drain = false;
        self.pending_drain_started_at = None;
        self.pending_first_pcm_at = None;
        self.pending_resolved_direct_url = false;
    }

    pub(crate) fn reset(&mut self) {
        self.stop_player();
        self.active = false;
        self.paused = false;
        self.finishing = false;
        self.active_url = None;
        self.active_resolved_direct_url = false;
        self.pending_stop = false;
        self.clear_pending_start();
    }

    pub(crate) fn queue_pending_start(&mut self, url: String, resolved_direct_url: bool) {
        self.stop_player();
        self.active = false;
        self.paused = false;
        self.finishing = false;
        self.pending_stop = false;
        self.active_url = Some(url.clone());
        self.active_resolved_direct_url = resolved_direct_url;
        self.pending_url = Some(url);
        self.pending_received_at = Some(time::Instant::now());
        self.pending_audio_seen = false;
        self.pending_last_audio_at = None;
        self.pending_waiting_for_drain = false;
        self.pending_drain_started_at = None;
        self.pending_first_pcm_at = None;
        self.pending_resolved_direct_url = resolved_direct_url;
    }
}

pub(crate) fn build_music_pipeline_command(url: &str, resolved_direct_url: bool) -> String {
    let quoted_url = url.replace('\'', "'\\''");
    if resolved_direct_url {
        format!("ffmpeg -nostdin -loglevel error -i '{quoted_url}' -f s16le -ar 48000 -ac 1 pipe:1")
    } else {
        format!(
            "yt-dlp --no-warnings --quiet --no-playlist --extractor-args 'youtube:player_client=android' -f bestaudio/best -o - '{quoted_url}' | ffmpeg -nostdin -loglevel error -i pipe:0 -f s16le -ar 48000 -ac 1 pipe:1"
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossbeam_channel as crossbeam;
    use parking_lot::Mutex;

    use super::{build_music_pipeline_command, is_music_output_drained};
    use crate::audio_pipeline::AudioSendState;

    #[test]
    fn music_output_not_drained_while_pcm_queue_has_chunks() {
        let (music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));

        music_pcm_tx.send(vec![0; 960]).expect("queue chunk");

        assert!(!is_music_output_drained(&music_pcm_rx, &audio_send_state));
    }

    #[test]
    fn music_output_not_drained_while_mixer_buffer_has_music() {
        let (_music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));
        {
            let mut guard = audio_send_state.lock();
            let state = guard.as_mut().expect("state");
            state.push_music_pcm(vec![0; 960]);
        }

        assert!(!is_music_output_drained(&music_pcm_rx, &audio_send_state));
    }

    #[test]
    fn music_output_drained_when_queue_and_mixer_are_empty() {
        let (_music_pcm_tx, music_pcm_rx) = crossbeam::bounded::<Vec<i16>>(4);
        let audio_send_state = Arc::new(Mutex::new(Some(
            AudioSendState::new().expect("audio state"),
        )));

        assert!(is_music_output_drained(&music_pcm_rx, &audio_send_state));
    }

    #[test]
    fn direct_music_pipeline_command_skips_ytdlp() {
        let command = build_music_pipeline_command("https://cdn.example.com/audio.m4a", true);
        assert!(command.starts_with("ffmpeg "));
        assert!(!command.contains("yt-dlp"));
    }

    #[test]
    fn unresolved_music_pipeline_command_uses_ytdlp() {
        let command = build_music_pipeline_command("https://www.youtube.com/watch?v=abc123", false);
        assert!(command.contains("yt-dlp"));
        assert!(command.contains("| ffmpeg "));
    }
}

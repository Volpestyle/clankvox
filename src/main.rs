mod app_state;
mod audio_pipeline;
mod capture;
mod capture_supervisor;
mod connection_supervisor;
mod dave;
mod ipc;
mod ipc_protocol;
mod ipc_router;
mod music;
mod playback_supervisor;
mod video;
mod voice_conn;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel as crossbeam;
use parking_lot::Mutex;
use tokio::time;

use crate::app_state::AppState;
use crate::audio_pipeline::AudioSendState;
use crate::dave::DaveManager;
use crate::ipc::{spawn_ipc_reader, spawn_ipc_writer};
use crate::music::MusicEvent;
use crate::voice_conn::VoiceEvent;

const MUSIC_PCM_QUEUE_CAPACITY_CHUNKS: usize = 100; // ~2s of 20ms PCM chunks

async fn reconnect_sleep(deadline: Option<time::Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,davey=warn,davey::cryptor::frame_processors=off",
                )
            }),
        )
        .with_writer(io::stderr)
        .init();

    spawn_ipc_writer();

    let audio_debug = std::env::var("AUDIO_DEBUG").is_ok();
    let mut inbound_ipc = spawn_ipc_reader(audio_debug);

    tracing::info!("Voice subprocess started, waiting for IPC messages");

    let dave: Arc<Mutex<Option<DaveManager>>> = Arc::new(Mutex::new(None));
    let (voice_event_tx, mut voice_event_rx) = tokio::sync::mpsc::channel::<VoiceEvent>(256);
    let audio_send_state = Arc::new(Mutex::new(None::<AudioSendState>));

    let mut send_interval = time::interval(Duration::from_millis(20));
    send_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    send_interval.tick().await;

    let (music_pcm_tx, music_pcm_rx) =
        crossbeam::bounded::<Vec<i16>>(MUSIC_PCM_QUEUE_CAPACITY_CHUNKS);
    let (music_event_tx, mut music_event_rx) = tokio::sync::mpsc::channel::<MusicEvent>(32);

    let mut state = AppState::new(
        dave,
        voice_event_tx,
        audio_send_state,
        music_pcm_tx,
        music_pcm_rx,
        music_event_tx,
    );

    loop {
        tokio::select! {
            msg = inbound_ipc.recv() => {
                let Some(msg) = msg else {
                    break;
                };

                if state.route_ipc_message(msg).await {
                    break;
                }
            }

            Some(event) = voice_event_rx.recv() => {
                state.handle_voice_event(event);
            }

            Some(event) = music_event_rx.recv() => {
                state.handle_music_event(event);
            }

            () = reconnect_sleep(state.reconnect_deadline) => {
                state.handle_reconnect_timer().await;
            }

            _ = send_interval.tick() => {
                state.on_audio_tick().await;
            }
        }
    }

    tracing::info!("Shutting down");
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;

    use super::reconnect_sleep;

    #[test]
    fn reconnect_sleep_without_deadline_is_pending() {
        let future = reconnect_sleep(None);
        tokio::pin!(future);
        assert!(future.now_or_never().is_none());
    }
}

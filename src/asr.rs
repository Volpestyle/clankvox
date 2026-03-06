use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::error;

use crate::ipc::{send_msg, OutMsg};

pub(crate) enum AsrCommand {
    Audio(Vec<u8>),
    Commit,
    Clear,
}

fn insert_optional_transcription_field(
    transcription_cfg: &mut serde_json::Value,
    key: &str,
    value: Option<String>,
) {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return;
    };

    if let Some(object) = transcription_cfg.as_object_mut() {
        object.insert(key.to_owned(), json!(value));
    } else {
        error!(
            field = key,
            "transcription config unexpectedly stopped being an object"
        );
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_asr_client(
    user_id: String,
    api_key: String,
    model: String,
    language: Option<String>,
    prompt: Option<String>,
    mut rx: mpsc::UnboundedReceiver<AsrCommand>,
) -> anyhow::Result<()> {
    let url = "wss://api.openai.com/v1/realtime?intent=transcription";
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {api_key}").parse()?);
    request
        .headers_mut()
        .insert("OpenAI-Beta", "realtime=v1".parse()?);

    let (ws_stream, _) = connect_async(request).await?;
    let (mut write, mut read) = ws_stream.split();

    let mut transcription_cfg = json!({
        "model": model,
    });
    insert_optional_transcription_field(&mut transcription_cfg, "language", language);
    insert_optional_transcription_field(&mut transcription_cfg, "prompt", prompt);

    let setup_msg = json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": {
                        "type": "audio/pcm",
                        "rate": 24000
                    },
                    "noise_reduction": {
                        "type": "near_field"
                    },
                    "turn_detection": {
                        "type": "server_vad",
                        "threshold": 0.55,
                        "prefix_padding_ms": 240,
                        "silence_duration_ms": 450,
                        "create_response": false,
                        "interrupt_response": false
                    },
                    "transcription": transcription_cfg
                }
            }
        },
        "include": ["item.input_audio_transcription.logprobs"]
    });

    write.send(Message::Text(setup_msg.to_string())).await?;

    loop {
        tokio::select! {
            Some(cmd) = rx.recv() => {
                match cmd {
                    AsrCommand::Audio(pcm) => {
                        let b64 = STANDARD.encode(&pcm);
                        let append_msg = json!({
                            "type": "input_audio_buffer.append",
                            "audio": b64
                        });
                        if write.send(Message::Text(append_msg.to_string())).await.is_err() {
                            break;
                        }
                    }
                    AsrCommand::Commit => {
                        let commit_msg = json!({
                            "type": "input_audio_buffer.commit"
                        });
                        let _ = write.send(Message::Text(commit_msg.to_string())).await;
                    }
                    AsrCommand::Clear => {
                        let clear_msg = json!({
                            "type": "input_audio_buffer.clear"
                        });
                        let _ = write.send(Message::Text(clear_msg.to_string())).await;
                    }
                }
            }
            Some(msg_res) = read.next() => {
                let Ok(msg) = msg_res else { break };
                if let Message::Text(text) = msg {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if msg_type == "conversation.item.input_audio_transcription.delta" {
                            if let Some(delta) = value.get("delta").and_then(|d| d.as_str()) {
                                send_msg(&OutMsg::AsrTranscript {
                                    user_id: user_id.clone(),
                                    text: delta.to_string(),
                                    is_final: false,
                                });
                            }
                        } else if msg_type == "conversation.item.input_audio_transcription.completed" {
                            if let Some(transcript) = value.get("transcript").and_then(|t| t.as_str()) {
                                send_msg(&OutMsg::AsrTranscript {
                                    user_id: user_id.clone(),
                                    text: transcript.to_string(),
                                    is_final: true,
                                });
                            }
                        } else if msg_type == "error" {
                            if let Some(err) = value.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
                                error!("OpenAI ASR error for user {}: {}", user_id, err);
                            }
                        }
                    }
                }
            }
            else => break,
        }
    }

    Ok(())
}

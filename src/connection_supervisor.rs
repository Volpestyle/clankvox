use crate::app_state::{AppState, TryConnectOutcome};
use crate::ipc::send_error;
use crate::ipc_protocol::ConnectionCommand;
use crate::voice_conn::{VoiceConnection, VoiceConnectionParams};

impl AppState {
    pub(crate) fn apply_connect_outcome(&mut self, outcome: TryConnectOutcome, failure_reason: &str) {
        match outcome {
            TryConnectOutcome::Connected => self.reset_reconnect(),
            TryConnectOutcome::Failed => self.schedule_reconnect(failure_reason),
            TryConnectOutcome::AlreadyConnected | TryConnectOutcome::MissingData => {}
        }
    }

    pub(crate) async fn try_connect(&mut self) -> TryConnectOutcome {
        if self.voice_conn.is_some() {
            return TryConnectOutcome::AlreadyConnected;
        }
        let Some(gid) = self.guild_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(cid) = self.channel_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(uid) = self.pending_conn.user_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(endpoint) = self.pending_conn.endpoint.as_deref() else {
            return TryConnectOutcome::MissingData;
        };
        let Some(session_id) = self.pending_conn.session_id.as_deref() else {
            return TryConnectOutcome::MissingData;
        };
        let Some(token) = self.pending_conn.token.as_deref() else {
            return TryConnectOutcome::MissingData;
        };

        tracing::info!(
            "Connecting to voice: endpoint={:?} guild={} channel={} user={}",
            self.pending_conn.endpoint, gid, cid, uid
        );

        match VoiceConnection::connect(
            VoiceConnectionParams {
                endpoint,
                guild_id: gid,
                user_id: uid,
                session_id,
                token,
                channel_id: cid,
            },
            self.voice_event_tx.clone(),
            self.dave.clone(),
        )
        .await
        {
            Ok(conn) => {
                self.voice_conn = Some(conn);
                TryConnectOutcome::Connected
            }
            Err(error) => {
                tracing::error!("Voice connection failed: {error}");
                send_error(
                    crate::ipc::ErrorCode::VoiceConnectFailed,
                    format!("Voice connect failed: {error}"),
                );
                TryConnectOutcome::Failed
            }
        }
    }

    pub(crate) async fn handle_reconnect_timer(&mut self) {
        self.reconnect_deadline = None;
        let outcome = self.try_connect().await;
        match outcome {
            TryConnectOutcome::Connected | TryConnectOutcome::AlreadyConnected => {
                self.reconnect_attempt = 0;
            }
            TryConnectOutcome::Failed | TryConnectOutcome::MissingData => {
                self.schedule_reconnect("reconnect_retry");
            }
        }
    }

    pub(crate) async fn handle_connection_command(&mut self, msg: ConnectionCommand) {
        match msg {
            ConnectionCommand::Join {
                guild_id,
                channel_id,
                self_mute,
            } => {
                let Ok(guild_id) = guild_id.parse::<u64>() else {
                    send_error(
                        crate::ipc::ErrorCode::InvalidRequest,
                        format!("join requires a numeric guild_id, got {guild_id:?}"),
                    );
                    return;
                };
                let Ok(channel_id) = channel_id.parse::<u64>() else {
                    send_error(
                        crate::ipc::ErrorCode::InvalidRequest,
                        format!("join requires a numeric channel_id, got {channel_id:?}"),
                    );
                    return;
                };
                self.guild_id = Some(guild_id);
                self.channel_id = Some(channel_id);
                self.self_mute = self_mute;
                self.reset_reconnect();

                crate::ipc::send_gateway_voice_state_update(guild_id, channel_id, self_mute);
                tracing::info!("Join requested guild={} channel={}, sent OP4", guild_id, channel_id);
            }
            ConnectionCommand::VoiceServer { data } => {
                let endpoint = data.endpoint.clone();
                let has_token = data.token.is_some();
                tracing::info!(
                    "IPC voice_server: endpoint={:?} token={} connected={}",
                    endpoint,
                    if has_token { "present" } else { "missing" },
                    self.voice_conn.is_some()
                );
                if let Some(ref endpoint) = endpoint {
                    self.pending_conn.endpoint = Some(endpoint.clone());
                }
                if let Some(token) = data.token.as_deref() {
                    self.pending_conn.token = Some(token.to_string());
                }
                let outcome = self.try_connect().await;
                self.apply_connect_outcome(outcome, "voice_server_connect_failed");
            }
            ConnectionCommand::VoiceState { data } => {
                let new_session_id = data.session_id.clone();
                let old_session_id = self.pending_conn.session_id.clone();
                let new_user_id = match data.user_id.as_deref() {
                    Some(user_id) => crate::app_state::parse_user_id_field(user_id, "voice_state"),
                    None => None,
                };
                tracing::info!(
                    "IPC voice_state: session_id={:?} prev_session_id={:?} channel_id={:?} user_id={:?} connected={}",
                    new_session_id,
                    old_session_id,
                    data.channel_id,
                    new_user_id,
                    self.voice_conn.is_some()
                );

                if let Some(ref session_id) = new_session_id {
                    if self.voice_conn.is_some() && old_session_id.as_deref() != Some(session_id.as_str()) {
                        tracing::warn!(
                            "Session ID changed while connected: {:?} -> {:?}, tearing down for reconnect",
                            old_session_id,
                            new_session_id
                        );
                        self.clear_voice_connection();
                    }
                    self.pending_conn.session_id = Some(session_id.clone());
                }
                if let Some(user_id) = new_user_id {
                    self.pending_conn.user_id = Some(user_id);
                    self.self_user_id = Some(user_id);
                }
                let outcome = self.try_connect().await;
                self.apply_connect_outcome(outcome, "voice_state_connect_failed");
            }
        }
    }
}

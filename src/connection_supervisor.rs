use crate::app_state::{AppState, TryConnectOutcome};
use crate::ipc::{ErrorCode, send_error};
use crate::ipc_protocol::ConnectionCommand;
use crate::voice_conn::{TransportRole, VoiceConnection, VoiceConnectionParams};

impl AppState {
    async fn maybe_try_connect(&mut self, failure_reason: &str, source: &str) {
        if self.reconnect_deadline.is_some() {
            tracing::info!(
                source = source,
                "Reconnect already scheduled; deferring immediate voice connect"
            );
            return;
        }

        let outcome = self.try_connect().await;
        self.apply_connect_outcome(outcome, failure_reason);
    }

    pub(crate) fn apply_connect_outcome(
        &mut self,
        outcome: TryConnectOutcome,
        failure_reason: &str,
    ) {
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
        let Some(guild_id) = self.guild_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(channel_id) = self.channel_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(user_id) = self.pending_conn.user_id else {
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
            endpoint = ?self.pending_conn.endpoint,
            guild_id,
            channel_id,
            user_id,
            "Connecting primary voice transport"
        );

        match VoiceConnection::connect(
            VoiceConnectionParams {
                endpoint,
                server_id: guild_id,
                user_id,
                session_id,
                token,
                dave_channel_id: channel_id,
                role: TransportRole::Voice,
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
                    ErrorCode::VoiceConnectFailed,
                    format!("Voice connect failed: {error}"),
                );
                TryConnectOutcome::Failed
            }
        }
    }

    async fn try_connect_stream_watch(&mut self) -> TryConnectOutcome {
        if self.stream_watch_conn.is_some() {
            return TryConnectOutcome::AlreadyConnected;
        }
        let Some(server_id) = self.stream_watch_pending_conn.server_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(dave_channel_id) = self.stream_watch_pending_conn.dave_channel_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(user_id) = self.stream_watch_pending_conn.user_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(endpoint) = self.stream_watch_pending_conn.endpoint.as_deref() else {
            return TryConnectOutcome::MissingData;
        };
        let Some(session_id) = self.stream_watch_pending_conn.session_id.as_deref() else {
            return TryConnectOutcome::MissingData;
        };
        let Some(token) = self.stream_watch_pending_conn.token.as_deref() else {
            return TryConnectOutcome::MissingData;
        };

        tracing::info!(
            endpoint = ?self.stream_watch_pending_conn.endpoint,
            server_id,
            dave_channel_id,
            user_id,
            "Connecting stream watch transport"
        );

        match VoiceConnection::connect(
            VoiceConnectionParams {
                endpoint,
                server_id,
                user_id,
                session_id,
                token,
                dave_channel_id,
                role: TransportRole::StreamWatch,
            },
            self.voice_event_tx.clone(),
            self.stream_watch_dave.clone(),
        )
        .await
        {
            Ok(conn) => {
                self.stream_watch_conn = Some(conn);
                TryConnectOutcome::Connected
            }
            Err(error) => {
                tracing::error!("Stream watch connection failed: {error}");
                send_error(
                    ErrorCode::StreamWatchConnectFailed,
                    format!("Stream watch connect failed: {error}"),
                );
                self.emit_transport_state(
                    TransportRole::StreamWatch,
                    "failed",
                    Some(&error.to_string()),
                );
                TryConnectOutcome::Failed
            }
        }
    }

    async fn try_connect_stream_publish(&mut self) -> TryConnectOutcome {
        if self.stream_publish_conn.is_some() {
            return TryConnectOutcome::AlreadyConnected;
        }
        let Some(server_id) = self.stream_publish_pending_conn.server_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(dave_channel_id) = self.stream_publish_pending_conn.dave_channel_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(user_id) = self.stream_publish_pending_conn.user_id else {
            return TryConnectOutcome::MissingData;
        };
        let Some(endpoint) = self.stream_publish_pending_conn.endpoint.as_deref() else {
            return TryConnectOutcome::MissingData;
        };
        let Some(session_id) = self.stream_publish_pending_conn.session_id.as_deref() else {
            return TryConnectOutcome::MissingData;
        };
        let Some(token) = self.stream_publish_pending_conn.token.as_deref() else {
            return TryConnectOutcome::MissingData;
        };

        tracing::info!(
            endpoint = ?self.stream_publish_pending_conn.endpoint,
            server_id,
            dave_channel_id,
            user_id,
            "Connecting stream publish transport"
        );

        match VoiceConnection::connect(
            VoiceConnectionParams {
                endpoint,
                server_id,
                user_id,
                session_id,
                token,
                dave_channel_id,
                role: TransportRole::StreamPublish,
            },
            self.voice_event_tx.clone(),
            self.stream_publish_dave.clone(),
        )
        .await
        {
            Ok(conn) => {
                self.stream_publish_conn = Some(conn);
                TryConnectOutcome::Connected
            }
            Err(error) => {
                tracing::error!("Stream publish connection failed: {error}");
                send_error(
                    ErrorCode::StreamPublishConnectFailed,
                    format!("Stream publish connect failed: {error}"),
                );
                self.emit_transport_state(
                    TransportRole::StreamPublish,
                    "failed",
                    Some(&error.to_string()),
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
                        ErrorCode::InvalidRequest,
                        format!("join requires a numeric guild_id, got {guild_id:?}"),
                    );
                    return;
                };
                let Ok(channel_id) = channel_id.parse::<u64>() else {
                    send_error(
                        ErrorCode::InvalidRequest,
                        format!("join requires a numeric channel_id, got {channel_id:?}"),
                    );
                    return;
                };
                self.guild_id = Some(guild_id);
                self.channel_id = Some(channel_id);
                self.self_mute = self_mute;
                self.reset_reconnect();

                crate::ipc::send_gateway_voice_state_update(guild_id, channel_id, self_mute);
                tracing::info!(
                    guild_id,
                    channel_id,
                    "Join requested; forwarded OP4 voice state update"
                );
            }
            ConnectionCommand::VoiceServer { data } => {
                let endpoint = data.endpoint.clone();
                let has_token = data.token.is_some();
                tracing::info!(
                    endpoint = ?endpoint,
                    has_token,
                    connected = self.voice_conn.is_some(),
                    "IPC voice_server received"
                );
                if let Some(ref endpoint) = endpoint {
                    self.pending_conn.endpoint = Some(endpoint.clone());
                }
                if let Some(token) = data.token.as_deref() {
                    self.pending_conn.token = Some(token.to_string());
                }
                self.maybe_try_connect("voice_server_connect_failed", "voice_server")
                    .await;
            }
            ConnectionCommand::VoiceState { data } => {
                let new_session_id = data.session_id.clone();
                let old_session_id = self.pending_conn.session_id.clone();
                let new_user_id = match data.user_id.as_deref() {
                    Some(user_id) => crate::app_state::parse_user_id_field(user_id, "voice_state"),
                    None => None,
                };
                tracing::info!(
                    session_id = ?new_session_id,
                    prev_session_id = ?old_session_id,
                    channel_id = ?data.channel_id,
                    user_id = ?new_user_id,
                    connected = self.voice_conn.is_some(),
                    stream_watch_connected = self.stream_watch_conn.is_some(),
                    "IPC voice_state received"
                );

                if let Some(ref session_id) = new_session_id {
                    if self.voice_conn.is_some()
                        && old_session_id.as_deref() != Some(session_id.as_str())
                    {
                        tracing::warn!(
                            previous = ?old_session_id,
                            current = ?new_session_id,
                            "Voice session id changed while connected; tearing down for reconnect"
                        );
                        self.clear_voice_connection();
                        self.clear_transport_runtime_state("session_id_changed");
                    }
                    if self.stream_watch_conn.is_some()
                        && self.stream_watch_pending_conn.session_id.as_deref()
                            != Some(session_id.as_str())
                    {
                        tracing::warn!(
                            previous = ?self.stream_watch_pending_conn.session_id,
                            current = ?new_session_id,
                            "Stream watch session id changed while connected; closing stream transport"
                        );
                        self.clear_stream_watch_connection();
                        self.emit_transport_state(
                            TransportRole::StreamWatch,
                            "disconnected",
                            Some("session_id_changed"),
                        );
                    }
                    if self.stream_publish_conn.is_some()
                        && self.stream_publish_pending_conn.session_id.as_deref()
                            != Some(session_id.as_str())
                    {
                        tracing::warn!(
                            previous = ?self.stream_publish_pending_conn.session_id,
                            current = ?new_session_id,
                            "Stream publish session id changed while connected; closing stream publish transport"
                        );
                        self.clear_stream_publish_connection();
                        self.emit_transport_state(
                            TransportRole::StreamPublish,
                            "disconnected",
                            Some("session_id_changed"),
                        );
                    }
                    self.pending_conn.session_id = Some(session_id.clone());
                    self.stream_watch_pending_conn.session_id = Some(session_id.clone());
                    self.stream_publish_pending_conn.session_id = Some(session_id.clone());
                }
                if let Some(user_id) = new_user_id {
                    self.pending_conn.user_id = Some(user_id);
                    self.self_user_id = Some(user_id);
                    if self.stream_watch_pending_conn.user_id.is_none() {
                        self.stream_watch_pending_conn.user_id = Some(user_id);
                    }
                    if self.stream_publish_pending_conn.user_id.is_none() {
                        self.stream_publish_pending_conn.user_id = Some(user_id);
                    }
                }
                self.maybe_try_connect("voice_state_connect_failed", "voice_state")
                    .await;
            }
            ConnectionCommand::StreamWatchConnect {
                endpoint,
                token,
                server_id,
                session_id,
                user_id,
                dave_channel_id,
            } => {
                let Some(user_id) =
                    crate::app_state::parse_user_id_field(&user_id, "stream_watch_connect.user_id")
                else {
                    return;
                };
                let Some(server_id) = crate::app_state::parse_user_id_field(
                    &server_id,
                    "stream_watch_connect.server_id",
                ) else {
                    return;
                };
                let Some(dave_channel_id) = crate::app_state::parse_user_id_field(
                    &dave_channel_id,
                    "stream_watch_connect.dave_channel_id",
                ) else {
                    return;
                };

                tracing::info!(
                    endpoint = %endpoint,
                    server_id,
                    dave_channel_id,
                    user_id,
                    "IPC stream_watch_connect received"
                );

                self.clear_stream_watch_connection();
                self.stream_watch_pending_conn.endpoint = Some(endpoint);
                self.stream_watch_pending_conn.token = Some(token);
                self.stream_watch_pending_conn.server_id = Some(server_id);
                self.stream_watch_pending_conn.session_id = Some(session_id);
                self.stream_watch_pending_conn.user_id = Some(user_id);
                self.stream_watch_pending_conn.dave_channel_id = Some(dave_channel_id);

                self.emit_transport_state(TransportRole::StreamWatch, "connecting", None);
                match self.try_connect_stream_watch().await {
                    TryConnectOutcome::Connected | TryConnectOutcome::AlreadyConnected => {}
                    TryConnectOutcome::MissingData => {
                        self.emit_transport_state(
                            TransportRole::StreamWatch,
                            "failed",
                            Some("missing_stream_watch_credentials"),
                        );
                    }
                    TryConnectOutcome::Failed => {}
                }
            }
            ConnectionCommand::StreamWatchDisconnect { reason } => {
                let disconnect_reason = reason.unwrap_or_else(|| "stream_watch_disconnect".into());
                tracing::info!(reason = %disconnect_reason, "IPC stream_watch_disconnect received");
                self.clear_stream_watch_connection();
                self.emit_transport_state(
                    TransportRole::StreamWatch,
                    "disconnected",
                    Some(&disconnect_reason),
                );
            }
            ConnectionCommand::StreamPublishConnect {
                endpoint,
                token,
                server_id,
                session_id,
                user_id,
                dave_channel_id,
            } => {
                let Some(user_id) = crate::app_state::parse_user_id_field(
                    &user_id,
                    "stream_publish_connect.user_id",
                ) else {
                    return;
                };
                let Some(server_id) = crate::app_state::parse_user_id_field(
                    &server_id,
                    "stream_publish_connect.server_id",
                ) else {
                    return;
                };
                let Some(dave_channel_id) = crate::app_state::parse_user_id_field(
                    &dave_channel_id,
                    "stream_publish_connect.dave_channel_id",
                ) else {
                    return;
                };

                tracing::info!(
                    endpoint = %endpoint,
                    server_id,
                    dave_channel_id,
                    user_id,
                    "IPC stream_publish_connect received"
                );

                self.clear_stream_publish_connection();
                self.stream_publish_pending_conn.endpoint = Some(endpoint);
                self.stream_publish_pending_conn.token = Some(token);
                self.stream_publish_pending_conn.server_id = Some(server_id);
                self.stream_publish_pending_conn.session_id = Some(session_id);
                self.stream_publish_pending_conn.user_id = Some(user_id);
                self.stream_publish_pending_conn.dave_channel_id = Some(dave_channel_id);

                self.emit_transport_state(TransportRole::StreamPublish, "connecting", None);
                match self.try_connect_stream_publish().await {
                    TryConnectOutcome::Connected | TryConnectOutcome::AlreadyConnected => {}
                    TryConnectOutcome::MissingData => {
                        self.emit_transport_state(
                            TransportRole::StreamPublish,
                            "failed",
                            Some("missing_stream_publish_credentials"),
                        );
                    }
                    TryConnectOutcome::Failed => {}
                }
            }
            ConnectionCommand::StreamPublishDisconnect { reason } => {
                let disconnect_reason =
                    reason.unwrap_or_else(|| "stream_publish_disconnect".into());
                tracing::info!(
                    reason = %disconnect_reason,
                    "IPC stream_publish_disconnect received"
                );
                self.stop_stream_publish_runtime("stream_publish_disconnect");
                self.clear_stream_publish_connection();
                self.emit_transport_state(
                    TransportRole::StreamPublish,
                    "disconnected",
                    Some(&disconnect_reason),
                );
            }
        }
    }
}

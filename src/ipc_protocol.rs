use crate::ipc::InMsg;

pub(crate) enum RoutedInMsg {
    Connection(ConnectionCommand),
    Capture(CaptureCommand),
    Playback(PlaybackCommand),
    StreamPublish(StreamPublishCommand),
}

pub(crate) enum ConnectionCommand {
    Join {
        guild_id: String,
        channel_id: String,
        self_mute: bool,
    },
    VoiceServer {
        data: crate::ipc::VoiceServerData,
    },
    VoiceState {
        data: crate::ipc::VoiceStateData,
    },
    StreamWatchConnect {
        endpoint: String,
        token: String,
        server_id: String,
        session_id: String,
        user_id: String,
        dave_channel_id: String,
    },
    StreamWatchDisconnect {
        reason: Option<String>,
    },
    StreamPublishConnect {
        endpoint: String,
        token: String,
        server_id: String,
        session_id: String,
        user_id: String,
        dave_channel_id: String,
    },
    StreamPublishDisconnect {
        reason: Option<String>,
    },
}

pub(crate) enum CaptureCommand {
    SubscribeUser {
        user_id: String,
        silence_duration_ms: u32,
        sample_rate: u32,
    },
    UnsubscribeUser {
        user_id: String,
    },
    SubscribeUserVideo {
        user_id: String,
        max_frames_per_second: u32,
        preferred_quality: u32,
        preferred_pixel_count: Option<u32>,
        preferred_stream_type: Option<String>,
    },
    UnsubscribeUserVideo {
        user_id: String,
    },
}

pub(crate) enum PlaybackCommand {
    Audio {
        pcm_base64: String,
        sample_rate: u32,
    },
    StopPlayback,
    StopTtsPlayback,
    MusicPlay {
        url: String,
        resolved_direct_url: bool,
    },
    MusicStop,
    MusicPause,
    MusicResume,
    MusicSetGain {
        target: f32,
        fade_ms: u32,
    },
    Destroy,
}

pub(crate) enum StreamPublishCommand {
    Play {
        url: String,
        resolved_direct_url: bool,
    },
    BrowserStart {
        mime_type: String,
    },
    BrowserFrame {
        mime_type: String,
        frame_base64: String,
        captured_at_ms: u64,
    },
    Stop,
    Pause,
    Resume,
}

impl TryFrom<InMsg> for RoutedInMsg {
    type Error = InMsg;

    fn try_from(msg: InMsg) -> Result<Self, Self::Error> {
        match msg {
            InMsg::Join {
                guild_id,
                channel_id,
                _self_deaf: _,
                self_mute,
            } => Ok(Self::Connection(ConnectionCommand::Join {
                guild_id,
                channel_id,
                self_mute,
            })),
            InMsg::VoiceServer { data } => {
                Ok(Self::Connection(ConnectionCommand::VoiceServer { data }))
            }
            InMsg::VoiceState { data } => {
                Ok(Self::Connection(ConnectionCommand::VoiceState { data }))
            }
            InMsg::StreamWatchConnect {
                endpoint,
                token,
                server_id,
                session_id,
                user_id,
                dave_channel_id,
            } => Ok(Self::Connection(ConnectionCommand::StreamWatchConnect {
                endpoint,
                token,
                server_id,
                session_id,
                user_id,
                dave_channel_id,
            })),
            InMsg::StreamWatchDisconnect { reason } => {
                Ok(Self::Connection(ConnectionCommand::StreamWatchDisconnect {
                    reason,
                }))
            }
            InMsg::StreamPublishConnect {
                endpoint,
                token,
                server_id,
                session_id,
                user_id,
                dave_channel_id,
            } => Ok(Self::Connection(ConnectionCommand::StreamPublishConnect {
                endpoint,
                token,
                server_id,
                session_id,
                user_id,
                dave_channel_id,
            })),
            InMsg::StreamPublishDisconnect { reason } => Ok(Self::Connection(
                ConnectionCommand::StreamPublishDisconnect { reason },
            )),
            InMsg::SubscribeUser {
                user_id,
                silence_duration_ms,
                sample_rate,
            } => Ok(Self::Capture(CaptureCommand::SubscribeUser {
                user_id,
                silence_duration_ms,
                sample_rate,
            })),
            InMsg::UnsubscribeUser { user_id } => {
                Ok(Self::Capture(CaptureCommand::UnsubscribeUser { user_id }))
            }
            InMsg::SubscribeUserVideo {
                user_id,
                max_frames_per_second,
                preferred_quality,
                preferred_pixel_count,
                preferred_stream_type,
            } => Ok(Self::Capture(CaptureCommand::SubscribeUserVideo {
                user_id,
                max_frames_per_second,
                preferred_quality,
                preferred_pixel_count,
                preferred_stream_type,
            })),
            InMsg::UnsubscribeUserVideo { user_id } => {
                Ok(Self::Capture(CaptureCommand::UnsubscribeUserVideo {
                    user_id,
                }))
            }
            InMsg::Audio {
                pcm_base64,
                sample_rate,
            } => Ok(Self::Playback(PlaybackCommand::Audio {
                pcm_base64,
                sample_rate,
            })),
            InMsg::StopPlayback => Ok(Self::Playback(PlaybackCommand::StopPlayback)),
            InMsg::StopTtsPlayback => Ok(Self::Playback(PlaybackCommand::StopTtsPlayback)),
            InMsg::MusicPlay {
                url,
                resolved_direct_url,
            } => Ok(Self::Playback(PlaybackCommand::MusicPlay {
                url,
                resolved_direct_url,
            })),
            InMsg::MusicStop => Ok(Self::Playback(PlaybackCommand::MusicStop)),
            InMsg::MusicPause => Ok(Self::Playback(PlaybackCommand::MusicPause)),
            InMsg::MusicResume => Ok(Self::Playback(PlaybackCommand::MusicResume)),
            InMsg::MusicSetGain { target, fade_ms } => {
                Ok(Self::Playback(PlaybackCommand::MusicSetGain {
                    target,
                    fade_ms,
                }))
            }
            InMsg::StreamPublishPlay {
                url,
                resolved_direct_url,
            } => Ok(Self::StreamPublish(StreamPublishCommand::Play {
                url,
                resolved_direct_url,
            })),
            InMsg::StreamPublishBrowserStart { mime_type } => {
                Ok(Self::StreamPublish(StreamPublishCommand::BrowserStart {
                    mime_type,
                }))
            }
            InMsg::StreamPublishBrowserFrame {
                mime_type,
                frame_base64,
                captured_at_ms,
            } => Ok(Self::StreamPublish(StreamPublishCommand::BrowserFrame {
                mime_type,
                frame_base64,
                captured_at_ms,
            })),
            InMsg::StreamPublishStop => Ok(Self::StreamPublish(StreamPublishCommand::Stop)),
            InMsg::StreamPublishPause => Ok(Self::StreamPublish(StreamPublishCommand::Pause)),
            InMsg::StreamPublishResume => Ok(Self::StreamPublish(StreamPublishCommand::Resume)),
            InMsg::Destroy => Ok(Self::Playback(PlaybackCommand::Destroy)),
        }
    }
}

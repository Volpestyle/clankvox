use crate::ipc::InMsg;

pub(crate) enum RoutedInMsg {
    Connection(ConnectionCommand),
    Capture(CaptureCommand),
    Playback(PlaybackCommand),
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
            InMsg::Destroy => Ok(Self::Playback(PlaybackCommand::Destroy)),
        }
    }
}

use strum_macros::{Display, EnumString};

/// https://freeswitch.org/confluence/display/FREESWITCH/Event+List
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Display, EnumString)]
#[strum(serialize_all = "shouty_snake_case")]
pub enum EventType {
    // Channel events
    ChannelCallstate,
    ChannelCreate,
    ChannelDestroy,
    ChannelState,
    ChannelAnswer,
    ChannelHangup,
    ChannelHangupComplete,
    ChannelExecute,
    ChannelExecuteComplete,
    ChannelBridge,
    ChannelUnbridge,
    ChannelProgress,
    ChannelProgressMedia,
    ChannelOutgoing,
    ChannelPark,
    ChannelUnpark,
    ChannelApplication,
    ChannelHold,
    ChannelUnhold,
    ChannelOriginate,
    ChannelUuid,

    // System events
    Shutdown,
    ModuleLoad,
    ModuleUnload,
    ReloadXml,
    Notify,
    SendMessage,
    RecvMessage,
    RequestParams,
    ChannelData,
    General,
    Command,
    SessionHeartbeat,
    ClientDisconnected,
    ServerDisconnected,
    SendInfo,
    RecvInfo,
    CallSecure,
    Nat,
    RecordStart,
    RecordStop,
    PlaybackStart,
    PlaybackStop,
    CallUpdate,

    // Other events
    Api,
    BackgroundJob,
    Custom,
    ReSchedule,
    Heartbeat,
    DetectedTone,

    // Undocumented
    Log,
    InboundChan,
    OutboundChan,
    Startup,
    Publish,
    Unpublish,
    Talk,
    Notalk,
    SessionCrash,
    Dtmf,
    Message,
    PresenceIn,
    PresenceOut,
    PresenceProbe,
    MessageWaiting,
    MessageQuery,
    Roster,
    RecvRtcpMessage,
    Codec,
    DetectedSpeech,
    PrivateCommand,
    Trap,
    AddSchedule,
    DelSchedule,
    ExeSchedule,

    // From NEventSocket:
    Clone,
    NotifyIn,
    Failure,
    SocketData,
    MediaBugStart,
    MediaBugStop,
    ConferenceDataQuery,
    ConferenceData,
    CallSetupReq,
    CallSetupResult,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn event_type_to_string_results_in_shouty_snake_case() {
        assert_eq!(&EventType::ChannelState.to_string(), "CHANNEL_STATE")
    }

    #[test]
    fn parsing_event_type_with_shouty_case() {
        assert_eq!(
            EventType::from_str("CHANNEL_STATE"),
            Ok(EventType::ChannelState)
        );
    }
}

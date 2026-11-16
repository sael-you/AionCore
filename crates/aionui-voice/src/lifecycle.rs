//! Call lifecycle rules (§3.6): end reasons, timers, and the per-tick decision.

use std::time::Duration;

use crate::engine::EngineTimings;

/// Why a call ended. `as_str` is persisted (`voice_sessions.end_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    User,
    /// Client disarmed ("Jarvis, sleep" / tap).
    Sleep,
    /// Client gave up after `/api/voice/ws` stayed down (§3.6.4).
    ClientOffline,
    /// Client saw ICE disconnected/failed (§3.6.5).
    NetworkLost,
    Idle,
    HardCap,
    Rotation,
    Budget,
    Disabled,
    AuthRevoked,
    DeviceFrozen,
    DeviceStale,
    Resumed,
    ConfigTimeout,
    AttachFailed,
    CreateFailed,
    ControlLost,
    ProviderEnded,
    Restart,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Sleep => "sleep",
            Self::ClientOffline => "offline",
            Self::NetworkLost => "network",
            Self::Idle => "idle",
            Self::HardCap => "hard_cap",
            Self::Rotation => "rotation",
            Self::Budget => "budget",
            Self::Disabled => "disabled",
            Self::AuthRevoked => "auth_revoked",
            Self::DeviceFrozen => "device_frozen",
            Self::DeviceStale => "device_stale",
            Self::Resumed => "resumed",
            Self::ConfigTimeout => "config_timeout",
            Self::AttachFailed => "attach_failed",
            Self::CreateFailed => "create_failed",
            Self::ControlLost => "control_lost",
            Self::ProviderEnded => "provider_ended",
            Self::Restart => "restart",
        }
    }

    /// Map the client's `POST …/end {reason}`; unknown values count as `user`.
    pub fn from_client(reason: Option<&str>) -> Self {
        match reason.map(str::trim) {
            Some("sleep") => Self::Sleep,
            Some("idle") => Self::Idle,
            Some("offline") => Self::ClientOffline,
            Some("network") => Self::NetworkLost,
            _ => Self::User,
        }
    }

    /// Persisted terminal state: setup failures are `failed`, the rest `ended`.
    pub fn terminal_state(self) -> &'static str {
        match self {
            Self::ConfigTimeout | Self::AttachFailed | Self::CreateFailed | Self::ControlLost | Self::Restart => {
                "failed"
            }
            _ => "ended",
        }
    }
}

/// All voice timers. Production values per PLAN §1/§3.5/§3.6/§3.8; tests
/// shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceTimings {
    pub engine: EngineTimings,
    /// Lifecycle tick of a call runner.
    pub tick: Duration,
    /// Hang up after this much silence (stays armed).
    pub idle_hangup: Duration,
    /// Rotate from here at the first idle point with no draft pending.
    pub rotation_after: Duration,
    /// Quiet time that counts as an "idle point" for rotation.
    pub rotation_quiet: Duration,
    /// Hard cap per call (provider maximum is 60 min).
    pub hard_cap: Duration,
    /// Re-validate the session token of live sockets and calls.
    pub revalidate_every: Duration,
    /// A device without heartbeat for this long is not armed.
    pub heartbeat_stale: Duration,
    /// Keep trying to re-attach a lost sideband until this deadline.
    pub reattach_deadline: Duration,
    pub reattach_retry_delay: Duration,
    /// First socket frame must be `voice.hello` within this time.
    pub hello_timeout: Duration,
    /// Conversation voice mode: how long the binding outlives its call when
    /// no voice socket of the user is connected any more (C26).
    pub binding_grace: Duration,
}

impl Default for VoiceTimings {
    fn default() -> Self {
        Self {
            engine: EngineTimings::default(),
            tick: Duration::from_secs(1),
            idle_hangup: Duration::from_secs(180),
            rotation_after: Duration::from_secs(30 * 60),
            rotation_quiet: Duration::from_secs(5),
            hard_cap: Duration::from_secs(55 * 60),
            revalidate_every: Duration::from_secs(60),
            heartbeat_stale: Duration::from_secs(25),
            reattach_deadline: Duration::from_secs(20),
            reattach_retry_delay: Duration::from_millis(500),
            hello_timeout: Duration::from_secs(5),
            binding_grace: Duration::from_secs(30 * 60),
        }
    }
}

/// What the runner knows at a tick (durations since the respective event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInputs {
    pub since_start: Duration,
    pub since_activity: Duration,
    pub since_heartbeat: Duration,
    pub device_frozen: bool,
    pub response_in_flight: bool,
    pub assistant_speaking: bool,
    /// Drafts arrive in S1b; rotation must wait while one is pending.
    pub draft_pending: bool,
}

/// The end reason due at this tick, if any (checked in priority order).
pub fn tick_decision(inputs: &TickInputs, timings: &VoiceTimings) -> Option<EndReason> {
    if inputs.since_start >= timings.hard_cap {
        return Some(EndReason::HardCap);
    }
    if inputs.device_frozen {
        return Some(EndReason::DeviceFrozen);
    }
    if inputs.since_heartbeat >= timings.heartbeat_stale {
        return Some(EndReason::DeviceStale);
    }
    let busy = inputs.response_in_flight || inputs.assistant_speaking;
    if !busy && inputs.since_activity >= timings.idle_hangup {
        return Some(EndReason::Idle);
    }
    if inputs.since_start >= timings.rotation_after
        && !busy
        && !inputs.draft_pending
        && inputs.since_activity >= timings.rotation_quiet
    {
        return Some(EndReason::Rotation);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(since_start: u64, since_activity: u64) -> TickInputs {
        TickInputs {
            since_start: Duration::from_secs(since_start),
            since_activity: Duration::from_secs(since_activity),
            since_heartbeat: Duration::from_secs(1),
            device_frozen: false,
            response_in_flight: false,
            assistant_speaking: false,
            draft_pending: false,
        }
    }

    #[test]
    fn idle_hangup_after_three_minutes() {
        let t = VoiceTimings::default();
        assert_eq!(tick_decision(&quiet(100, 179), &t), None);
        assert_eq!(tick_decision(&quiet(200, 180), &t), Some(EndReason::Idle));
        let mut speaking = quiet(200, 200);
        speaking.assistant_speaking = true;
        assert_eq!(tick_decision(&speaking, &t), None, "never idle while Jarvis speaks");
    }

    #[test]
    fn hard_cap_at_55_minutes_wins() {
        let t = VoiceTimings::default();
        let mut busy = quiet(55 * 60, 0);
        busy.response_in_flight = true;
        assert_eq!(tick_decision(&busy, &t), Some(EndReason::HardCap));
    }

    #[test]
    fn rotation_from_30_minutes_at_an_idle_point_without_draft() {
        let t = VoiceTimings::default();
        assert_eq!(tick_decision(&quiet(29 * 60, 60), &t), None);
        assert_eq!(tick_decision(&quiet(30 * 60, 2), &t), None, "not at an idle point yet");
        assert_eq!(tick_decision(&quiet(30 * 60, 6), &t), Some(EndReason::Rotation));
        let mut draft = quiet(31 * 60, 60);
        draft.draft_pending = true;
        assert_eq!(tick_decision(&draft, &t), None);
        let mut busy = quiet(31 * 60, 60);
        busy.response_in_flight = true;
        assert_eq!(tick_decision(&busy, &t), None);
    }

    #[test]
    fn frozen_or_stale_device_hangs_up() {
        let t = VoiceTimings::default();
        let mut frozen = quiet(10, 1);
        frozen.device_frozen = true;
        assert_eq!(tick_decision(&frozen, &t), Some(EndReason::DeviceFrozen));
        let mut stale = quiet(10, 1);
        stale.since_heartbeat = Duration::from_secs(25);
        assert_eq!(tick_decision(&stale, &t), Some(EndReason::DeviceStale));
    }

    #[test]
    fn client_reasons_map_to_known_values() {
        assert_eq!(EndReason::from_client(Some("sleep")), EndReason::Sleep);
        assert_eq!(EndReason::from_client(Some("network")).as_str(), "network");
        assert_eq!(EndReason::from_client(Some("<script>")), EndReason::User);
        assert_eq!(EndReason::from_client(None), EndReason::User);
    }

    #[test]
    fn failure_reasons_persist_as_failed() {
        assert_eq!(EndReason::ConfigTimeout.terminal_state(), "failed");
        assert_eq!(EndReason::Idle.terminal_state(), "ended");
        assert_eq!(EndReason::HardCap.as_str(), "hard_cap");
    }
}

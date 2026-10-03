//! Broadcast servers — which servers carry broadcasts today and how they are doing
//! (broadcast-ops page, P1). A 10 s collector observes mm-switch, LiveKit and the active
//! streams; `build_view` turns one observation into the page's snapshot; `SnapshotCell`
//! caches it, so a page load never probes anything.
//!
//! Privacy: viewer ids embed Matrix user ids. They are matched against a broadcast's id
//! prefix and counted here; no id ever leaves this module.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use mm_core::switch_client::{
    SwitchHealth, SwitchSource, SwitchViewer, switch_source_id, switch_viewer_prefix,
};
use mm_db::models::Stream;

use crate::stream_lifecycle::{RoomLookup, sweep_considers_occupied};

/// Collector period, seconds.
pub const COLLECT_INTERVAL_SECS: u64 = 10;
/// Consecutive failures of a server's primary probe before it is reported unreachable.
pub const UNREACHABLE_AFTER: u32 = 3;
/// The sweep's own bound on active streams; the snapshot says when it was hit.
pub const STREAM_LIMIT: u32 = 100;

/// Recording rows whose `egress_id` starts with this are written by mm-switch
/// (`client.rs` `format!("mm-switch:{}", …)`); anything else is LiveKit egress.
const SWITCH_EGRESS_PREFIX: &str = "mm-switch:";

const ROLE_SWITCH: &str = "origin — carries every broadcast's media";
const ROLE_LIVEKIT: &str = "one room per broadcast; media only in fallbacks";
const ROLE_EGRESS: &str = "fallback recordings";
const ROLE_COTURN: &str = "relay for clients that cannot connect directly";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerKind {
    MmSwitch,
    Livekit,
    LivekitEgress,
    Coturn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatus {
    Ok,
    Degraded,
    Unreachable,
    NotConfigured,
    NotMonitored,
}

/// One probe's raw outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    NotConfigured,
    /// The primary call and every secondary call succeeded.
    Ok,
    /// The primary call succeeded; a secondary call failed.
    Degraded,
    /// The primary call failed.
    Failed,
}

/// Per-server probe history: turns raw probes into a status that does not flap.
#[derive(Debug, Clone, Default)]
pub struct ProbeTracker {
    consecutive_failures: u32,
    last_ok_at: Option<DateTime<Utc>>,
    last_status: Option<ServerStatus>,
}

#[derive(Debug, Default)]
pub struct Trackers {
    pub switch: ProbeTracker,
    pub livekit: ProbeTracker,
}

/// What the switch said this tick.
#[derive(Debug)]
pub enum SwitchObservation {
    /// No `advertising.switch_url`.
    NotConfigured,
    /// `/health` failed.
    Unreachable { error: String, latency_ms: u64 },
    /// `/health` answered; the list calls may still have failed.
    Reachable {
        health: SwitchHealth,
        latency_ms: u64,
        sources: Result<Vec<SwitchSource>, String>,
        viewers: Result<Vec<SwitchViewer>, String>,
    },
}

/// An open (`recording` or `paused`) recording row.
#[derive(Debug, Clone)]
pub struct OpenRecording {
    pub egress_id: Option<String>,
    pub status: String,
}

#[derive(Debug)]
pub struct StreamObservation {
    pub stream: Stream,
    pub room: RoomLookup,
    /// Open recordings, or why they could not be read.
    pub recordings: Result<Vec<OpenRecording>, String>,
    /// Active LiveKit egress jobs for the room; `None` without a room or when the listing failed.
    pub egresses_active: Option<u64>,
}

#[derive(Debug)]
pub struct Observations {
    pub at: DateTime<Utc>,
    pub switch: SwitchObservation,
    /// LiveKit `health_check`: latency on success, the error otherwise.
    pub livekit: Result<u64, String>,
    pub streams: Result<Vec<StreamObservation>, String>,
    /// `streaming.auto_end_grace_secs` this tick (0 = sweep off).
    pub sweep_grace_secs: u64,
    /// `streaming.switch_viewer_capacity` this tick (0 = not measured).
    pub capacity_estimate: u64,
    pub turn_urls: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ServerDetail {
    Switch {
        sources: u64,
        viewers: u64,
        recorders: BTreeMap<String, u64>,
    },
    Livekit {
        /// `None` when LiveKit did not answer or the stream listing failed — unknown, not 0.
        participants: Option<u64>,
        /// Broadcasts whose room lookup failed this tick (their participants are not in the sum).
        rooms_unavailable: u64,
    },
    Egress {
        /// `None` when the stream listing failed or a broadcast with a room had no egress listing.
        active: Option<u64>,
    },
    Coturn {
        urls_configured: usize,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerView {
    pub kind: ServerKind,
    pub role: &'static str,
    /// This and every field below are `None` for the demo role.
    pub status: Option<ServerStatus>,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub consecutive_failures: Option<u32>,
    pub latency_ms: Option<u64>,
    pub last_error: Option<String>,
    pub detail: Option<ServerDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPath {
    Switch,
    Egress,
    None,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingView {
    pub path: RecordingPath,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Warning {
    /// The switch carries the broadcast, but the auto-end sweep's rule says "empty".
    SweepSeesEmpty,
    /// Active in the database, no programme source on the switch.
    SwitchSourceMissing,
    /// Recording on LiveKit egress, not on the switch.
    RecordingFallback,
}

#[derive(Debug, Clone, Serialize)]
pub struct BroadcastView {
    pub stream_id: String,
    pub title: Option<String>,
    pub host: String,
    pub started_at: DateTime<Utc>,
    /// `None` when the switch's source list was not available this tick.
    pub switch_source: Option<bool>,
    /// `None` when the switch's viewer list was not available this tick.
    pub switch_viewers: Option<u64>,
    /// `None` without a room or when LiveKit did not answer.
    pub livekit_participants: Option<u64>,
    pub recording: RecordingView,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapacityView {
    pub viewers: Option<u64>,
    pub sources: Option<u64>,
    pub recorders: BTreeMap<String, u64>,
    /// `None` = not measured (`streaming.switch_viewer_capacity` is 0).
    pub estimate: Option<u64>,
    pub over: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BroadcastServersView {
    pub demo: bool,
    /// `None` until the first collector tick completes.
    pub collected_at: Option<DateTime<Utc>>,
    pub collector_interval_secs: u64,
    pub servers: Vec<ServerView>,
    pub capacity: Option<CapacityView>,
    pub broadcasts: Vec<BroadcastView>,
    pub broadcasts_error: Option<String>,
    pub truncated: bool,
}

impl ProbeTracker {
    /// Fold one probe into the tracker and return the status to show.
    ///
    /// One or two failures in a row keep the previous status (the count is shown beside
    /// it) so a single blip does not flap the page; the third is `Unreachable`. A failure
    /// with no previous status is `Unreachable` at once — never a guessed `Ok`.
    pub fn record(&mut self, probe: Probe, now: DateTime<Utc>) -> ServerStatus {
        let status = match probe {
            Probe::NotConfigured => {
                *self = Self::default();
                return ServerStatus::NotConfigured;
            }
            Probe::Ok | Probe::Degraded => {
                self.consecutive_failures = 0;
                self.last_ok_at = Some(now);
                if probe == Probe::Ok {
                    ServerStatus::Ok
                } else {
                    ServerStatus::Degraded
                }
            }
            Probe::Failed => {
                self.consecutive_failures += 1;
                match self.last_status {
                    Some(previous) if self.consecutive_failures < UNREACHABLE_AFTER => previous,
                    _ => ServerStatus::Unreachable,
                }
            }
        };
        self.last_status = Some(status);
        status
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn last_ok_at(&self) -> Option<DateTime<Utc>> {
        self.last_ok_at
    }
}

impl BroadcastServersView {
    /// The demo role sees the page's structure only: which kinds of server exist and what
    /// each is for. No status, number, id, title or error — absent, so nothing can leak.
    pub fn demo() -> Self {
        let blank = |kind: ServerKind, role: &'static str| ServerView {
            kind,
            role,
            status: None,
            last_ok_at: None,
            consecutive_failures: None,
            latency_ms: None,
            last_error: None,
            detail: None,
        };
        Self {
            demo: true,
            collected_at: None,
            collector_interval_secs: COLLECT_INTERVAL_SECS,
            servers: vec![
                blank(ServerKind::MmSwitch, ROLE_SWITCH),
                blank(ServerKind::Livekit, ROLE_LIVEKIT),
                blank(ServerKind::LivekitEgress, ROLE_EGRESS),
                blank(ServerKind::Coturn, ROLE_COTURN),
            ],
            capacity: None,
            broadcasts: Vec::new(),
            broadcasts_error: None,
            truncated: false,
        }
    }

    /// Before the collector's first tick completes.
    pub fn collecting() -> Self {
        Self {
            demo: false,
            ..Self::demo()
        }
    }
}

/// Turn one observation into the page's snapshot. No I/O, no clock; advances the trackers
/// (call once per observation).
pub fn build_view(obs: &Observations, trackers: &mut Trackers) -> BroadcastServersView {
    let switch = switch_server_view(&obs.switch, &mut trackers.switch, obs.at);

    let livekit_status = trackers.livekit.record(
        if obs.livekit.is_ok() {
            Probe::Ok
        } else {
            Probe::Failed
        },
        obs.at,
    );

    let (broadcasts, broadcasts_error, truncated): (Vec<BroadcastView>, Option<String>, bool) =
        match &obs.streams {
            Ok(streams) => (
                streams
                    .iter()
                    .map(|s| broadcast_view(s, &obs.switch, obs.sweep_grace_secs))
                    .collect(),
                None,
                streams.len() >= STREAM_LIMIT as usize,
            ),
            Err(e) => (Vec::new(), Some(e.clone()), false),
        };
    // A failure is unknown, never 0. In production most broadcasts' LiveKit rooms are already
    // gone (`RoomLookup::Failed` / `NoRoom`), so one failed room must not blank the whole count:
    // sum what answered and say how many did not.
    let participants: Option<u64> = (obs.livekit.is_ok() && obs.streams.is_ok()).then(|| {
        broadcasts
            .iter()
            .filter_map(|b| b.livekit_participants)
            .sum()
    });
    let rooms_unavailable: u64 = obs.streams.as_ref().map_or(0, |streams| {
        streams
            .iter()
            .filter(|s| s.room == RoomLookup::Failed)
            .count() as u64
    });
    let egress_active: Option<u64> = obs.streams.as_ref().ok().and_then(|streams| {
        let mut total = 0u64;
        for s in streams {
            match s.egresses_active {
                Some(n) => total += n,
                // A stream with a room whose egress listing failed makes the total unknown;
                // a stream without a room has no egress to list.
                None if s.stream.sfu_room_id.is_some() => return None,
                None => {}
            }
        }
        Some(total)
    });

    let unmonitored = |kind: ServerKind, role: &'static str, detail: ServerDetail| ServerView {
        kind,
        role,
        status: Some(ServerStatus::NotMonitored),
        last_ok_at: None,
        consecutive_failures: None,
        latency_ms: None,
        last_error: None,
        detail: Some(detail),
    };

    let servers = vec![
        switch,
        ServerView {
            kind: ServerKind::Livekit,
            role: ROLE_LIVEKIT,
            status: Some(livekit_status),
            last_ok_at: trackers.livekit.last_ok_at(),
            consecutive_failures: Some(trackers.livekit.consecutive_failures()),
            latency_ms: obs.livekit.as_ref().ok().copied(),
            last_error: obs.livekit.as_ref().err().cloned(),
            detail: Some(ServerDetail::Livekit {
                participants,
                rooms_unavailable,
            }),
        },
        unmonitored(
            ServerKind::LivekitEgress,
            ROLE_EGRESS,
            ServerDetail::Egress {
                active: egress_active,
            },
        ),
        unmonitored(
            ServerKind::Coturn,
            ROLE_COTURN,
            ServerDetail::Coturn {
                urls_configured: obs.turn_urls,
            },
        ),
    ];

    BroadcastServersView {
        demo: false,
        collected_at: Some(obs.at),
        collector_interval_secs: COLLECT_INTERVAL_SECS,
        servers,
        capacity: Some(capacity_view(&obs.switch, obs.capacity_estimate)),
        broadcasts,
        broadcasts_error,
        truncated,
    }
}

fn switch_server_view(
    obs: &SwitchObservation,
    tracker: &mut ProbeTracker,
    now: DateTime<Utc>,
) -> ServerView {
    let (probe, latency_ms, last_error, detail) = match obs {
        SwitchObservation::NotConfigured => (Probe::NotConfigured, None, None, None),
        SwitchObservation::Unreachable { error, latency_ms } => {
            (Probe::Failed, Some(*latency_ms), Some(error.clone()), None)
        }
        SwitchObservation::Reachable {
            health,
            latency_ms,
            sources,
            viewers,
        } => {
            let error = [sources.as_ref().err(), viewers.as_ref().err()]
                .into_iter()
                .flatten()
                .next()
                .cloned();
            let probe = if error.is_none() {
                Probe::Ok
            } else {
                Probe::Degraded
            };
            let detail = ServerDetail::Switch {
                sources: health.sources,
                viewers: health.viewers,
                recorders: health.recorders.clone(),
            };
            (probe, Some(*latency_ms), error, Some(detail))
        }
    };
    let status = tracker.record(probe, now);
    ServerView {
        kind: ServerKind::MmSwitch,
        role: ROLE_SWITCH,
        status: Some(status),
        last_ok_at: tracker.last_ok_at(),
        consecutive_failures: Some(tracker.consecutive_failures()),
        latency_ms,
        last_error,
        detail,
    }
}

fn broadcast_view(
    o: &StreamObservation,
    switch: &SwitchObservation,
    sweep_grace_secs: u64,
) -> BroadcastView {
    let id = &o.stream.id;
    let (switch_source, switch_viewers) = match switch {
        SwitchObservation::Reachable {
            sources, viewers, ..
        } => {
            let source_id = switch_source_id(id);
            let prefix = switch_viewer_prefix(id);
            (
                sources
                    .as_ref()
                    .ok()
                    .map(|list| list.iter().any(|s| s.id == source_id && s.active)),
                viewers.as_ref().ok().map(|list| {
                    list.iter()
                        .filter(|v| v.connected && v.id.starts_with(&prefix))
                        .count() as u64
                }),
            )
        }
        _ => (None, None),
    };
    let livekit_participants = match o.room {
        RoomLookup::Participants(n) => Some(n as u64),
        RoomLookup::NoRoom | RoomLookup::Failed => None,
    };
    let recording = recording_view(&o.recordings);

    let mut warnings = Vec::new();
    if switch_source == Some(true) && sweep_grace_secs > 0 && !sweep_considers_occupied(o.room) {
        warnings.push(Warning::SweepSeesEmpty);
    }
    if switch_source == Some(false) {
        warnings.push(Warning::SwitchSourceMissing);
    }
    if recording.path == RecordingPath::Egress {
        warnings.push(Warning::RecordingFallback);
    }

    BroadcastView {
        stream_id: id.clone(),
        title: o.stream.title.clone(),
        host: o.stream.host_user_id.clone(),
        started_at: o.stream.started_at,
        switch_source,
        switch_viewers,
        livekit_participants,
        recording,
        warnings,
    }
}

fn recording_view(recordings: &Result<Vec<OpenRecording>, String>) -> RecordingView {
    let Ok(list) = recordings else {
        return RecordingView {
            path: RecordingPath::Unknown,
            state: None,
        };
    };
    let on_switch = list.iter().find(|r| {
        r.egress_id
            .as_deref()
            .is_some_and(|e| e.starts_with(SWITCH_EGRESS_PREFIX))
    });
    match (on_switch, list.first()) {
        (Some(r), _) => RecordingView {
            path: RecordingPath::Switch,
            state: Some(r.status.clone()),
        },
        (None, Some(r)) => RecordingView {
            path: RecordingPath::Egress,
            state: Some(r.status.clone()),
        },
        (None, None) => RecordingView {
            path: RecordingPath::None,
            state: None,
        },
    }
}

fn capacity_view(switch: &SwitchObservation, estimate: u64) -> CapacityView {
    let estimate = (estimate > 0).then_some(estimate);
    match switch {
        SwitchObservation::Reachable { health, .. } => CapacityView {
            viewers: Some(health.viewers),
            sources: Some(health.sources),
            recorders: health.recorders.clone(),
            estimate,
            over: estimate.is_some_and(|e| health.viewers > e),
        },
        SwitchObservation::NotConfigured | SwitchObservation::Unreachable { .. } => CapacityView {
            viewers: None,
            sources: None,
            recorders: BTreeMap::new(),
            estimate,
            over: false,
        },
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use mm_core::switch_client::switch_viewer_id;

    pub(crate) const STREAM_A: &str = "3f2a1c9e-0000-4000-8000-00000000000a";
    pub(crate) const STREAM_B: &str = "3f2a1c9e-0000-4000-8000-00000000000b";

    pub(crate) fn stream(id: &str) -> Stream {
        Stream {
            id: id.to_string(),
            room_id: 1,
            host_user_id: "@host:leak.example".into(),
            media_type: "video".into(),
            title: Some("Secret title".into()),
            status: "active".into(),
            sfu_room_id: Some(format!("mm-{id}")),
            participant_count: 0,
            started_at: Utc::now(),
            ended_at: None,
            state_event_id: None,
            feed_started_event_id: None,
            e2ee_enabled: false,
            e2ee_algorithm: None,
            e2ee_key_id: None,
            e2ee_key_generation: None,
            min_tier_level: None,
            ended_event_id: None,
            marker_generation: 1,
        }
    }

    pub(crate) fn stream_obs(id: &str, room: RoomLookup) -> StreamObservation {
        StreamObservation {
            stream: stream(id),
            room,
            recordings: Ok(vec![]),
            egresses_active: Some(0),
        }
    }

    pub(crate) fn source(id: &str, active: bool) -> SwitchSource {
        SwitchSource {
            id: id.into(),
            source_type: "webrtc".into(),
            active,
        }
    }

    pub(crate) fn viewer(id: String, current: &str, connected: bool) -> SwitchViewer {
        SwitchViewer {
            id,
            current_source: current.into(),
            connected,
        }
    }

    pub(crate) fn reachable(
        sources: Vec<SwitchSource>,
        viewers: Vec<SwitchViewer>,
    ) -> SwitchObservation {
        SwitchObservation::Reachable {
            health: SwitchHealth {
                sources: sources.len() as u64,
                viewers: viewers.len() as u64,
                recorders: BTreeMap::new(),
            },
            latency_ms: 3,
            sources: Ok(sources),
            viewers: Ok(viewers),
        }
    }

    pub(crate) fn obs(switch: SwitchObservation, streams: Vec<StreamObservation>) -> Observations {
        Observations {
            at: Utc::now(),
            switch,
            livekit: Ok(5),
            streams: Ok(streams),
            sweep_grace_secs: 600,
            capacity_estimate: 0,
            turn_urls: 1,
        }
    }

    /// One broadcast on the switch with two counted viewers (one mid ad-break), one
    /// disconnected viewer, one viewer of another broadcast — and no LiveKit room.
    pub(crate) fn sample() -> Observations {
        obs(
            reachable(
                vec![source(&switch_source_id(STREAM_A), true)],
                vec![
                    viewer(
                        switch_viewer_id(STREAM_A, "@viewer:leak.example"),
                        &switch_source_id(STREAM_A),
                        true,
                    ),
                    viewer(
                        switch_viewer_id(STREAM_A, "@adwatcher:leak.example"),
                        "ad--adwatcher-leak.example-1700000000000",
                        true,
                    ),
                    viewer(
                        switch_viewer_id(STREAM_A, "@gone:leak.example"),
                        &switch_source_id(STREAM_A),
                        false,
                    ),
                    viewer(
                        switch_viewer_id(STREAM_B, "@other:leak.example"),
                        &switch_source_id(STREAM_B),
                        true,
                    ),
                ],
            ),
            vec![stream_obs(STREAM_A, RoomLookup::Failed)],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn server(v: &BroadcastServersView, kind: ServerKind) -> &ServerView {
        v.servers
            .iter()
            .find(|s| s.kind == kind)
            .expect("server present")
    }

    #[test]
    fn tracker_keeps_the_previous_status_for_two_failures_then_reports_unreachable() {
        let mut t = ProbeTracker::default();
        let now = Utc::now();
        assert_eq!(t.record(Probe::Ok, now), ServerStatus::Ok);
        assert_eq!(t.record(Probe::Failed, now), ServerStatus::Ok);
        assert_eq!(t.consecutive_failures(), 1);
        assert_eq!(t.record(Probe::Failed, now), ServerStatus::Ok);
        assert_eq!(t.record(Probe::Failed, now), ServerStatus::Unreachable);
        assert_eq!(t.record(Probe::Ok, now), ServerStatus::Ok);
        assert_eq!(t.consecutive_failures(), 0);
    }

    #[test]
    fn tracker_never_guesses_ok_for_a_first_failure() {
        let mut t = ProbeTracker::default();
        assert_eq!(
            t.record(Probe::Failed, Utc::now()),
            ServerStatus::Unreachable
        );
    }

    #[test]
    fn tracker_not_configured_resets() {
        let mut t = ProbeTracker::default();
        t.record(Probe::Failed, Utc::now());
        assert_eq!(
            t.record(Probe::NotConfigured, Utc::now()),
            ServerStatus::NotConfigured
        );
        assert_eq!(t.consecutive_failures(), 0);
        assert_eq!(t.last_ok_at(), None);
    }

    #[test]
    fn viewers_are_counted_per_broadcast_by_id_prefix() {
        let v = build_view(&sample(), &mut Trackers::default());
        let b = &v.broadcasts[0];
        // The viewer and the ad-break watcher; not the disconnected one, not STREAM_B's.
        assert_eq!(b.switch_viewers, Some(2));
        assert_eq!(b.switch_source, Some(true));
    }

    #[test]
    fn sweep_sees_empty_when_the_switch_carries_a_broadcast_whose_room_is_empty() {
        let v = build_view(&sample(), &mut Trackers::default());
        assert_eq!(v.broadcasts[0].warnings, vec![Warning::SweepSeesEmpty]);
    }

    #[test]
    fn no_sweep_warning_when_the_sweep_is_off_or_the_room_is_occupied() {
        let mut off = sample();
        off.sweep_grace_secs = 0;
        assert!(
            build_view(&off, &mut Trackers::default()).broadcasts[0]
                .warnings
                .is_empty()
        );

        let mut occupied = sample();
        occupied.streams.as_mut().expect("fixture streams")[0].room = RoomLookup::Participants(1);
        let v = build_view(&occupied, &mut Trackers::default());
        assert!(v.broadcasts[0].warnings.is_empty());
        assert_eq!(v.broadcasts[0].livekit_participants, Some(1));
    }

    #[test]
    fn a_live_stream_with_no_switch_source_is_flagged() {
        let o = obs(
            reachable(vec![], vec![]),
            vec![stream_obs(STREAM_A, RoomLookup::Participants(0))],
        );
        let v = build_view(&o, &mut Trackers::default());
        assert_eq!(v.broadcasts[0].switch_source, Some(false));
        assert_eq!(v.broadcasts[0].warnings, vec![Warning::SwitchSourceMissing]);
    }

    #[test]
    fn a_failed_viewer_list_degrades_the_switch_and_hides_viewer_counts() {
        let mut o = sample();
        if let SwitchObservation::Reachable { viewers, .. } = &mut o.switch {
            *viewers = Err("switch /api/viewers answered 401 Unauthorized".into());
        }
        let v = build_view(&o, &mut Trackers::default());
        let s = server(&v, ServerKind::MmSwitch);
        assert_eq!(s.status, Some(ServerStatus::Degraded));
        assert_eq!(
            s.last_error.as_deref(),
            Some("switch /api/viewers answered 401 Unauthorized")
        );
        assert_eq!(v.broadcasts[0].switch_viewers, None);
        assert_eq!(v.broadcasts[0].switch_source, Some(true));
    }

    #[test]
    fn an_unconfigured_switch_reports_not_configured_and_no_switch_facts() {
        let o = obs(
            SwitchObservation::NotConfigured,
            vec![stream_obs(STREAM_A, RoomLookup::Failed)],
        );
        let v = build_view(&o, &mut Trackers::default());
        assert_eq!(
            server(&v, ServerKind::MmSwitch).status,
            Some(ServerStatus::NotConfigured)
        );
        assert_eq!(v.broadcasts[0].switch_source, None);
        assert!(v.broadcasts[0].warnings.is_empty());
        assert_eq!(v.capacity.as_ref().expect("capacity").viewers, None);
    }

    fn detail(v: &BroadcastServersView, kind: ServerKind) -> &ServerDetail {
        server(v, kind).detail.as_ref().expect("detail present")
    }

    #[test]
    fn a_failed_livekit_health_check_makes_participants_unknown_not_zero() {
        let mut o = sample();
        o.livekit = Err("connection refused".into());
        let v = build_view(&o, &mut Trackers::default());
        assert_eq!(
            detail(&v, ServerKind::Livekit),
            &ServerDetail::Livekit {
                participants: None,
                rooms_unavailable: 1
            }
        );
        let j = serde_json::to_value(&v).unwrap();
        assert!(j["servers"][1]["detail"]["participants"].is_null());
    }

    #[test]
    fn a_failed_stream_listing_makes_livekit_and_egress_counts_unknown() {
        let mut o = sample();
        o.streams = Err("database unavailable".into());
        let v = build_view(&o, &mut Trackers::default());
        assert_eq!(
            detail(&v, ServerKind::Livekit),
            &ServerDetail::Livekit {
                participants: None,
                rooms_unavailable: 0
            }
        );
        assert_eq!(
            detail(&v, ServerKind::LivekitEgress),
            &ServerDetail::Egress { active: None }
        );
    }

    #[test]
    fn participants_sum_the_answering_rooms_and_count_the_unavailable_ones() {
        let o = obs(
            reachable(vec![], vec![]),
            vec![
                stream_obs(STREAM_A, RoomLookup::Participants(3)),
                stream_obs(STREAM_B, RoomLookup::Failed),
            ],
        );
        let v = build_view(&o, &mut Trackers::default());
        assert_eq!(
            detail(&v, ServerKind::Livekit),
            &ServerDetail::Livekit {
                participants: Some(3),
                rooms_unavailable: 1
            }
        );
    }

    #[test]
    fn egress_total_is_unknown_when_a_stream_with_a_room_has_no_egress_listing() {
        let mut unlisted = stream_obs(STREAM_A, RoomLookup::Participants(0));
        unlisted.egresses_active = None;
        let v = build_view(
            &obs(reachable(vec![], vec![]), vec![unlisted]),
            &mut Trackers::default(),
        );
        assert_eq!(
            detail(&v, ServerKind::LivekitEgress),
            &ServerDetail::Egress { active: None }
        );

        // A stream without a room has no egress to list: it does not blank the total.
        let mut roomless = stream_obs(STREAM_B, RoomLookup::NoRoom);
        roomless.stream.sfu_room_id = None;
        roomless.egresses_active = None;
        let mut with_egress = stream_obs(STREAM_A, RoomLookup::Participants(0));
        with_egress.egresses_active = Some(2);
        let v = build_view(
            &obs(reachable(vec![], vec![]), vec![roomless, with_egress]),
            &mut Trackers::default(),
        );
        assert_eq!(
            detail(&v, ServerKind::LivekitEgress),
            &ServerDetail::Egress { active: Some(2) }
        );
    }

    #[test]
    fn an_unreachable_switch_reports_unreachable_with_its_error_and_no_switch_facts() {
        let o = obs(
            SwitchObservation::Unreachable {
                error: "connection refused".into(),
                latency_ms: 7,
            },
            vec![stream_obs(STREAM_A, RoomLookup::Participants(0))],
        );
        let v = build_view(&o, &mut Trackers::default());
        let s = server(&v, ServerKind::MmSwitch);
        assert_eq!(s.status, Some(ServerStatus::Unreachable));
        assert_eq!(s.last_error.as_deref(), Some("connection refused"));
        assert_eq!(s.latency_ms, Some(7));
        assert_eq!(s.detail, None);
        assert_eq!(v.capacity.as_ref().expect("capacity").viewers, None);
        assert_eq!(v.broadcasts[0].switch_source, None);
        assert_eq!(v.broadcasts[0].switch_viewers, None);
        assert!(v.broadcasts[0].warnings.is_empty());
    }

    #[test]
    fn a_failed_source_list_degrades_the_switch_and_raises_no_missing_source_warning() {
        let mut o = sample();
        let SwitchObservation::Reachable { sources, .. } = &mut o.switch else {
            panic!("fixture switch must be reachable");
        };
        *sources = Err("switch /api/sources answered 500".into());
        let v = build_view(&o, &mut Trackers::default());
        assert_eq!(
            server(&v, ServerKind::MmSwitch).status,
            Some(ServerStatus::Degraded)
        );
        assert_eq!(v.broadcasts[0].switch_source, None);
        assert!(
            !v.broadcasts[0]
                .warnings
                .contains(&Warning::SwitchSourceMissing)
        );
    }

    #[test]
    fn an_unreachable_livekit_is_reported_with_its_error() {
        let mut o = sample();
        o.livekit = Err("connection refused".into());
        let v = build_view(&o, &mut Trackers::default());
        let lk = server(&v, ServerKind::Livekit);
        assert_eq!(lk.status, Some(ServerStatus::Unreachable));
        assert_eq!(lk.last_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn recording_path_follows_the_egress_id() {
        let rec = |egress: Option<&str>| OpenRecording {
            egress_id: egress.map(str::to_string),
            status: "recording".into(),
        };
        let cases: [(Result<Vec<OpenRecording>, String>, RecordingPath, bool); 4] = [
            (
                Ok(vec![rec(Some("mm-switch:stream-x"))]),
                RecordingPath::Switch,
                false,
            ),
            (Ok(vec![rec(Some("EG_abc"))]), RecordingPath::Egress, true),
            (Ok(vec![]), RecordingPath::None, false),
            (Err("db down".into()), RecordingPath::Unknown, false),
        ];
        for (recordings, path, fallback_warning) in cases {
            let mut o = sample();
            o.streams.as_mut().expect("fixture streams")[0].recordings = recordings;
            let v = build_view(&o, &mut Trackers::default());
            assert_eq!(v.broadcasts[0].recording.path, path);
            assert_eq!(
                v.broadcasts[0]
                    .warnings
                    .contains(&Warning::RecordingFallback),
                fallback_warning
            );
        }
    }

    #[test]
    fn capacity_estimate_zero_means_not_measured_and_over_compares_viewers() {
        let mut o = sample(); // health.viewers = 4
        let c = build_view(&o, &mut Trackers::default())
            .capacity
            .expect("capacity");
        assert_eq!(c.estimate, None);
        assert!(!c.over);
        o.capacity_estimate = 3;
        let c = build_view(&o, &mut Trackers::default())
            .capacity
            .expect("capacity");
        assert_eq!(c.estimate, Some(3));
        assert!(c.over);
    }

    #[test]
    fn a_failed_stream_listing_is_reported_not_shown_as_no_broadcasts() {
        let mut o = sample();
        o.streams = Err("database unavailable".into());
        let v = build_view(&o, &mut Trackers::default());
        assert!(v.broadcasts.is_empty());
        assert_eq!(v.broadcasts_error.as_deref(), Some("database unavailable"));
    }

    #[test]
    fn hitting_the_stream_limit_marks_the_view_truncated() {
        let streams = (0..STREAM_LIMIT)
            .map(|i| {
                stream_obs(
                    &format!("3f2a1c9e-0000-4000-8000-{i:012}"),
                    RoomLookup::NoRoom,
                )
            })
            .collect();
        let v = build_view(
            &obs(reachable(vec![], vec![]), streams),
            &mut Trackers::default(),
        );
        assert!(v.truncated);
    }

    #[test]
    fn wire_format_is_what_the_dashboard_reads() {
        let j = serde_json::to_value(build_view(&sample(), &mut Trackers::default())).unwrap();
        assert_eq!(j["servers"][0]["kind"], "mm-switch");
        assert_eq!(j["servers"][0]["status"], "ok");
        assert_eq!(j["servers"][0]["detail"]["viewers"], 4);
        assert_eq!(j["servers"][1]["kind"], "livekit");
        // LiveKit and the stream listing answered, so the sum is known (0 from the rooms that
        // answered); the sample's only room lookup failed, which `rooms_unavailable` reports.
        assert_eq!(j["servers"][1]["detail"]["participants"], 0);
        assert_eq!(j["servers"][1]["detail"]["rooms_unavailable"], 1);
        assert_eq!(j["servers"][2]["kind"], "livekit-egress");
        assert_eq!(j["servers"][3]["status"], "not_monitored");
        assert_eq!(j["broadcasts"][0]["warnings"][0], "sweep_sees_empty");
        assert_eq!(j["broadcasts"][0]["recording"]["path"], "none");
        assert!(j["capacity"]["estimate"].is_null());
        assert_eq!(j["demo"], false);
    }

    #[test]
    fn no_viewer_id_or_viewer_matrix_id_reaches_the_view() {
        let j = serde_json::to_string(&build_view(&sample(), &mut Trackers::default())).unwrap();
        assert!(!j.contains("viewer-"), "{j}");
        assert!(
            !j.contains("viewer:leak.example") && !j.contains("-viewer-leak.example"),
            "{j}"
        );
        assert!(!j.contains("adwatcher") && !j.contains("@other:"), "{j}");
    }

    #[test]
    fn demo_has_structure_and_no_values() {
        let j = serde_json::to_value(BroadcastServersView::demo()).unwrap();
        assert_eq!(j["demo"], true);
        assert_eq!(j["servers"].as_array().unwrap().len(), 4);
        for s in j["servers"].as_array().unwrap() {
            assert!(s["kind"].is_string() && s["role"].is_string());
            for field in [
                "status",
                "last_ok_at",
                "consecutive_failures",
                "latency_ms",
                "last_error",
                "detail",
            ] {
                assert!(s[field].is_null(), "demo {field} must be null: {s}");
            }
        }
        assert!(j["capacity"].is_null() && j["collected_at"].is_null());
        assert_eq!(j["broadcasts"], serde_json::json!([]));
    }

    #[test]
    fn collecting_is_not_demo_and_has_no_snapshot_time() {
        let v = BroadcastServersView::collecting();
        assert!(!v.demo);
        assert!(v.collected_at.is_none());
    }
}

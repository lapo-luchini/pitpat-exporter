// SPDX-License-Identifier: ISC

//! Daemon state machine (§4) plus the per-connection pump that feeds it.

use crate::link::Notify;
use crate::metrics::Metrics;
use crate::protocol as p;
use crate::session::{DeltaTracker, EndReason, Totals};
use jiff::Timestamp;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// Treadmill per-session counters (distance/steps/energy/moving time).
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionCounters {
    pub distance_m: u32,
    pub steps: u32,
    pub kcal: u16,
    pub duration_s: f64,
}

/// Belt-agnostic states (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Stopped,
    Starting,
    Running,
    Paused,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Disconnected => "disconnected",
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Running => "running",
            State::Paused => "paused",
        }
    }
}

impl From<p::BeltState> for State {
    fn from(b: p::BeltState) -> Self {
        match b {
            p::BeltState::Stopped => State::Stopped,
            p::BeltState::Starting => State::Starting,
            p::BeltState::Running => State::Running,
            p::BeltState::Paused => State::Paused,
        }
    }
}

/// Wire-level command (§3.5); the speed only means anything for
/// start/set-speed, pause/stop resolve against the live target speed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WireCmd {
    Start(f64),
    SetSpeed(f64),
    Pause,
    Stop,
}

impl WireCmd {
    /// Encode, resolving pause/stop speed per §3.3: the current target
    /// speed, clamped (any valid value works; the reference sends 1000).
    fn packet(&self, target_speed_kmh: f64, cfg: &crate::config::DaemonCfg) -> Vec<u8> {
        let pause_speed = target_speed_kmh.clamp(cfg.min_speed, cfg.max_speed);
        let out =
            |c: p::Command, speed| p::encode_command(c, speed, cfg.weight_kg, cfg.user_id).to_vec();
        match *self {
            WireCmd::Start(s) => out(p::Command::Start, s),
            WireCmd::SetSpeed(s) => out(p::Command::SetSpeed, s),
            WireCmd::Pause => out(p::Command::Pause, pause_speed),
            WireCmd::Stop => out(p::Command::Stop, pause_speed),
        }
    }
}

/// Validated client command from the HTTP/CLI side.
#[derive(Debug, Clone, Copy)]
pub enum ApiCmd {
    /// `None` = use the remembered target speed.
    Start(Option<f64>),
    Pause,
    Stop,
    SpeedAbs(f64),
    SpeedDelta(f64),
    Preset(usize),
    TargetSet(Option<u32>, Option<f64>),
    TargetClear,
}

/// Rejection of a client command (mapping in http.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErr {
    /// 409: not connected.
    Disabled,
    /// 400: bad request.
    BadRequest(&'static str),
    /// 503: command queue full.
    Full,
}

/// Auto-stop target (§4.5); memory-only, cleared when triggered.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Target {
    pub distance_m: Option<u32>,
    pub duration_s: Option<f64>,
}

/// Open session (§4.3), mirroring the treadmill's per-session counters.
#[derive(Debug, Clone)]
pub struct OpenSession {
    pub start: Timestamp,
    pub distance_m: u32,
    pub steps: u32,
    pub kcal: u16,
    pub duration_s: f64,
    pub max_speed_kmh: f64,
}

/// View for HTTP and SSE consumers.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub connected: bool,
    pub address: Option<String>,
    pub state: State,
    pub imperial: bool,
    pub speed_kmh: f64,
    pub target_speed_kmh: f64,
    pub queue_len: usize,
    pub session: Option<OpenSession>,
    pub target: Target,
    /// Why the last session ended (empty while a session is open); lets
    /// remote clients detect auto-stop transitions (§10 widgets).
    pub last_end_reason: Option<&'static str>,
    /// Lifetime totals ride along for the state endpoint.
    pub totals: Totals,
}

/// Why a connection died (drives the session end reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    Link,
    Watchdog,
    Shutdown,
}

/// Control messages from signal tasks into the pump.
#[derive(Debug, Clone, Copy)]
pub enum Ctrl {
    /// SIGTERM/SIGINT: shut down cleanly (§4.6). The simulator's link drop
    /// is handled via its shared flag, not through the pump.
    Shutdown,
}

pub const QUEUE_CAP: usize = 8;
/// No notification this long ⇒ the link is effectively dead (§4.1).
pub const WATCHDOG_S: u64 = 10;
/// Periodic totals.json flush while running (§4.4).
pub const PERSIST_S: u64 = 60;

pub struct Core {
    pub cfg: crate::config::DaemonCfg,
    pub metrics: Arc<Metrics>,
    pub totals: Totals,
    deltas: DeltaTracker,
    pub state: State,
    /// Open a session on the next running packet (first running after
    /// stopped/disconnected, §4.3); paused/starting never open one.
    may_open: bool,
    imperial: bool,
    pub speed_kmh: f64,
    pub target_speed_kmh: f64,
    address: Option<String>,
    session: Option<OpenSession>,
    /// Accumulated pre-reset counters for the open session (§4.3): when
    /// the treadmill resets its counters mid-session, what was walked
    /// before the reset stays in the record (REVIEW item 7).
    session_offset: SessionCounters,
    /// End reason of the most recently closed session (for the API view).
    last_end_reason: Option<&'static str>,
    pub target: Target,
    queue: VecDeque<WireCmd>,
    /// Packet awaiting retry after a failed write (§3.5).
    pending_write: Option<Vec<u8>>,
    write_retries: u8,
    /// Last two reported belt speeds while running (ramp detection).
    settle: [f64; 2],
    inbox: mpsc::Receiver<ApiCmd>,
    events: broadcast::Sender<Snapshot>,
    state_dir: std::path::PathBuf,
}

impl Core {
    pub fn new(
        cfg: crate::config::DaemonCfg,
        totals: Totals,
        metrics: Arc<Metrics>,
        events: broadcast::Sender<Snapshot>,
        inbox: mpsc::Receiver<ApiCmd>,
        state_dir: std::path::PathBuf,
    ) -> Self {
        let target_speed_kmh = cfg.default_speed;
        Self {
            cfg,
            metrics,
            totals,
            deltas: DeltaTracker::new(),
            state: State::Disconnected,
            may_open: true,
            imperial: false,
            speed_kmh: 0.0,
            target_speed_kmh,
            address: None,
            session: None,
            session_offset: SessionCounters::default(),
            last_end_reason: None,
            target: Target::default(),
            queue: VecDeque::new(),
            pending_write: None,
            write_retries: 0,
            settle: [f64::NAN; 2],
            inbox,
            events,
            state_dir,
        }
    }

    // ------------------------------------------------------------- views

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            connected: self.state != State::Disconnected,
            address: self.address.clone(),
            state: self.state,
            imperial: self.imperial,
            speed_kmh: self.speed_kmh,
            target_speed_kmh: self.target_speed_kmh,
            queue_len: self.queue.len(),
            session: self.session.clone(),
            target: self.target,
            last_end_reason: self.last_end_reason,
            totals: self.totals.clone(),
        }
    }

    /// Publish into metrics + the SSE channel.
    fn publish(&mut self) {
        let snap = self.snapshot();
        let m = &self.metrics;
        m.set_state(snap.state.as_str());
        m.connected.set(i64::from(snap.connected));
        m.speed_mps.set(snap.speed_kmh / 3.6);
        m.target_speed_mps.set(snap.target_speed_kmh / 3.6);
        if let Some(s) = &snap.session {
            m.session_distance_m.set(i64::from(s.distance_m));
            m.session_steps.set(i64::from(s.steps));
            m.session_kcal.set(i64::from(s.kcal));
            m.session_duration_s.set(s.duration_s);
        } else {
            m.session_distance_m.set(0);
            m.session_steps.set(0);
            m.session_kcal.set(0);
            m.session_duration_s.set(0.0);
        }
        let _ = self.events.send(snap); // nobody listening is normal
    }

    pub fn persist(&self) {
        if let Err(err) = crate::session::save_totals(&self.state_dir, &self.totals) {
            tracing::error!(error = %err, "persisting totals.json failed");
        }
    }

    /// Belt still turning or capable of turning? (drives SIGTERM, §4.6)
    pub fn moving(&self) -> bool {
        matches!(self.state, State::Running | State::Paused)
    }

    /// SIGTERM while moving (§4.6): end the session as `shutdown` BEFORE
    /// queueing the stop — the treadmill's post-reset stop packet would
    /// otherwise close it as `stopped` first (REVIEW item 7).
    pub fn begin_shutdown(&mut self) {
        if self.moving() {
            self.close_session(EndReason::Shutdown);
        }
        self.queue.clear();
        self.pending_write = None;
        self.queue.push_back(WireCmd::Stop);
    }

    // -------------------------------------------------------- API entry

    /// Queue/apply a client command; `Err` maps to HTTP 400/409/503 (§6.2).
    pub fn queue_api(&mut self, cmd: ApiCmd) -> Result<(), ApiErr> {
        match cmd {
            ApiCmd::TargetSet(_, _) | ApiCmd::TargetClear => {
                // targets apply to the current or next session, so this is
                // allowed even while disconnected
                self.target = match cmd {
                    ApiCmd::TargetSet(d, s) => {
                        if d.is_some_and(|v| v > 100_000_000)
                            || s.is_some_and(|v| {
                                !v.is_finite() || !(0.0..=100_000_000.0).contains(&v)
                            })
                        {
                            return Err(ApiErr::BadRequest("target value out of range"));
                        }
                        Target {
                            distance_m: d,
                            duration_s: s,
                        }
                    }
                    _ => Target::default(),
                };
                Ok(())
            }
            _ if self.state == State::Disconnected => Err(ApiErr::Disabled),
            ApiCmd::Start(s) => {
                let speed = self.clamp(s.unwrap_or(self.target_speed_kmh));
                self.target_speed_kmh = speed;
                self.forget_settle();
                if self.state == State::Running {
                    // an explicit speed while moving means set-speed
                    self.queue_set_speed(speed)
                } else {
                    self.queue_back(WireCmd::Start(speed))
                }
            }
            ApiCmd::Pause => {
                self.forget_settle();
                self.queue_back(WireCmd::Pause)
            }
            ApiCmd::Stop => {
                // stop must never wait behind anything (§3.5)
                self.queue.clear();
                self.pending_write = None;
                self.forget_settle();
                self.queue.push_back(WireCmd::Stop);
                Ok(())
            }
            ApiCmd::SpeedAbs(v) => {
                let v = self.clamp(v);
                self.target_speed_kmh = v;
                self.forget_settle();
                if self.moving() {
                    self.queue_set_speed(v)?;
                }
                Ok(())
            }
            ApiCmd::SpeedDelta(d) => {
                let v = self.clamp(self.target_speed_kmh + d);
                self.target_speed_kmh = v;
                self.forget_settle();
                if self.moving() {
                    self.queue_set_speed(v)?;
                }
                Ok(())
            }
            ApiCmd::Preset(i) => {
                let Some(v) = self.cfg.presets.get(i).copied() else {
                    return Err(ApiErr::BadRequest("no such preset"));
                };
                self.queue_api(ApiCmd::SpeedAbs(v))
            }
        }
    }

    /// Consecutive set-speeds coalesce: the newer replaces the queued one.
    fn queue_set_speed(&mut self, v: f64) -> Result<(), ApiErr> {
        if let Some(WireCmd::SetSpeed(s)) = self.queue.back_mut() {
            // coalescing never grows the queue
            *s = v;
            return Ok(());
        }
        self.queue_back(WireCmd::SetSpeed(v))
    }

    fn queue_back(&mut self, c: WireCmd) -> Result<(), ApiErr> {
        if self.queue.len() >= QUEUE_CAP {
            tracing::warn!("command queue full, rejecting");
            return Err(ApiErr::Full);
        }
        self.queue.push_back(c);
        Ok(())
    }

    fn clamp(&self, kmh: f64) -> f64 {
        if !kmh.is_finite() {
            return self.cfg.default_speed;
        }
        kmh.clamp(self.cfg.min_speed, self.cfg.max_speed)
    }

    fn forget_settle(&mut self) {
        self.settle = [f64::NAN; 2];
    }

    /// Commands are sent only within the notification cycle (§3.1), so
    /// client commands that arrived meanwhile are drained before a reply.
    pub fn drain_api_cmds(&mut self) {
        while let Ok(cmd) = self.inbox.try_recv() {
            self.queue_api(cmd).ok();
        }
    }

    // ---------------------------------------------------- status packets

    /// One raw notification (§3.2). Undecodable packets: counted, logged,
    /// no state change; the caller still replies with cmd/heartbeat.
    pub fn on_packet(&mut self, raw: &[u8]) {
        self.metrics.last_notification.set(unix_now());
        let Some(st) = p::decode_status(raw) else {
            self.metrics.invalid_packets_total.inc_by(1);
            tracing::debug!(pkt_len = raw.len(), "undecodable status notification");
            return;
        };

        self.imperial = st.imperial();
        self.speed_kmh = st.speed_mps * 3.6;

        // lifetime totals via deltas (§4.4)
        if let Some(d) = self.deltas.apply(crate::session::Counters {
            distance_m: st.distance_m,
            steps: st.steps,
            kcal: st.kcal,
            duration_s: st.duration_s,
        }) {
            self.totals.distance_m += u64::from(d.distance_m);
            self.totals.steps += u64::from(d.steps);
            self.totals.kcal += u64::from(d.kcal);
            self.totals.moving_s += d.moving_s;
            self.metrics
                .bump_totals(d.distance_m, d.steps, u32::from(d.kcal), d.moving_s);
        }

        let belt = State::from(st.belt_state());
        self.begin_session_if_needed(belt);
        if belt != State::Stopped {
            // a Stopped packet carries the treadmill's post-reset counters
            // (back to 0 on stop): keep the session's last values instead
            self.update_running_session(&st);
        }
        self.sync_target_speed(belt);
        self.state = belt;
        match belt {
            State::Stopped | State::Disconnected => self.may_open = true,
            _ => {}
        }
        if self.state == State::Stopped {
            self.close_session(EndReason::Stopped);
        }
        self.check_targets();
        self.publish();
    }

    /// A session starts on the first running after stopped/disconnected.
    fn begin_session_if_needed(&mut self, belt: State) {
        if belt != State::Running || !self.may_open {
            return;
        }
        self.may_open = false;
        self.session = Some(OpenSession {
            start: now_ts(),
            distance_m: 0,
            steps: 0,
            kcal: 0,
            duration_s: 0.0,
            max_speed_kmh: self.speed_kmh,
        });
        self.session_offset = SessionCounters::default();
        self.last_end_reason = None;
        tracing::info!("session started");
    }

    /// Mirror the treadmill's session counters; when it resets them
    /// mid-session, what was walked before the reset is kept as an
    /// offset and added back into the record at the end (§4.3).
    fn update_running_session(&mut self, st: &p::Status) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        if st.distance_m < s.distance_m || st.steps < s.steps {
            self.session_offset.distance_m += s.distance_m;
            self.session_offset.steps += s.steps;
            self.session_offset.kcal += s.kcal;
            self.session_offset.duration_s += s.duration_s;
        }
        s.distance_m = st.distance_m;
        s.steps = st.steps;
        s.kcal = st.kcal;
        s.duration_s = st.duration_s;
        s.max_speed_kmh = s.max_speed_kmh.max(self.speed_kmh);
    }

    /// While running and nothing queued: remember reported speeds; once two
    /// consecutive reports agree, sync the remembered target (§3.6) so a
    /// physical-remote change is picked up. Ramps never sync.
    fn sync_target_speed(&mut self, belt: State) {
        if belt != State::Running || !self.queue.is_empty() {
            self.forget_settle();
            return;
        }
        if eq_speed(self.speed_kmh, self.settle[0]) && eq_speed(self.speed_kmh, self.settle[1]) {
            if !eq_speed(self.speed_kmh, self.target_speed_kmh) {
                tracing::info!(
                    speed_kmh = self.speed_kmh,
                    "belt settled elsewhere; syncing target"
                );
                self.target_speed_kmh = self.speed_kmh;
            }
        } else {
            self.settle = [self.settle[1], self.speed_kmh];
        }
    }

    /// Auto-stop targets (§4.5): queue stop, close the session as
    /// `target_reached`, clear the target.
    fn check_targets(&mut self) {
        let Some(s) = &self.session else { return };
        let (dist, dur) = (s.distance_m, s.duration_s);
        let hit_d = self.target.distance_m.is_some_and(|d| dist >= d);
        let hit_t = self.target.duration_s.is_some_and(|d| dur >= d);
        if !hit_d && !hit_t {
            return;
        }
        tracing::info!(
            distance_m = dist,
            duration_s = dur,
            "target reached, stopping"
        );
        self.target = Target::default();
        self.queue.clear();
        self.pending_write = None;
        self.queue.push_back(WireCmd::Stop);
        self.close_session(EndReason::TargetReached);
    }

    /// End the open session (any reason) and append it, unless noise (§4.3).
    fn close_session(&mut self, reason: EndReason) {
        let Some(open) = self.session.clone() else {
            // no session open: do not overwrite the last real end reason
            // (a treadmill stopped-packet arrives on every disconnect too,
            // REVIEW round-2 A1)
            return;
        };
        self.session = None;
        self.last_end_reason = Some(reason.as_str());
        // record = pre-reset offset + the values mirrored at close
        let (dist, steps, kcal, dur, max) = (
            self.session_offset.distance_m + open.distance_m,
            self.session_offset.steps + open.steps,
            self.session_offset.kcal + open.kcal,
            self.session_offset.duration_s + open.duration_s,
            open.max_speed_kmh,
        );
        let end = now_ts();
        let avg = if dur > 0.0 {
            dist as f64 / dur * 3.6
        } else {
            0.0
        };
        let rec = crate::session::SessionRec {
            start: open.start,
            end,
            distance_m: dist,
            steps,
            kcal,
            duration_s: dur,
            avg_speed_kmh: avg,
            max_speed_kmh: max,
            end_reason: reason.as_str().into(),
        };
        tracing::info!(
            start = %rec.start,
            end = %rec.end,
            distance_m = rec.distance_m,
            steps = rec.steps,
            kcal = rec.kcal,
            duration_s = rec.duration_s,
            end_reason = rec.end_reason,
            "session ended"
        );
        if crate::session::is_noise(&rec) {
            tracing::debug!("session under 10 s and under 10 m: discarded as noise");
            return;
        }
        self.totals.sessions += 1;
        self.metrics.sessions_total.inc();
        if let Err(err) = crate::session::append_session(&self.state_dir, &rec) {
            tracing::error!(error = %err, "appending session record failed");
        }
        self.persist();
    }

    /// The connection died (§4.1): disconnected, end the open session,
    /// forget the delta baseline, keep aligned state.
    pub fn on_disconnect(&mut self, reason: DeadReason) {
        self.state = State::Disconnected;
        self.may_open = true;
        self.deltas.reset();
        self.queue.clear();
        self.pending_write = None;
        self.forget_settle();
        let end_reason = match reason {
            DeadReason::Link | DeadReason::Watchdog => EndReason::Disconnected,
            DeadReason::Shutdown => EndReason::Shutdown,
        };
        self.close_session(end_reason);
        self.persist();
        self.address = None;
        self.publish();
    }

    /// Fresh connection: the next notification becomes the deltas baseline.
    pub fn begin_connection(&mut self, address: Option<String>) {
        self.address = address;
        self.deltas.reset();
        // first running after the (re)connect opens a session (§4.3)
        self.may_open = true;
        self.forget_settle();
        self.publish();
    }

    // ------------------------------------------- heartbeat / reply side

    /// Packet for the next notification: the retry-candidate, else the next
    /// queued command, else the heartbeat (§3.1).
    pub fn next_reply(&mut self) -> Vec<u8> {
        if let Some(pk) = self.pending_write.take() {
            return pk;
        }
        match self.queue.pop_front() {
            Some(c) => c.packet(self.target_speed_kmh, &self.cfg),
            None => p::HEARTBEAT.to_vec(),
        }
    }

    pub fn write_succeeded(&mut self) {
        self.write_retries = 0;
    }

    /// Write failed: retry the same packet on the next notification, at
    /// most 3 times, then drop it with a warning (§3.5).
    pub fn write_failed(&mut self, pkt: Vec<u8>) {
        self.write_retries += 1;
        if self.write_retries > 3 {
            tracing::warn!("dropping command after 3 failed writes");
            self.write_retries = 0;
        } else {
            self.pending_write = Some(pkt);
        }
    }
}

// --------------------------------------------------------------- helpers

fn eq_speed(a: f64, b: f64) -> bool {
    a.is_finite() && b.is_finite() && (a - b).abs() < 1e-9
}

fn now_ts() -> Timestamp {
    jiff::Timestamp::now()
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

// ------------------------------------------------------------------ pump

/// One reply packet per notification: next queued command or heartbeat,
/// with the same-packet retry rule on write errors (§3.5).
async fn write_reply<C: Notify>(core: &mut Core, conn: &mut C) {
    let pkt = core.next_reply();
    if conn.write_packet(&pkt).await.is_ok() {
        core.write_succeeded();
        return;
    }
    if pkt.as_slice() != p::HEARTBEAT {
        core.write_failed(pkt);
    }
}

/// Drive one connection until it dies: apply notifications, send exactly
/// one packet per notification (§3.1), enforce the 10 s liveness watchdog
/// (§4.1), flush totals every 60 s (§4.4). The caller reconnects (or
/// shuts down) upon return.
pub async fn pump<C: Notify>(
    core: &mut Core,
    conn: &mut C,
    ctrl: &mut mpsc::Receiver<Ctrl>,
) -> DeadReason {
    // The notification future is recreated on every loop pass; this is safe
    // ONLY because no other select arm can complete while it is pending —
    // an earlier two-price sleep arm caused the recreated timer to restart
    // and starve notifications forever.
    let mut next_persist = tokio::time::Instant::now() + Duration::from_secs(PERSIST_S);
    loop {
        core.drain_api_cmds();

        tokio::select! {
            ev = tokio::time::timeout(Duration::from_secs(WATCHDOG_S), conn.recv_notification()) => {
                let raw = match ev {
                    Ok(Ok(raw)) => raw,
                    Ok(Err(_)) => return DeadReason::Link,
                    Err(_) => {
                        tracing::warn!(secs = WATCHDOG_S, "no notification, forcing reconnect");
                        return DeadReason::Watchdog;
                    }
                };
                core.on_packet(&raw);
                core.drain_api_cmds();
                write_reply(core, conn).await;
                if core.moving() && tokio::time::Instant::now() >= next_persist {
                    next_persist = tokio::time::Instant::now() + Duration::from_secs(PERSIST_S);
                    core.persist();
                }
            }
            c = ctrl.recv() => match c {
                Some(Ctrl::Shutdown) => return DeadReason::Shutdown,
                None => return DeadReason::Link,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonCfg;
    use crate::link::LinkError;
    use crate::protocol as p;
    use crate::session::Totals;

    fn cfg() -> DaemonCfg {
        DaemonCfg::from(&clap::Parser::parse_from(["pitpat"]))
    }

    fn st(belt: p::BeltState, speed: f64, dist: u32, steps: u32, kcal: u16, dur: f64) -> Vec<u8> {
        p::encode_status(belt, false, speed, dist, steps, kcal, dur).to_vec()
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pitpat-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    struct T {
        core: Core,
    }

    fn mkcore(dir: &std::path::Path) -> T {
        let (events, _ev_rx) = tokio::sync::broadcast::channel(8);
        let (_, api_rx) = mpsc::channel(32);
        let core = Core::new(
            cfg(),
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir.into(),
        );
        T { core }
    }

    const USER: u64 = 58965456623;

    // ------------------------------------------------------------ queue

    #[test]
    fn queue_coalesces_and_stop_jumps_front() {
        let mut t = mkcore(&tmpdir("queue"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 10, 20, 1, 12.0));
        assert!(c.queue.is_empty());

        // consecutive set-speeds coalesce, newest wins
        c.queue_api(ApiCmd::SpeedAbs(3.0)).ok();
        c.queue_api(ApiCmd::SpeedAbs(3.2)).ok();
        assert_eq!(c.queue, VecDeque::from([WireCmd::SetSpeed(3.2)]));
        assert_eq!(
            c.next_reply(),
            p::encode_command(p::Command::SetSpeed, 3.2, 80, USER).to_vec()
        );
        assert!(c.queue.is_empty());

        // pause interleaves (no coalescing across kinds)
        c.queue_api(ApiCmd::SpeedAbs(2.6)).ok();
        c.queue_api(ApiCmd::Pause).ok();
        assert_eq!(
            c.queue,
            VecDeque::from([WireCmd::SetSpeed(2.6), WireCmd::Pause])
        );

        // stop clears the queue and goes to the front
        c.queue_api(ApiCmd::Stop).ok();
        assert_eq!(c.queue, VecDeque::from([WireCmd::Stop]));
        let pk = c.next_reply();
        assert_eq!(pk.len(), 23);
        assert_eq!(pk[12], 0x00, "stop command byte");
        assert_eq!(pk[22], 0x43);
    }

    #[test]
    fn start_speed_uses_requested_value() {
        let mut t = mkcore(&tmpdir("start"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.queue_api(ApiCmd::Start(Some(2.5))).ok();
        assert_eq!(
            c.next_reply(),
            p::encode_command(p::Command::Start, 2.5, 80, USER).to_vec()
        );
    }

    #[test]
    fn stop_packet_carries_clamped_target_speed() {
        let mut t = mkcore(&tmpdir("stopspeed"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 3.2, 0, 0, 0, 0.0));
        // one running report is not "settled": target still 2.0
        c.queue_api(ApiCmd::Stop).ok();
        let pk = c.next_reply();
        assert_eq!(&pk[6..8], &2000u16.to_be_bytes(), "target speed 2.0 km/h");
    }

    #[test]
    fn queue_rejects_when_full() {
        let mut t = mkcore(&tmpdir("cap"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0));

        // pause/set pairs never coalesce
        let mut i = 0;
        while i < QUEUE_CAP {
            if c.queue_api(ApiCmd::Pause).is_err() {
                break;
            }
            i += 1;
            if c.queue_api(ApiCmd::SpeedAbs(1.0 + i as f64)).is_err() {
                break;
            }
            i += 1;
        }
        assert_eq!(c.queue.len(), QUEUE_CAP, "queue at capacity");
        assert_eq!(c.queue_api(ApiCmd::Pause), Err(ApiErr::Full));
    }

    #[test]
    fn write_retry_then_drop() {
        let mut t = mkcore(&tmpdir("retry"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0));

        c.queue_api(ApiCmd::Pause).ok();
        let pk = c.next_reply();
        assert_eq!(pk[12], 0x02);
        assert_eq!(c.next_reply(), p::HEARTBEAT.to_vec(), "nothing pending");
        // 3 retries of the same packet
        for _ in 0..3 {
            c.write_failed(pk.clone());
            assert_eq!(c.next_reply(), pk, "retry the same packet");
        }
        // 4th failure drops it
        c.write_failed(pk);
        assert_eq!(c.next_reply(), p::HEARTBEAT.to_vec());
    }

    #[test]
    fn commands_rejected_while_disconnected() {
        let mut t = mkcore(&tmpdir("disc"));
        let c = &mut t.core;
        assert_eq!(c.queue_api(ApiCmd::Start(Some(2.5))), Err(ApiErr::Disabled));
        assert_eq!(c.queue_api(ApiCmd::Pause), Err(ApiErr::Disabled));
        assert_eq!(c.queue_api(ApiCmd::Stop), Err(ApiErr::Disabled));
        // targets work for the next session
        assert_eq!(c.queue_api(ApiCmd::TargetSet(Some(500), None)), Ok(()));
        assert_eq!(
            c.queue_api(ApiCmd::Preset(4)),
            Err(ApiErr::Disabled),
            "disconnected first"
        );
    }

    // ------------------------------------------------------- sessions

    #[test]
    fn pause_does_not_split_a_session() {
        let dir = tmpdir("sess");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 10, 14, 1, 10.0));
        assert!(c.session.is_some());
        c.on_packet(&st(p::BeltState::Paused, 0.0, 10, 14, 1, 10.0));
        assert!(c.session.is_some(), "paused must not end the session");
        c.on_packet(&st(p::BeltState::Running, 2.0, 500, 700, 30, 800.0));
        assert!(c.session.is_some(), "resume is the same session");
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        assert!(c.session.is_none());
        assert_eq!(c.totals.sessions, 1);
        let recs = crate::session::read_sessions(&dir, 10);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].end_reason, "stopped");
        assert_eq!(
            recs[0].distance_m, 500,
            "values from before the counter reset"
        );
        assert_eq!(recs[0].duration_s, 800.0);
        assert!((recs[0].avg_speed_kmh - 500.0 / 800.0 * 3.6).abs() < 1e-6);
    }

    #[test]
    fn short_sessions_are_noise() {
        let dir = tmpdir("noise");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 5, 7, 0, 5.0));
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        assert_eq!(c.totals.sessions, 0, "noise discarded");
        assert!(crate::session::read_sessions(&dir, 10).is_empty());
        assert!(c.totals.distance_m >= 5, "lifetime deltas still counted");
    }

    #[test]
    fn disconnect_ends_the_session() {
        let dir = tmpdir("discsess");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 100, 140, 5, 100.0));
        c.on_disconnect(DeadReason::Link);
        assert_eq!(c.state, State::Disconnected);
        let recs = crate::session::read_sessions(&dir, 10);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].end_reason, "disconnected");
        assert_eq!(recs[0].distance_m, 100);
    }

    // -------------------------------------------------------- targets

    #[test]
    fn target_distance_autostop() {
        let dir = tmpdir("target");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 5, 10, 1, 10.0));
        assert_eq!(c.queue_api(ApiCmd::TargetSet(Some(40), None)), Ok(()));
        assert_eq!(c.snapshot().target.distance_m, Some(40));
        c.on_packet(&st(p::BeltState::Running, 2.0, 50, 80, 5, 100.0));
        assert_eq!(c.snapshot().target, Target::default(), "cleared on trigger");
        assert_eq!(c.queue, VecDeque::from([WireCmd::Stop]), "stop queued");
        let recs = crate::session::read_sessions(&dir, 10);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].end_reason, "target_reached");
        assert_eq!(recs[0].distance_m, 50);
        // a later stop notification must not ghost a second session
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        assert_eq!(crate::session::read_sessions(&dir, 10).len(), 1);
        // the end reason stays target_reached no matter how many stopped
        // packets follow (REVIEW round-2 A1): the widget's 1 s poll must
        // not miss it
        assert_eq!(c.snapshot().last_end_reason, Some("target_reached"));
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        assert_eq!(c.snapshot().last_end_reason, Some("target_reached"));
        // a fresh session clears it again (contract: empty while open)
        c.on_packet(&st(p::BeltState::Running, 2.0, 1, 2, 0, 1.0));
        assert_eq!(c.snapshot().last_end_reason, None);
        c.on_disconnect(DeadReason::Link);
        assert_eq!(c.snapshot().last_end_reason, Some("disconnected"));
    }

    #[test]
    fn target_duration_autostop() {
        let dir = tmpdir("targett");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 0, 0, 0, 1780.0));
        assert_eq!(c.queue_api(ApiCmd::TargetSet(None, Some(1800.0))), Ok(()));
        c.on_packet(&st(p::BeltState::Running, 2.0, 100, 200, 6, 1800.0));
        assert_eq!(c.queue, VecDeque::from([WireCmd::Stop]));
        assert!(c.session.is_none(), "session closed as target_reached");
        let recs = crate::session::read_sessions(&dir, 10);
        assert_eq!(recs[0].end_reason, "target_reached");
    }

    // ------------------------------------------------- settle / totals

    #[test]
    fn settled_speed_syncs_to_physical_remote() {
        let mut t = mkcore(&tmpdir("settle"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0));
        assert!((c.target_speed_kmh - 2.0).abs() < 1e-9);

        // the physical remote moved to 2.8; equal reports settle it
        c.on_packet(&st(p::BeltState::Running, 2.8, 1, 2, 0, 1.0));
        assert!((c.target_speed_kmh - 2.0).abs() < 1e-9, "not yet settled");
        c.on_packet(&st(p::BeltState::Running, 2.8, 1, 2, 0, 1.5));
        assert!((c.target_speed_kmh - 2.0).abs() < 1e-9, "only base stored");
        c.on_packet(&st(p::BeltState::Running, 2.8, 2, 3, 0, 2.0));
        assert!((c.target_speed_kmh - 2.8).abs() < 1e-9, "synced");

        // a ramp (changing reports) never syncs
        c.target_speed_kmh = 4.0;
        c.on_packet(&st(p::BeltState::Running, 3.0, 2, 3, 0, 2.5));
        c.on_packet(&st(p::BeltState::Running, 3.1, 3, 4, 0, 3.0));
        c.on_packet(&st(p::BeltState::Running, 3.2, 4, 5, 0, 3.5));
        assert!((c.target_speed_kmh - 4.0).abs() < 1e-9);

        // queued commands block the sync (need the SetSpeed to land first)
        c.forget_settle();
        c.target_speed_kmh = 5.0;
        c.queue_api(ApiCmd::SpeedAbs(5.0)).ok();
        c.on_packet(&st(p::BeltState::Running, 3.2, 4, 5, 0, 3.5));
        assert!((c.target_speed_kmh - 5.0).abs() < 1e-9);
        assert_eq!(c.queue.back(), Some(&WireCmd::SetSpeed(5.0)));
    }

    #[test]
    fn lifetime_totals_track_deltas() {
        let mut t = mkcore(&tmpdir("lifet"));
        let c = &mut t.core;
        c.begin_connection(None);
        // first packet after connecting = baseline
        c.on_packet(&st(p::BeltState::Running, 2.0, 100, 140, 10, 120.0));
        assert_eq!(c.totals.distance_m, 0, "baseline does not count");
        c.on_packet(&st(p::BeltState::Running, 2.0, 107, 146, 11, 124.0));
        assert_eq!(c.totals.distance_m, 7);
        assert_eq!(c.totals.steps, 6);
        assert_eq!(c.totals.kcal, 1);
        assert_eq!(c.totals.moving_s, 4.0);
        // counter reset counts from zero
        c.on_packet(&st(p::BeltState::Running, 2.0, 3, 4, 1, 3.0));
        assert_eq!(c.totals.distance_m, 10);
        // absurd jump rejected
        c.on_packet(&st(p::BeltState::Running, 2.0, 100, 400, 1, 3.0));
        assert_eq!(c.totals.distance_m, 10, "no absurd jump");
        assert_eq!(c.metrics.distance_m_total.get(), 10, "metrics mirrored");
    }

    #[test]
    fn reconnect_baseline_is_reset() {
        let mut t = mkcore(&tmpdir("base"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 100, 140, 10, 120.0));
        c.on_packet(&st(p::BeltState::Running, 2.0, 103, 141, 10, 121.0));
        assert_eq!(c.totals.distance_m, 3);
        c.on_disconnect(DeadReason::Link);
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 200, 250, 20, 300.0));
        assert_eq!(
            c.totals.distance_m, 3,
            "continuation baseline not double-counted"
        );
        c.on_packet(&st(p::BeltState::Running, 2.0, 203, 251, 21, 301.0));
        assert_eq!(c.totals.distance_m, 6);
    }

    #[test]
    fn imperial_is_normalised() {
        let mut t = mkcore(&tmpdir("imp"));
        let c = &mut t.core;
        c.begin_connection(None);
        let pk = p::encode_status(
            p::BeltState::Running,
            true,
            2.0 * 1.609344,
            1609,
            100,
            3,
            30.0,
        );
        c.on_packet(pk.as_ref());
        assert!(c.snapshot().imperial);
        assert!((c.speed_kmh - 3.218688).abs() < 0.01, "mph → m/s → km/h");
        let s = c.snapshot().session.expect("session open");
        assert!((s.distance_m as f64 - 1609.344).abs() < 3.0, "mi → m");
    }

    // ----------------------------------------------------------- pump

    struct FakeConn {
        script: VecDeque<Vec<u8>>,
        pub written: Vec<Vec<u8>>,
    }

    impl crate::link::Notify for FakeConn {
        async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
            match self.script.pop_front() {
                Some(v) => Ok(v),
                None => Err(LinkError::Dead("script exhausted".into())),
            }
        }

        async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
            self.written.push(pkt.to_vec());
            Ok(())
        }
    }

    #[tokio::test]
    async fn pump_writes_reply_per_notification() {
        let dir = tmpdir("pump");
        let t = mkcore(&dir);
        let (_ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(4); // no control traffic
        let mut conn = FakeConn {
            script: VecDeque::new(),
            written: vec![],
        };

        let mut core = t.core;
        let core = &mut core;
        core.begin_connection(Some("AA:BB".into()));

        // phase 1: first notification, replies may only be a heartbeat
        conn.script
            .push_back(st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        let _ = pump(core, &mut conn, &mut ctrl_rx).await;
        assert_eq!(conn.written, [p::HEARTBEAT.to_vec()]);

        // phase 2: now connected; the start command goes with the next one
        core.queue_api(ApiCmd::Start(Some(2.5))).ok();
        conn.script
            .push_back(st(p::BeltState::Running, 2.0, 10, 14, 0, 10.0));
        conn.script
            .push_back(st(p::BeltState::Running, 2.0, 20, 28, 1, 20.0));
        let reason = pump(core, &mut conn, &mut ctrl_rx).await;
        assert_eq!(reason, DeadReason::Link);

        // one reply per notification; the first is the queued start command
        assert_eq!(conn.written.len(), 3);
        assert_eq!(conn.written[1][8], 0x01, "start kind");
        assert_eq!(
            &conn.written[1][6..8],
            &(2500u16).to_be_bytes(),
            "speed 2.5 km/h"
        );
        assert_eq!(conn.written[2], p::HEARTBEAT.to_vec());

        core.on_disconnect(reason);
        assert!(core.snapshot().session.is_none());
        assert_eq!(core.totals.distance_m, 20);
        let rec = &crate::session::read_sessions(&dir, 4)[0];
        assert_eq!(rec.end_reason, "disconnected");
        assert_eq!(rec.distance_m, 20);
    }

    #[tokio::test]
    async fn sse_receives_snapshots() {
        let dir = tmpdir("sse");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(8);
        let (_, api_rx) = mpsc::channel(4);
        let mut core = Core::new(
            cfg(),
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        core.begin_connection(None);
        core.on_packet(&st(p::BeltState::Running, 2.0, 1, 1, 0, 1.0));
        let first = ev_rx.recv().await.unwrap();
        assert_eq!(first.state, State::Disconnected);
        let second = ev_rx.recv().await.unwrap();
        assert_eq!(second.state, State::Running);
        assert!((second.speed_kmh - 2.0).abs() < 1e-9);
    }
}

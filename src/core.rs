// SPDX-License-Identifier: ISC

//! Daemon state machine (§4) plus the per-connection pump that feeds it.

use crate::link::{LinkError, Notify};
use crate::metrics::Metrics;
use crate::protocol as p;
use crate::session::{DeltaTracker, EndReason, Totals};
use futures::pin_mut;
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
    /// On-demand link down by choice (§4.5a): no reconnects, any control
    /// call wakes the daemon.
    Sleeping,
    Stopped,
    Starting,
    Running,
    Paused,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Disconnected => "disconnected",
            State::Sleeping => "sleeping",
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
    fn packet(
        &self,
        target_speed_kmh: f64,
        cfg: &crate::config::DaemonCfg,
        variant: p::Variant,
    ) -> Vec<u8> {
        let pause_speed = target_speed_kmh.clamp(cfg.min_speed, cfg.max_speed);
        let out = |c: p::Command, speed| {
            variant
                .encode_command(c, speed, cfg.weight_kg, cfg.user_id)
                .to_vec()
        };
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
    /// §4.5a: bring the link up without queueing a belt command.
    Connect,
    /// §4.5a: drop the link now (409 while moving).
    Sleep,
}

/// Rejection of a client command (mapping in http.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErr {
    /// 409: not connected (or the belt is moving where that's required).
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
    /// K6: why walks can't be saved right now (None = saving works).
    pub persist_error: Option<String>,
    /// Lifetime totals ride along for the state endpoint.
    pub totals: Totals,
}

/// Why a connection died (drives the session end reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    Link,
    Watchdog,
    Shutdown,
    /// §4.5a: stopped idle for --idle-timeout, disconnect on purpose.
    Idle,
    /// §4.5a: POST /api/sleep while connected-and-stopped.
    Sleep,
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
/// F1: settle-sync suppression window after a daemon-issued start.
pub const SETTLE_SUPPRESS_S: u64 = 10;
/// F1/G2: at most this many SetSpeed corrections per start.
pub const MAX_START_CORRECT: u8 = 3;
/// H1: seconds between corrective SetSpeed sends.
pub const START_RETRY_S: u64 = 2;
/// §4.5a: a wake must have the link up within this budget, or the queued
/// command is dropped and the daemon goes back to sleep.
pub const WAKE_BUDGET_S: u64 = 30;
/// §4.5a: late-connect crediting caps (a remote-started walk the daemon
/// slept through is credited once, but absurd values are clamped).
pub const LATE_CREDIT_MAX_M: u32 = 20_000;
pub const LATE_CREDIT_MAX_STEPS: u32 = 30_000;

pub struct Core {
    pub cfg: crate::config::DaemonCfg,
    pub metrics: Arc<Metrics>,
    pub totals: Totals,
    deltas: DeltaTracker,
    pub state: State,
    /// Open a session on the next running packet (first running after
    /// stopped/disconnected, §4.3); paused/starting never open one.
    may_open: bool,
    /// §4.5a: when the belt has been stopped since this instant (and no
    /// control call happened), the link may go to sleep.
    idle_since: Option<tokio::time::Instant>,
    /// §4.5a: set by a control call while sleeping; the daemon wakes.
    wake_requested: bool,
    /// §4.5a: set by POST /api/sleep; the pump drops the link.
    sleep_requested: bool,
    /// §4.5a: a wake-from-sleep connect expects the first packet to
    /// describe a belt possibly started with the remote: credit its
    /// counters to the lifetime totals once.
    expect_late_credit: bool,
    /// F1: this model ignores the speed field of the start command, so a
    /// daemon-issued `Start(v)` is followed by a `SetSpeed(v)` on the
    /// first running packet if the belt targets something else, and the
    /// settle-sync (§3.6 remote pickup) is suppressed until the belt
    /// reports `v` (at most SETTLE_SUPPRESS_S), so the user's speed
    /// is not erased.
    pending_start_speed: Option<f64>,
    settle_suppress_until: Option<tokio::time::Instant>,
    start_correct_tries: u8,
    /// K7: the last reported step count — frames without a steps value
    /// (the 52-byte `68 34` connect frame) keep it, so the counter-reset
    /// path never trips on a missing value.
    last_steps: Option<u32>,
    /// K6: session records held back because the file wasn't writable,
    /// retried on the next persist (capped so a long outage can't grow it).
    pending_records: Vec<crate::session::SessionRec>,
    /// K6: why walks can't be saved right now (None = saving works).
    persist_error: Option<String>,
    /// H1: when the last corrective SetSpeed went out.
    last_start_correct: Option<tokio::time::Instant>,
    imperial: bool,
    /// Protocol family of the current link (§3.7), set on connect.
    variant: p::Variant,
    /// SupeRun frame sequence number, restarted on every connection.
    seq: u8,
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
            idle_since: None,
            wake_requested: false,
            sleep_requested: false,
            expect_late_credit: false,
            pending_start_speed: None,
            settle_suppress_until: None,
            start_correct_tries: 0,
            last_steps: None,
            pending_records: Vec::new(),
            persist_error: None,
            last_start_correct: None,
            imperial: false,
            variant: p::Variant::default(),
            seq: 0,
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

    /// The session as it is reported and compared: pre-reset offset plus
    /// the currently mirrored (raw, post-reset) counters, so a distance
    /// target set before a mid-session reset does not over-walk
    /// (REVIEW round-2 A6).
    fn reported_session(&self) -> Option<OpenSession> {
        self.session.as_ref().map(|s| OpenSession {
            distance_m: self.session_offset.distance_m + s.distance_m,
            steps: self.session_offset.steps + s.steps,
            kcal: self.session_offset.kcal + s.kcal,
            duration_s: self.session_offset.duration_s + s.duration_s,
            start: s.start,
            max_speed_kmh: s.max_speed_kmh,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            connected: self.linked(),
            address: self.address.clone(),
            state: self.state,
            imperial: self.imperial,
            speed_kmh: self.speed_kmh,
            target_speed_kmh: self.target_speed_kmh,
            queue_len: self.queue.len(),
            session: self.reported_session(),
            target: self.target,
            last_end_reason: self.last_end_reason,
            persist_error: self.persist_error.clone(),
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

    /// K6: run once at startup so a state dir the service can't write to
    /// is reported loudly instead of silently losing walks.
    pub fn check_state_dir(&mut self) {
        if let Err(reason) = crate::session::check_state_dir_writable(&self.state_dir) {
            tracing::error!("{reason}");
            self.persist_error = Some(reason);
        }
    }

    pub fn persist(&mut self) {
        if let Err(err) = crate::session::save_totals(&self.state_dir, &self.totals) {
            tracing::error!(error = %err, "persisting totals.json failed");
            self.persist_error = Some(format!("totals.json: {err}"));
            self.metrics.persist_errors_total.inc();
            return;
        }
        // K6/L4: clear the error only when the thing that failed is fixed.
        // totals.json just saved; if sessions.jsonl was the problem, probe
        // appendability before clearing (an open() alone writes nothing,
        // unlike the create-dir probe below).
        self.persist_error = None;
        let sessions_path = self.state_dir.join("sessions.jsonl");
        if sessions_path.exists() {
            match std::fs::OpenOptions::new()
                .append(true)
                .open(&sessions_path)
            {
                Ok(_) => {}
                Err(err) => {
                    self.persist_error = Some(format!(
                        "sessions.jsonl: {err} (chown the files in the state dir)"
                    ));
                }
            }
        } else if let Err(reason) = crate::session::check_state_dir_writable(&self.state_dir) {
            self.persist_error = Some(reason);
        }
        // K6: retry any session records held back by an earlier failure
        let held: Vec<_> = self.pending_records.drain(..).collect();
        for rec in held {
            match crate::session::append_session(&self.state_dir, &rec) {
                Ok(()) => {
                    self.totals.sessions += 1;
                    self.metrics.sessions_total.inc();
                    tracing::info!(distance_m = rec.distance_m, "held walk finally saved");
                }
                Err(_) => {
                    // still failing: put it back and stop retrying
                    self.pending_records.push(rec);
                    self.persist_error = Some(
                        "sessions.jsonl: still not writable (chown the files in the state dir)"
                            .into(),
                    );
                    break;
                }
            }
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
            ApiCmd::Connect => {
                // waking with no belt command: the daemon sees the request
                self.wake_requested = true;
                Ok(())
            }
            ApiCmd::Sleep => {
                // §4.5a (amended): also while disconnected, where it ends
                // a mid-walk reconnect loop (REVIEW round-6 D2)
                if self.moving() {
                    return Err(ApiErr::Disabled);
                }
                self.sleep_requested = true;
                Ok(())
            }
            _ if self.state == State::Disconnected => Err(ApiErr::Disabled),
            ApiCmd::Start(s) => {
                if self.state == State::Sleeping {
                    self.wake_requested = true;
                }
                let speed = self.clamp(s.unwrap_or(self.target_speed_kmh));
                self.target_speed_kmh = speed;
                self.forget_settle();
                if self.state == State::Running {
                    // an explicit speed while moving means set-speed
                    self.queue_set_speed(speed)
                } else {
                    // F1: the belt ignores the start command's speed field
                    // and ramps to its own default: remember the requested
                    // speed and correct it on the first running packet
                    self.start_at(speed)
                }
            }
            ApiCmd::Pause => {
                if self.state == State::Sleeping {
                    self.wake_requested = true;
                }
                self.idle_since = Some(tokio::time::Instant::now());
                // user override: the start-speed correction is cancelled
                self.cancel_start_correction();
                self.forget_settle();
                self.queue_back(WireCmd::Pause)
            }
            ApiCmd::Stop => {
                if self.state == State::Sleeping {
                    self.wake_requested = true;
                }
                self.idle_since = Some(tokio::time::Instant::now());
                self.cancel_start_correction();
                // stop must never wait behind anything (§3.5)
                self.queue.clear();
                self.pending_write = None;
                self.forget_settle();
                self.queue.push_back(WireCmd::Stop);
                Ok(())
            }
            ApiCmd::SpeedAbs(v) => {
                // §4.5a (amended): a speed change while sleeping only
                // updates the remembered target — no wake, no beep
                self.idle_since = Some(tokio::time::Instant::now());
                // user override
                self.cancel_start_correction();
                let v = self.clamp(v);
                self.target_speed_kmh = v;
                self.forget_settle();
                if self.state == State::Paused {
                    // K8: like Start — one code path
                    return self.start_at(v);
                }
                if self.moving() {
                    self.queue_set_speed(v)?;
                }
                Ok(())
            }
            ApiCmd::SpeedDelta(d) => {
                // §4.5a (amended): no wake, like SpeedAbs; also a user
                // override of the start-speed correction
                self.idle_since = Some(tokio::time::Instant::now());
                self.cancel_start_correction();
                let v = self.clamp(self.target_speed_kmh + d);
                self.target_speed_kmh = v;
                self.forget_settle();
                if self.state == State::Paused {
                    // K8: like SpeedAbs — Start(v) from paused, one path
                    self.pending_start_speed = Some(v);
                    self.queue_back(WireCmd::Start(v))?;
                } else if self.moving() {
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
        let Some(st) = self.variant.decode_status(raw) else {
            self.metrics.invalid_packets_total.inc_by(1);
            tracing::debug!(bytes = %hex(raw), "undecodable status notification");
            return;
        };
        // §12: every packet in hex, both directions, with the model that
        // decoded it (visible once a second model exists)
        tracing::debug!(
            bytes = %hex(raw),
            state = ?st.belt_state(),
            model = p::decoder_for(raw).name(),
            speed_kmh = st.speed_mps * 3.6,
            "status notification"
        );

        self.imperial = st.imperial();
        self.speed_kmh = st.speed_mps * 3.6;

        // K7: a frame without a steps value (the 52-byte `68 34` connect
        // frame) keeps the last seen count, so no counter-reset trip
        let steps = st.steps.unwrap_or(self.last_steps.unwrap_or(0));
        self.last_steps = Some(steps);

        // lifetime totals via deltas (§4.4)
        if let Some(d) = self.deltas.apply(crate::session::Counters {
            distance_m: st.distance_m,
            steps,
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
        self.correct_start_speed(&st, belt);
        self.late_credit(&st, steps, belt);
        self.begin_session_if_needed(belt);
        if belt != State::Stopped {
            self.update_running_session(&st, steps);
        } else if let Some(s) = self.session.as_mut() {
            // §4.3: the stop frame usually still carries the walk's final
            // counters — sometimes a step or a metre above the last
            // running frame (the belt's final accounting) — before the
            // counters reset ~6 s later. Fold it in when it grew; a
            // post-reset stop frame (all counters back to 0) is ignored,
            // so the record keeps the values mirrored while running.
            if st.distance_m >= s.distance_m && st.duration_s >= s.duration_s && steps >= s.steps {
                s.distance_m = st.distance_m;
                s.steps = steps;
                s.kcal = st.kcal;
                s.duration_s = st.duration_s;
            }
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
        // §4.5a: the idle clock runs while the belt is stopped
        if belt == State::Stopped {
            if self.idle_since.is_none() {
                self.idle_since = Some(tokio::time::Instant::now());
            }
        } else {
            self.idle_since = None;
        }
        self.check_targets();
        self.publish();
    }

    /// F1: on the first running packet after a daemon-issued start, make
    /// the belt go to the requested speed (it ignored the start command's
    /// speed field) and suppress the settle-sync until it gets there.
    /// F1/G2: this model ignores the start command's speed and reports
    /// target 0 during the starting countdown. Act only on `running`
    /// packets; keep the pending speed until the belt reports it, re-
    /// queueing SetSpeed at most MAX_START_CORRECT times (the one-shot
    /// version could spend itself on the countdown and let the 1.0
    /// default settle back in). The suppression ends when the belt
    /// reports the requested target, or after the window — whichever
    /// comes first — so the §3.6 remote pickup keeps working.
    /// K8/L6: one code path for starting at a requested speed — used by
    /// ApiCmd::Start and by a speed-set while paused (the belt starts on
    /// SetSpeed but loses the speed). Arms the start-speed reassertion.
    fn start_at(&mut self, v: f64) -> Result<(), ApiErr> {
        self.idle_since = Some(tokio::time::Instant::now());
        self.pending_start_speed = Some(v);
        self.queue_back(WireCmd::Start(v))
    }

    fn correct_start_speed(&mut self, st: &p::Status, belt: State) {
        let Some(v) = self.pending_start_speed else {
            return;
        };
        if belt != State::Running {
            return; // act only on running packets
        }
        if self.settle_suppress_until.is_none() {
            self.settle_suppress_until =
                Some(tokio::time::Instant::now() + Duration::from_secs(SETTLE_SUPPRESS_S));
        }
        if eq_speed(st.target_speed_kmh, v) {
            // the belt got there: normal remote pickup resumes
            self.cancel_start_correction();
            return;
        }
        if self
            .settle_suppress_until
            .is_some_and(|d| tokio::time::Instant::now() >= d)
        {
            // give up: log so the behaviour is visible
            tracing::warn!(
                want = v,
                sends = self.start_correct_tries,
                "belt never reported the requested start speed; giving up"
            );
            self.cancel_start_correction();
            return;
        }
        // H1: count SENDS and time them — the previous packet-counted
        // version fired twice within the first second (one packet = one
        // try) and then stayed silent for the rest of the window
        let due = self.last_start_correct.is_none()
            || tokio::time::Instant::now()
                >= self.last_start_correct.expect("checked above")
                    + Duration::from_secs(START_RETRY_S);
        if due && self.start_correct_tries < MAX_START_CORRECT {
            self.start_correct_tries += 1;
            self.last_start_correct = Some(tokio::time::Instant::now());
            tracing::info!(
                target = st.target_speed_kmh,
                want = v,
                send = self.start_correct_tries,
                "belt targets its own default; queueing SetSpeed"
            );
            let _ = self.queue_set_speed(v);
        }
    }

    /// K6/L4: held records die with the process — dump each as its JSON
    /// line at error level so they can be recovered from the journal with
    /// a plain `grep 'unsaved walk' … >> sessions.jsonl`.
    fn dump_held_records(&mut self) {
        for rec in self.pending_records.drain(..) {
            if let Ok(line) = serde_json::to_string(&rec) {
                tracing::error!("unsaved walk: {line}");
            }
        }
    }

    /// F1/G2: drop the pending start-speed correction (user override,
    /// belt landed, or give-up).
    fn cancel_start_correction(&mut self) {
        self.pending_start_speed = None;
        self.start_correct_tries = 0;
        self.last_start_correct = None;
        self.settle_suppress_until = None;
    }

    /// §4.5a: the first packet of a wake-from-sleep connection may show a
    /// belt the daemon never saw being started (remote start): credit its
    /// counters to the lifetime totals once (capped), and seed the session
    /// that is about to open with them.
    fn late_credit(&mut self, st: &p::Status, steps: u32, belt: State) {
        if !self.expect_late_credit || self.session.is_some() {
            return;
        }
        self.expect_late_credit = false;
        if !matches!(belt, State::Running | State::Paused) {
            return;
        }
        let capped = SessionCounters {
            distance_m: st.distance_m.min(LATE_CREDIT_MAX_M),
            steps: steps.min(LATE_CREDIT_MAX_STEPS),
            kcal: st.kcal,
            duration_s: st.duration_s,
        };
        if capped.distance_m < st.distance_m || capped.steps < steps {
            tracing::warn!(
                distance_m = st.distance_m,
                steps = capped.steps,
                "late-connect counters exceed the cap, clamped"
            );
        }
        if capped.distance_m == 0
            && capped.steps == 0
            && capped.kcal == 0
            && capped.duration_s == 0.0
        {
            return; // nothing to credit
        }
        tracing::info!(
            distance_m = capped.distance_m,
            steps = capped.steps,
            kcal = capped.kcal,
            duration_s = capped.duration_s,
            "crediting a remote-started walk that happened while sleeping"
        );
        self.totals.distance_m += u64::from(capped.distance_m);
        self.totals.steps += u64::from(capped.steps);
        self.totals.kcal += u64::from(capped.kcal);
        self.totals.moving_s += capped.duration_s;
        self.metrics.bump_totals(
            capped.distance_m,
            capped.steps,
            u32::from(capped.kcal),
            capped.duration_s,
        );
        // no session seed: the mirrored counters overwrite it in the same
        // packet, and a stale seed would inflate the next record
        // (REVIEW round-6 D4)
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
    fn update_running_session(&mut self, st: &p::Status, steps: u32) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        // L5/K7: the counters reset together on this model. A drop in
        // distance or duration is a real reset (freeze the old values);
        // a lone steps drop with distance/duration still growing is a
        // stale carry-over — re-baseline steps without an offset.
        // Real captures (tests/captures/) show the belt's moving-duration
        // regressing a hair under one second at the pause boundary
        // (106.999 → 106 with the distance frozen): that is the belt's
        // own rounding, not a reset — only a drop past a packet tick
        // counts (a real reset zeroes the distance with it anyway).
        if st.distance_m < s.distance_m
            || st.duration_s < s.duration_s - crate::session::DURATION_JITTER_S
        {
            self.session_offset.distance_m += s.distance_m;
            self.session_offset.steps += s.steps;
            self.session_offset.kcal += s.kcal;
            self.session_offset.duration_s += s.duration_s;
        }
        // a lone steps drop (stale carry-over from the previous walk):
        // mirror the fresh value without touching the offset — the stale
        // steps were never part of this session
        s.distance_m = st.distance_m;
        s.steps = steps;
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
        // F1: after a daemon-issued start, the belt ramps to its own
        // default first; do not "sync" the user's requested speed away
        // before it reports the requested target (or ~10 s pass)
        if self
            .settle_suppress_until
            .is_some_and(|d| tokio::time::Instant::now() < d)
        {
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
        let Some(s) = self.reported_session() else {
            return;
        };
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
        match crate::session::append_session(&self.state_dir, &rec) {
            Ok(()) => {
                self.totals.sessions += 1;
                self.metrics.sessions_total.inc();
            }
            Err(err) => {
                // K6: keep the record and retry it on the next persist;
                // the sessions counter increments only on the save
                tracing::error!(error = %err, "appending session record failed; holding it in memory");
                self.pending_records.push(rec);
                if self.pending_records.len() > 100 {
                    self.pending_records.remove(0);
                }
                self.persist_error = Some(format!(
                    "sessions.jsonl: {err} (chown the files in the state dir)"
                ));
                self.metrics.persist_errors_total.inc();
            }
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
            DeadReason::Link | DeadReason::Watchdog | DeadReason::Idle => EndReason::Disconnected,
            DeadReason::Shutdown => EndReason::Shutdown,
            DeadReason::Sleep => EndReason::Disconnected,
        };
        self.close_session(end_reason);
        self.persist();
        // K6/L4: held walks are dumped on EVERY shutdown path (the
        // sleeping/waking/reconnecting exits included), not just the
        // live-link shutdown_flow
        if reason == DeadReason::Shutdown {
            self.dump_held_records();
        }
        self.address = None;
        self.publish();
    }

    /// Fresh connection: the next notification becomes the deltas baseline.
    pub fn begin_connection(&mut self, address: Option<String>) {
        self.address = address;
        self.seq = 0;
        self.deltas.reset();
        // first running after the (re)connect opens a session (§4.3)
        self.may_open = true;
        // a plain reconnect's first packet is a continuation baseline, not
        // a remote-started walk to credit (§4.4 vs §4.5a)
        self.expect_late_credit = false;
        self.forget_settle();
        self.publish();
    }

    /// A link just came up: take its protocol family (§3.7), then start
    /// the connection as above. Returns the device address.
    pub fn begin_link<C: Notify>(&mut self, conn: &C) -> String {
        let addr = conn.address();
        self.variant = conn.variant();
        tracing::info!(address = %addr, variant = self.variant.as_str(), "protocol");
        self.begin_connection(Some(addr.clone()));
        addr
    }

    /// Protocol family of the current (or last) link.
    pub fn variant(&self) -> p::Variant {
        self.variant
    }

    // ---------------------------------------------------- §4.5a sleep

    /// Is the daemon holding a live link (not sleeping, not dead)?
    pub fn linked(&self) -> bool {
        !matches!(self.state, State::Disconnected | State::Sleeping)
    }

    /// §4.5a: when the belt is stopped, the instant at which the idle
    /// timer runs out, if ever.
    pub fn idle_deadline(&self) -> Option<tokio::time::Instant> {
        if self.cfg.idle_timeout == 0 || !self.linked() {
            return None;
        }
        self.idle_since
            .map(|t| t + Duration::from_secs(self.cfg.idle_timeout))
    }

    /// §4.5a: disconnect on purpose and stop trying. No session is open
    /// (sleeping only happens from `stopped`), deltas re-baseline on wake.
    pub fn enter_sleep(&mut self) {
        self.state = State::Sleeping;
        self.deltas.reset();
        self.queue.clear();
        self.pending_write = None;
        self.forget_settle();
        self.wake_requested = false;
        self.sleep_requested = false;
        self.idle_since = None;
        // a remote-started walk during sleep must be credited at wake
        self.expect_late_credit = true;
        self.publish();
    }

    pub fn take_wake(&mut self) -> bool {
        std::mem::take(&mut self.wake_requested)
    }

    /// §4.5a: mark the next connection as wake-from-sleep, so its first
    /// packet may credit a remote-started walk (unlike a plain reconnect).
    pub fn arm_late_credit(&mut self) {
        self.expect_late_credit = true;
    }

    pub fn take_sleep(&mut self) -> bool {
        std::mem::take(&mut self.sleep_requested)
    }

    /// §4.5a: a wake that timed out drops the command that caused it.
    pub fn drop_queued(&mut self) {
        self.queue.clear();
        self.pending_write = None;
    }

    /// §4.5a: the sleeping wait loop pulls client commands from here.
    pub async fn wait_wake_cmd(&mut self) -> Option<ApiCmd> {
        self.inbox.recv().await
    }

    // ------------------------------------------- heartbeat / reply side

    /// Packet for the next notification: the retry-candidate, else the next
    /// queued command, else the heartbeat (§3.1).
    pub fn next_reply(&mut self) -> Vec<u8> {
        if let Some(pk) = self.pending_write.take() {
            return pk;
        }
        match self.queue.pop_front() {
            Some(c) => c.packet(self.target_speed_kmh, &self.cfg, self.variant),
            None => p::HEARTBEAT.to_vec(),
        }
    }

    /// Wire bytes for a reply packet: framed with the next sequence
    /// number on SupeRun links, as is otherwise (§3.7).
    fn wire(&mut self, pkt: &[u8]) -> Vec<u8> {
        let out = self.variant.frame(self.seq, pkt);
        if self.variant == p::Variant::SupeRun {
            self.seq = self.seq.wrapping_add(1);
        }
        out
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

/// §12: hex trace of a packet, space-separated.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |acc, b| {
        if acc.is_empty() {
            format!("{b:02x}")
        } else {
            acc + &format!(" {b:02x}")
        }
    })
}

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

/// One reply packet (per notification, or per keep-alive tick on SupeRun
/// links): next queued command or heartbeat, with the same-packet retry
/// rule on write errors (§3.5). A retry is re-framed with a new sequence
/// number.
async fn write_reply<C: Notify>(core: &mut Core, conn: &mut C) {
    let pkt = core.next_reply();
    let wire = core.wire(&pkt);
    // §12: every packet in hex, both directions
    tracing::debug!(bytes = %hex(&wire), "write");
    if conn.write_packet(&wire).await.is_ok() {
        core.write_succeeded();
        return;
    }
    if pkt.as_slice() != p::HEARTBEAT {
        core.write_failed(pkt);
    }
}

/// Drive one connection until it dies: apply notifications, send exactly
/// one packet per notification (§3.1) — or, on SupeRun links, one per
/// 200 ms keep-alive tick and none in reply (§3.7) — enforce the 10 s
/// liveness watchdog (§4.1), flush totals every 60 s (§4.4). The caller
/// reconnects (or shuts down) upon return.
pub async fn pump<C: Notify>(
    core: &mut Core,
    conn: &mut C,
    ctrl: &mut mpsc::Receiver<Ctrl>,
) -> DeadReason {
    // The notification future is recreated on every loop pass. On PitPat
    // links no other select arm completes while it is pending — an earlier
    // two-price sleep arm caused the recreated timer to restart and starve
    // notifications forever. The SupeRun keep-alive arm below does fire
    // while it is pending: that relies on recv_notification being
    // cancel-safe (see `Notify`).
    let mut next_persist = tokio::time::Instant::now() + Duration::from_secs(PERSIST_S);
    let mut last_notification = tokio::time::Instant::now();
    // §3.7: a SupeRun link is kept alive by a timer; the interval lives
    // outside the loop so it is never restarted, and every deadline in
    // the loop is absolute.
    let mut keepalive = core.variant().keepalive().map(|period| {
        let mut i = tokio::time::interval(period);
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        i
    });
    loop {
        core.drain_api_cmds();
        if core.take_sleep() {
            // §4.5a: POST /api/sleep while connected and stopped
            return DeadReason::Sleep;
        }
        // whichever comes first: the liveness watchdog or the idle
        // timeout (§4.5a). The notification future is recreated every
        // pass, which is safe only because this timeout is never shorter
        // than the notification period (watchdog 10 s; idle ≥ stopped
        // period); an earlier 200 ms tick arm starved notifications
        // (sleep-based, not cancel-safe, connections at the time).
        let watchdog_at = last_notification + Duration::from_secs(WATCHDOG_S);
        let at = core
            .idle_deadline()
            .map_or(watchdog_at, |idle_at| idle_at.min(watchdog_at));
        let wait = at.saturating_duration_since(tokio::time::Instant::now());
        tokio::select! {
            ev = tokio::time::timeout(wait, conn.recv_notification()) => {
                let raw = match ev {
                    Ok(Ok(raw)) => raw,
                    Ok(Err(_)) => return DeadReason::Link,
                    Err(_) => {
                        if core.idle_deadline().is_some_and(|d| d <= at) {
                            tracing::info!(idle_s = core.cfg.idle_timeout, "idle timeout, sleeping");
                            return DeadReason::Idle;
                        }
                        tracing::warn!(secs = WATCHDOG_S, "no notification, forcing reconnect");
                        return DeadReason::Watchdog;
                    }
                };
                last_notification = tokio::time::Instant::now();
                core.on_packet(&raw);
                core.drain_api_cmds();
                if keepalive.is_none() {
                    write_reply(core, conn).await;
                }
                if core.moving() && tokio::time::Instant::now() >= next_persist {
                    next_persist = tokio::time::Instant::now() + Duration::from_secs(PERSIST_S);
                    core.persist();
                }
            }
            _ = keepalive_tick(&mut keepalive) => {
                core.drain_api_cmds();
                write_reply(core, conn).await;
            }
            c = ctrl.recv() => match c {
                Some(Ctrl::Shutdown) => return DeadReason::Shutdown,
                None => return DeadReason::Link,
            },
        }
    }
}

/// Next keep-alive tick; never completes on links without one.
async fn keepalive_tick(keepalive: &mut Option<tokio::time::Interval>) {
    match keepalive {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// SIGTERM/SIGINT (§4.6): if the belt is moving, send stop and wait up
/// to 3 s for the treadmill to confirm via the notification cycle, then
/// close the session (end_reason shutdown), persist totals, drop the
/// LE link explicitly. Used by the supervisor before returning.
pub async fn shutdown_flow<C: Notify>(core: &mut Core, conn: &mut C) {
    if core.moving() {
        tracing::info!("stopping the belt before exit");
        core.begin_shutdown();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        // §3.7: SupeRun links write every keep-alive period whether or
        // not a notification came in, so the stop goes out right away
        let keepalive = core.variant().keepalive();
        let step = keepalive.unwrap_or(Duration::from_millis(500));
        while core.state != State::Stopped {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("belt not confirmed stopped within 3 s, exiting anyway");
                break;
            }
            match tokio::time::timeout(step, conn.recv_notification()).await {
                Ok(Ok(raw)) => {
                    core.on_packet(&raw);
                    core.drain_api_cmds();
                    write_reply(core, conn).await;
                }
                _ if keepalive.is_some() => {
                    core.drain_api_cmds();
                    write_reply(core, conn).await;
                }
                _ => {}
            }
        }
    }
    core.on_disconnect(DeadReason::Shutdown);
    core.persist();
    // BlueZ keeps the LE link up after the notify stream dies unless the
    // Device.Disconnect goes out explicitly: the phone app would stay
    // locked out (REVIEW round-2 A4)
    conn.disconnect().await;
}

/// §4.5a + §4.1: the whole daemon link policy, factored out of main so
/// it can be tested (REVIEW round-6 D3):
/// - boot asleep (unless idle_timeout = 0), wake on control calls with a
///   30 s budget (queued command dropped on timeout),
/// - idle timeout → disconnect + sleep,
/// - link lost while stopped → sleep immediately; lost mid-walk →
///   reconnect with backoff but give up after idle_timeout without a
///   link (close the session as disconnected, then sleep); /api/sleep
///   ends such a reconnect loop; idle_timeout = 0 reconnects forever.
pub async fn supervise<L>(mut core: Core, mut link: L, mut ctrl: mpsc::Receiver<Ctrl>)
where
    L: crate::link::LinkT,
    L::Conn: Notify + 'static,
{
    let mut sleeping = core.cfg.idle_timeout > 0;
    let mut backoff_s: u64 = 1;
    if sleeping {
        core.enter_sleep();
    }
    'outer: loop {
        // ---------------------------------------------- obtain a link
        let conn = if sleeping {
            // §4.5a: wait for a wake-capable control call or shutdown;
            // nothing else runs: no reconnects, no backoff, no watchdog
            let woke = tokio::select! {
                c = ctrl.recv() => match c {
                    Some(Ctrl::Shutdown) | None => None,
                },
                cmd = core.wait_wake_cmd() => cmd,
            };
            let Some(cmd) = woke else {
                core.on_disconnect(DeadReason::Shutdown);
                return;
            };
            core.queue_api(cmd).ok();
            core.take_sleep(); // sleeping again is a no-op success
            if !core.take_wake() {
                // target-only or repeat-sleep: keep sleeping
                continue 'outer;
            }
            tracing::info!("waking on control call");
            // §4.5a: connect within the wake budget; the budget applies
            // to each attempt too (timeout_at), not just the loop
            let deadline = tokio::time::Instant::now() + Duration::from_secs(WAKE_BUDGET_S);
            let mut woken_conn: Option<L::Conn> = None;
            enum WakeAttempt<C> {
                Done(Result<C, LinkError>),
                TimedOut,
                Shutdown,
            }
            while tokio::time::Instant::now() < deadline {
                let attempt = {
                    let connect = link.connect();
                    pin_mut!(connect);
                    tokio::select! {
                        outcome = tokio::time::timeout_at(deadline, &mut connect) => match outcome {
                            Ok(res) => WakeAttempt::Done(res),
                            Err(_) => WakeAttempt::TimedOut,
                        },
                        c = ctrl.recv() => match c {
                            Some(Ctrl::Shutdown) | None => WakeAttempt::Shutdown,
                        },
                    }
                };
                match attempt {
                    WakeAttempt::Done(Ok(conn)) => {
                        let addr = core.begin_link(&conn);
                        // the belt may have been remote-started meanwhile
                        core.arm_late_credit();
                        tracing::info!(address = %addr, "woken, link established");
                        woken_conn = Some(conn);
                        break;
                    }
                    WakeAttempt::Done(Err(err)) => {
                        tracing::warn!(?err, "wake connect failed");
                        // the retry sleep is raced against shutdown, like
                        // every other wait in the supervisor
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                            c = ctrl.recv() => match c {
                                Some(Ctrl::Shutdown) | None => {
                                    core.drop_queued();
                                    core.on_disconnect(DeadReason::Shutdown);
                                    return;
                                }
                            },
                        }
                    }
                    WakeAttempt::TimedOut => break, // budget exhausted
                    WakeAttempt::Shutdown => {
                        // shutdown while waking
                        core.drop_queued();
                        core.on_disconnect(DeadReason::Shutdown);
                        return;
                    }
                }
            }
            match woken_conn {
                Some(conn) => conn,
                None => {
                    // §4.5a: the link didn't come up in time: drop the
                    // queued command and go back to sleep
                    tracing::warn!("wake budget exhausted, dropping queued command");
                    core.drop_queued();
                    core.enter_sleep();
                    continue 'outer;
                }
            }
        } else {
            // §4.1 connect (raced with shutdown so a SIGTERM doesn't wait
            // for a whole scan)
            let attempt = {
                let connect = link.connect();
                pin_mut!(connect);
                tokio::select! {
                    outcome = &mut connect => Some(outcome),
                    c = ctrl.recv() => match c {
                        Some(Ctrl::Shutdown) | None => None,
                    },
                }
            };
            match attempt {
                Some(Ok(conn)) => {
                    let addr = core.begin_link(&conn);
                    tracing::info!(address = %addr, "link established");
                    backoff_s = 1;
                    conn
                }
                Some(Err(err)) => {
                    tracing::warn!(?err, "connection failed");
                    core.on_disconnect(DeadReason::Link);
                    core.metrics.reconnects_total.inc();
                    match backoff_wait(&mut core, &mut backoff_s, &mut ctrl).await {
                        BackoffOutcome::Shutdown => {
                            core.on_disconnect(DeadReason::Shutdown);
                            return;
                        }
                        BackoffOutcome::Sleep => {
                            core.enter_sleep();
                            sleeping = true;
                            continue 'outer;
                        }
                        BackoffOutcome::Tick => continue 'outer,
                    }
                }
                None => {
                    core.on_disconnect(DeadReason::Shutdown);
                    return;
                }
            }
        };
        // ------------------------------------------------------ run
        let mut conn = conn;
        loop {
            let reason = pump(&mut core, &mut conn, &mut ctrl).await;
            match reason {
                DeadReason::Shutdown => {
                    shutdown_flow(&mut core, &mut conn).await;
                    return;
                }
                DeadReason::Idle | DeadReason::Sleep => {
                    conn.disconnect().await;
                    core.enter_sleep();
                    sleeping = true;
                    continue 'outer;
                }
                DeadReason::Link | DeadReason::Watchdog => {
                    tracing::warn!(?reason, address = %conn.address(), "link lost");
                    // drop the LE link even when the notify stream died:
                    // BlueZ would keep the treadmill locked against its
                    // phone app otherwise (REVIEW round-2 A4)
                    conn.disconnect().await;
                    if core.state == State::Stopped && core.session.is_none() {
                        // §4.5a (amended): the treadmill was most likely
                        // switched off: reconnecting forever keeps the
                        // radio busy — sleep straight away
                        core.enter_sleep();
                        sleeping = true;
                        continue 'outer;
                    }
                    if core.cfg.idle_timeout == 0 {
                        // §4.1: the old reconnect-forever behaviour
                        core.on_disconnect(reason);
                        core.metrics.reconnects_total.inc();
                        match backoff_wait(&mut core, &mut backoff_s, &mut ctrl).await {
                            BackoffOutcome::Shutdown => {
                                core.on_disconnect(DeadReason::Shutdown);
                                return;
                            }
                            BackoffOutcome::Sleep => {
                                core.enter_sleep();
                                sleeping = true;
                                continue 'outer;
                            }
                            BackoffOutcome::Tick => continue 'outer,
                        }
                    }
                    // §4.5a (amended): lost mid-walk: the loss is handled
                    // as in §4.1 FIRST — the open session closes as
                    // `disconnected` right away (state disconnected, speed
                    // cleared) so the API/metrics don't show a moving,
                    // connected treadmill with no link, and the walk is
                    // recorded. The reconnect that follows then opens a
                    // new session as usual (§4.3). (REVIEW round-7 E1)
                    core.on_disconnect(reason);
                    core.metrics.reconnects_total.inc();
                    // reconnect with backoff, but give up after
                    // idle_timeout without a link and sleep. The wait also
                    // serves queued client commands so a POST /api/sleep
                    // can end it. Each attempt is bounded only by the
                    // remaining budget — a real BLE connect takes several
                    // seconds — and a control call does not cancel it
                    // (REVIEW round-7 E2).
                    let budget =
                        tokio::time::Instant::now() + Duration::from_secs(core.cfg.idle_timeout);
                    let mut regained = false;
                    loop {
                        if tokio::time::Instant::now() >= budget {
                            break;
                        }
                        let attempt = {
                            let connect = link.connect();
                            pin_mut!(connect);
                            tokio::select! {
                                outcome = tokio::time::timeout_at(budget, &mut connect) => outcome.ok(),
                                c = ctrl.recv() => match c {
                                    Some(Ctrl::Shutdown) | None => {
                                        core.on_disconnect(DeadReason::Shutdown);
                                        return;
                                    }
                                },
                                cmd = core.wait_wake_cmd() => match cmd {
                                    Some(cmd) => {
                                        core.queue_api(cmd).ok();
                                        if core.take_sleep() {
                                            break; // ends the reconnect loop
                                        }
                                        None
                                    }
                                    None => {
                                        core.on_disconnect(DeadReason::Shutdown);
                                        return;
                                    }
                                },
                            }
                        };
                        match attempt {
                            Some(Ok(new)) => {
                                let addr = core.begin_link(&new);
                                tracing::info!(address = %addr, "link regained");
                                conn = new;
                                regained = true;
                                break;
                            }
                            Some(Err(_err)) => {
                                // failed attempt: sleep the backoff, raced
                                // against shutdown and the inbox (E2)
                                match backoff_wait(&mut core, &mut backoff_s, &mut ctrl).await {
                                    BackoffOutcome::Shutdown => {
                                        core.on_disconnect(DeadReason::Shutdown);
                                        return;
                                    }
                                    BackoffOutcome::Sleep => break,
                                    BackoffOutcome::Tick => {}
                                }
                            }
                            None => {
                                // budget exhausted
                                break;
                            }
                        }
                    }
                    if regained {
                        continue; // resume pumping the same session
                    }
                    core.on_disconnect(DeadReason::Link);
                    core.metrics.reconnects_total.inc();
                    core.enter_sleep();
                    sleeping = true;
                    continue 'outer;
                }
            }
        }
    }
}

/// Backoff sleep raced against shutdown AND client commands (E2): a
/// control call is queued without ending the wait — only /api/sleep or
/// shutdown end the loop.
enum BackoffOutcome {
    Shutdown,
    Sleep,
    Tick,
}

async fn backoff_wait(
    core: &mut Core,
    backoff_s: &mut u64,
    ctrl: &mut mpsc::Receiver<Ctrl>,
) -> BackoffOutcome {
    let out = tokio::select! {
        c = ctrl.recv() => match c {
            Some(Ctrl::Shutdown) | None => BackoffOutcome::Shutdown,
        },
        cmd = core.wait_wake_cmd() => match cmd {
            Some(cmd) => {
                core.queue_api(cmd).ok();
                if core.take_sleep() {
                    BackoffOutcome::Sleep
                } else {
                    BackoffOutcome::Tick
                }
            }
            None => BackoffOutcome::Shutdown,
        },
        _ = tokio::time::sleep(Duration::from_secs(*backoff_s)) => BackoffOutcome::Tick,
    };
    *backoff_s = (*backoff_s * 2).min(30);
    out
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
        st_target(belt, speed, speed, dist, steps, kcal, dur)
    }

    /// Variant with an explicit treadmill-side target speed (F1: the
    /// belt ignores the start command's speed, so its own target can
    /// differ from the daemon's).
    fn st_target(
        belt: p::BeltState,
        speed: f64,
        target: f64,
        dist: u32,
        steps: u32,
        kcal: u16,
        dur: f64,
    ) -> Vec<u8> {
        p::encode_status(belt, false, speed, target, dist, steps, kcal, dur).to_vec()
    }

    /// L5: a real 52-byte `68 34` connect frame — same fields, ASCII
    /// junk in the tail, no steps value (K7).
    fn st_connect_frame(
        belt: p::BeltState,
        speed: f64,
        target: f64,
        dist: u32,
        kcal: u16,
        dur: f64,
    ) -> Vec<u8> {
        let mut p = [0x00u8; 52];
        p[0] = 0x68;
        p[1] = 0x34;
        let spd = (speed * 1000.0) as u16;
        p[3] = (spd >> 8) as u8;
        p[4] = spd as u8;
        let tgt = (target * 1000.0) as u16;
        p[5] = (tgt >> 8) as u8;
        p[6] = tgt as u8;
        p[7..11].copy_from_slice(&dist.to_be_bytes());
        p[18] = (kcal >> 8) as u8;
        p[19] = kcal as u8;
        p[20..24].copy_from_slice(&((dur * 1000.0) as u32).to_be_bytes());
        p[26] = match belt {
            p::BeltState::Starting => 0x18,
            p::BeltState::Running => 0x08,
            p::BeltState::Paused => 0x10,
            p::BeltState::Stopped => 0x00,
        };
        // ASCII junk such as a device serial, bytes 40..47
        p[40..47].copy_from_slice(b"SN12345");
        p[51] = 0x43;
        p.to_vec()
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
        mkcore_with_idle(dir, 300)
    }

    fn mkcore_with_idle(dir: &std::path::Path, idle_timeout: u64) -> T {
        let (events, _ev_rx) = tokio::sync::broadcast::channel(8);
        let (_, api_rx) = mpsc::channel(32);
        let mut cfg = cfg();
        cfg.idle_timeout = idle_timeout;
        let core = Core::new(
            cfg,
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

    /// K8: a speed preset while paused must send Start(v) (kind 0x01,
    /// cmd 0x04, carrying v) — the belt starts on SetSpeed but loses the
    /// speed; the reassertion is armed like ApiCmd::Start.
    #[test]
    fn speed_while_paused_sends_start() {
        let dir = tmpdir("k8");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Paused, 0.0, 0, 0, 0, 0.0));
        c.queue_api(ApiCmd::SpeedAbs(2.5)).ok();
        // the wire packet is a Start carrying 2.5, not a SetSpeed
        let pkt = c.next_reply();
        assert_eq!(pkt[8], 0x01, "kind = start");
        assert_eq!(pkt[12], 0x04, "cmd = start");
        assert_eq!(&pkt[6..8], &2500u16.to_be_bytes());
        // and the reassertion is armed
        assert_eq!(c.pending_start_speed, Some(2.5));
    }

    /// F1 regression: the belt ignores the start command's speed and
    /// ramps to its own default. After a daemon-issued Start(v) the
    /// daemon corrects with a SetSpeed(v) on the first running packet
    /// and does NOT sync its target away from `v`.
    #[test]
    fn start_speed_is_reasserted() {
        let dir = tmpdir("startspeed");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        // user picks 2.5 (e.g. a preset), then Start
        c.queue_api(ApiCmd::SpeedAbs(2.5)).ok();
        c.queue_api(ApiCmd::Start(None)).ok();
        // the start command goes out first
        assert_eq!(c.queue.front(), Some(&WireCmd::Start(2.5)));
        let start_pkt = c.next_reply();
        assert_eq!(&start_pkt[6..8], &2500u16.to_be_bytes());
        // first running packet: the belt targets its own default 1.0
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 0.0));
        // the corrective SetSpeed is queued once, and the daemon's target
        // is NOT synced to 1.0
        assert_eq!(
            c.queue.front(),
            Some(&WireCmd::SetSpeed(2.5)),
            "belt ignored the start speed; SetSpeed queued"
        );
        assert!((c.target_speed_kmh - 2.5).abs() < 1e-9, "target stays 2.5");
        // drain: send #1 leaves the queue (the pump does this per
        // notification)
        let sent = c.next_reply();
        assert_eq!(&sent[6..8], &2500u16.to_be_bytes());
        // while suppressed, settled 1.0 reports do not sync either
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 0.5));
        assert!((c.target_speed_kmh - 2.5).abs() < 1e-9);
        // H1: within the 2 s retry gap NO second SetSpeed is queued —
        // sends are timed, not packet-counted
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 0.5));
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 0.5));
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 0.5));
        assert_eq!(c.start_correct_tries, 1, "one send so far");
        assert!(
            !c.queue.iter().any(|q| matches!(q, WireCmd::SetSpeed(_))),
            "no second SetSpeed inside the retry gap"
        );
        // once the belt reports 2.5 the sync is allowed again
        c.on_packet(&st_target(p::BeltState::Running, 2.5, 2.5, 0, 0, 0, 1.0));
        c.on_packet(&st_target(p::BeltState::Running, 2.5, 2.5, 0, 0, 0, 1.5));
        assert!(
            (c.target_speed_kmh - 2.5).abs() < 1e-9,
            "still 2.5, as asked"
        );
        assert!(c.pending_start_speed.is_none(), "correction ended");
    }

    /// H1: after the retry gap a second SetSpeed goes out; after
    /// MAX_START_CORRECT sends the correction gives up (warn path) and
    /// the suppression ends, so the settle-sync picks the belt's speed up.
    #[tokio::test(start_paused = true)]
    async fn start_speed_retry_and_giveup() {
        let dir = tmpdir("startretry");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.queue_api(ApiCmd::SpeedAbs(2.5)).ok();
        c.queue_api(ApiCmd::Start(None)).ok();
        c.next_reply(); // start command out
        // first running packet: send #1
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 0.0));
        assert_eq!(c.start_correct_tries, 1);
        // still inside the retry gap: no second send
        tokio::time::advance(Duration::from_secs(1)).await;
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 1.0));
        assert_eq!(c.start_correct_tries, 1);
        // past the gap: send #2
        tokio::time::advance(Duration::from_secs(2)).await;
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 3.0));
        assert_eq!(c.start_correct_tries, 2);
        // past the gap again: send #3 (the last, at ~5 s < the window)
        tokio::time::advance(Duration::from_secs(2)).await;
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 5.0));
        assert_eq!(c.start_correct_tries, 3, "MAX_START_CORRECT reached");
        // drain: send #3 leaves the queue
        let sent3 = c.next_reply();
        assert_eq!(&sent3[6..8], &2500u16.to_be_bytes());
        // drive past the suppression window entirely: the give-up fires
        tokio::time::advance(Duration::from_secs(SETTLE_SUPPRESS_S)).await;
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 15.0));
        assert!(c.pending_start_speed.is_none(), "gave up after MAX sends");
        assert!(c.settle_suppress_until.is_none(), "suppression ended");
        // the settle-sync picks the belt's 1.0 up as before (§3.6): the
        // settle window refills from scratch after the suppression ended
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 16.0));
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 16.5));
        c.on_packet(&st_target(p::BeltState::Running, 1.0, 1.0, 0, 0, 0, 17.0));
        assert!(
            (c.target_speed_kmh - 1.0).abs() < 1e-9,
            "the default settles in after the give-up"
        );
    }

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
    fn mid_session_reset_keeps_session_totals() {
        // REVIEW round-2 A6: a distance target set before a mid-session
        // counter reset must still trigger at the true walked distance —
        // the live session view includes the pre-reset offset.
        let dir = tmpdir("resetoff");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 30, 40, 1, 30.0));
        assert_eq!(c.queue_api(ApiCmd::TargetSet(Some(100), None)), Ok(()));
        // treadmill resets its counters mid-session, keeps running
        c.on_packet(&st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0));
        let ses = c.snapshot().session.expect("session open");
        assert_eq!(ses.distance_m, 30, "pre-reset offset reported live");
        // keeps walking: 80 m after the reset → 30 + 80 = 110 ≥ 100
        c.on_packet(&st(p::BeltState::Running, 2.0, 80, 120, 3, 160.0));
        assert_eq!(c.snapshot().target, Target::default(), "target cleared");
        assert_eq!(c.queue, VecDeque::from([WireCmd::Stop]));
        let rec = &crate::session::read_sessions(&dir, 4)[0];
        assert_eq!(rec.distance_m, 110, "record = offset + post-reset");
        assert_eq!(rec.duration_s, 190.0);
    }

    #[test]
    fn pause_boundary_duration_jitter_is_not_a_reset() {
        // Real captures (tests/captures/device-a/2026-11-17-walk-60-steps.log):
        // the belt's moving duration regresses a hair under one second at
        // the pause boundary, with the distance frozen — that is not a
        // counter reset, and treating it as one double-counted the steps
        // in the replay tests (REVIEW round-17 N3).
        let dir = tmpdir("durjit");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 1.0, 49, 101, 2, 106.999));
        c.on_packet(&st(p::BeltState::Paused, 0.0, 49, 101, 2, 106.0));
        c.on_packet(&st(p::BeltState::Running, 1.0, 119, 268, 10, 237.0));
        // the stop frame carries the walk's final counters before the
        // counters reset ~6 s later
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 119, 268, 10, 237.0));
        let recs = crate::session::read_sessions(&dir, 4);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].steps, 268, "not double-counted, not 0");
        assert_eq!(recs[0].duration_s, 237.0);
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
        /// Repeat the last packet instead of dying at script end: the
        /// idle-timeout tests need a belt that stays stopped while the
        /// timer runs out.
        loop_last: bool,
        last: Option<Vec<u8>>,
        pub written: Vec<Vec<u8>>,
        variant: p::Variant,
        /// Deadline of the pending notification, kept across a dropped
        /// call: `Notify` requires cancel-safety (§3.7).
        next_at: Option<tokio::time::Instant>,
    }

    impl FakeConn {
        fn new(script: VecDeque<Vec<u8>>, loop_last: bool) -> Self {
            Self {
                script,
                loop_last,
                last: None,
                written: vec![],
                variant: p::Variant::PitPat,
                next_at: None,
            }
        }
    }

    impl crate::link::Notify for FakeConn {
        fn address(&self) -> String {
            "00:11:22:33:44:55".to_string()
        }

        async fn disconnect(&mut self) {}

        fn variant(&self) -> p::Variant {
            self.variant
        }

        async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
            // paused-time tests advance through real waits, like the
            // simulator's 500 ms cycle
            let at = *self
                .next_at
                .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_millis(500));
            tokio::time::sleep_until(at).await;
            self.next_at = None;
            let pkt = match self.script.pop_front() {
                Some(v) => Some(v),
                None if self.loop_last => self.last.clone(),
                None => None,
            };
            match pkt {
                Some(v) => {
                    self.last = Some(v.clone());
                    Ok(v)
                }
                None => Err(LinkError::Dead("script exhausted".into())),
            }
        }

        async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
            self.written.push(pkt.to_vec());
            Ok(())
        }
    }

    // ------------------------------------------------- §4.5a/§4.1 supervise

    /// A link that reports how many times connect() was called; each
    /// variant programmes the next attempt's outcome.
    struct ScriptedLink {
        connects: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Pending forever = the wake budget must expire.
        hang: bool,
    }

    impl ScriptedLink {
        fn new(hang: bool) -> Self {
            Self {
                connects: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                hang,
            }
        }

        #[allow(dead_code)]
        fn connects(&self) -> usize {
            self.connects.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl crate::link::LinkT for ScriptedLink {
        type Conn = FakeConn;

        async fn connect(&mut self) -> Result<FakeConn, LinkError> {
            self.connects
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.hang {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            // each connection: a short run then a dead link
            Ok(FakeConn::new(
                VecDeque::from([
                    st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0),
                    st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0),
                ]),
                false,
            ))
        }
    }

    /// Collect the states the supervisor publishes, with a deadline.
    async fn wait_state(rx: &mut tokio::sync::broadcast::Receiver<Snapshot>, want: &str) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            let Ok(snap) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await else {
                return false;
            };
            match snap {
                Ok(s) if s.state.as_str() == want => return true,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Boot asleep → wake on Start → running → idle → asleep again.
    #[tokio::test(start_paused = true)]
    async fn supervise_wake_and_idle() {
        let dir = tmpdir("sup1");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 2,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let link = ScriptedLink::new(false);
        let connects = link.connects.clone();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(
                    wait_state(&mut ev_rx, "sleeping").await,
                    "daemon boots asleep"
                );
                api_tx.send(ApiCmd::Start(Some(3.0))).await.unwrap();
                assert!(
                    wait_state(&mut ev_rx, "running").await,
                    "start wakes and runs"
                );
                // idle_timeout = 2 s of stopped → asleep again
                assert!(
                    wait_state(&mut ev_rx, "sleeping").await,
                    "idles back to sleep"
                );
                sup.abort();
            })
            .await;
        assert!(
            connects.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the wake connected"
        );
    }

    /// Connect wakes without queueing a belt command; the belt is
    /// untouched and the daemon goes back to sleep after the timeout.
    #[tokio::test(start_paused = true)]
    async fn supervise_connect_no_belt_command() {
        let dir = tmpdir("sup2");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 2,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let link = ScriptedLink::new(false);
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await);
                api_tx.send(ApiCmd::Connect).await.unwrap();
                assert!(
                    wait_state(&mut ev_rx, "stopped").await,
                    "woken, belt stopped"
                );
                // no start command was sent: FakeConn dies right after the
                // first packet, but with the belt stopped the supervisor
                // sleeps again (§4.5a amended)
                assert!(wait_state(&mut ev_rx, "sleeping").await);
                sup.abort();
            })
            .await;
    }

    /// A link whose connect() never completes: after the wake budget the
    /// queued command is dropped and the daemon is asleep again.
    #[tokio::test(start_paused = true)]
    async fn supervise_wake_budget_drops_command() {
        let dir = tmpdir("sup3");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 2,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let link = ScriptedLink::new(true);
        let connects = link.connects.clone();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await);
                api_tx.send(ApiCmd::Start(Some(3.0))).await.unwrap();
                // back to sleep after the 30 s budget, command dropped.
                // Drive the clock explicitly: the hanging connect owns a
                // 3600 s timer and the observer churns 2 s recv timers.
                let mut sleeping_events = 0;
                for _ in 0..40 {
                    tokio::time::advance(Duration::from_secs(1)).await;
                    while let Ok(snap) = ev_rx.try_recv() {
                        if snap.state.as_str() == "sleeping" {
                            sleeping_events += 1;
                        }
                    }
                    if sleeping_events >= 2 {
                        break;
                    }
                }
                assert!(
                    sleeping_events >= 1,
                    "the boot sleeping event was consumed by wait_state; the
                     post-budget one must follow"
                );
                assert!(
                    sleeping_events >= 1,
                    "the boot sleeping event was consumed by wait_state; the
                     post-budget one must follow"
                );
                sup.abort();
                assert!(
                    connects.load(std::sync::atomic::Ordering::Relaxed) >= 1,
                    "the wake tried to connect"
                );
            })
            .await;
    }

    /// Link lost while stopped → sleep with no reconnect attempt (D2).
    #[tokio::test(start_paused = true)]
    async fn supervise_link_lost_while_stopped_sleeps() {
        let dir = tmpdir("sup4");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 2,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let link = ScriptedLink::new(false);
        let connects = link.connects.clone();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await);
                api_tx.send(ApiCmd::Start(Some(3.0))).await.unwrap();
                assert!(wait_state(&mut ev_rx, "running").await);
                // FakeConn's script ends: running → stopped → dead link;
                // stopped + dead → sleep with NO further connect (D2)
                assert!(wait_state(&mut ev_rx, "sleeping").await);
                let before = connects.load(std::sync::atomic::Ordering::Relaxed);
                tokio::time::advance(Duration::from_secs(10)).await;
                assert_eq!(
                    connects.load(std::sync::atomic::Ordering::Relaxed),
                    before,
                    "no reconnect while asleep"
                );
                sup.abort();
            })
            .await;
    }

    /// Link lost mid-walk with the link unavailable: the session closes
    /// as disconnected after the idle budget, then the daemon sleeps (D2).
    #[tokio::test(start_paused = true)]
    async fn supervise_link_lost_midwalk_budget() {
        let dir = tmpdir("sup5");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 2,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        // first connect: a short run then a dead link; every later
        // connect fails — the link is "unavailable" (D2)
        struct LostMidwalk {
            connects: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl crate::link::LinkT for LostMidwalk {
            type Conn = FakeConn;
            async fn connect(&mut self) -> Result<FakeConn, LinkError> {
                let n = self
                    .connects
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n == 0 {
                    Ok(FakeConn::new(
                        VecDeque::from([st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0)]),
                        false,
                    ))
                } else {
                    Err(LinkError::NotFound)
                }
            }
        }
        let link = LostMidwalk {
            connects: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let connects = link.connects.clone();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await, "boots asleep");
                api_tx.send(ApiCmd::Connect).await.unwrap();
                assert!(wait_state(&mut ev_rx, "running").await);
                // link unavailable: after the idle budget the session
                // closes as disconnected and the daemon sleeps (D2).
                // Explicit clock drive: the supervisor's backoff sleeps
                // and the observer churn otherwise interleave.
                let mut saw_sleeping = false;
                let mut saw_session = false;
                let adv = tokio::task::spawn_local(async move {
                    loop {
                        tokio::time::advance(Duration::from_millis(100)).await;
                    }
                });
                for _ in 0..600 {
                    if let Ok(snap) = ev_rx.try_recv() {
                        if snap.state.as_str() == "sleeping" {
                            saw_sleeping = true;
                        }
                        // the walk is 0.5 s / 0 m → legitimately discarded
                        // as noise, so observe the end reason via the
                        // /api/state data instead
                        if snap.last_end_reason == Some("disconnected") {
                            saw_session = true;
                        }
                    }
                    if saw_sleeping && saw_session {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert!(saw_session, "session closed as disconnected");
                assert!(saw_sleeping, "mid-walk loss sleeps after the budget");
                assert!(connects.load(std::sync::atomic::Ordering::Relaxed) >= 1);
                adv.abort();
                sup.abort();
            })
            .await;
    }

    /// Link lost mid-walk, then the link comes back: the first walk is
    /// closed as `disconnected` and recorded, and the regained link opens
    /// a new session (REVIEW round-7 E1).
    #[tokio::test(start_paused = true)]
    async fn supervise_link_lost_midwalk_regain() {
        let dir = tmpdir("sup7");
        let dir2 = dir.clone();
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 5,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        // connects alternate: running-then-die, running-and-stays
        struct RegainLink {
            connects: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl crate::link::LinkT for RegainLink {
            type Conn = FakeConn;
            async fn connect(&mut self) -> Result<FakeConn, LinkError> {
                let n = self
                    .connects
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n == 0 {
                    // run briefly (treadmill reports 10 s of moving), then
                    // the link dies
                    Ok(FakeConn::new(
                        VecDeque::from([st(p::BeltState::Running, 2.0, 0, 0, 0, 10.0)]),
                        false,
                    ))
                } else {
                    // regained: keeps running long enough to count
                    Ok(FakeConn::new(
                        VecDeque::from([st(p::BeltState::Running, 2.0, 0, 0, 0, 0.0)]),
                        true,
                    ))
                }
            }
        }
        let link = RegainLink {
            connects: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let connects = link.connects.clone();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await, "boots asleep");
                api_tx.send(ApiCmd::Connect).await.unwrap();
                assert!(wait_state(&mut ev_rx, "running").await);
                // link dies mid-walk: the first session is recorded as
                // disconnected BEFORE the regain (E1)
                assert!(wait_state(&mut ev_rx, "disconnected").await);
                let rec = &crate::session::read_sessions(&dir2, 4)[0];
                assert_eq!(rec.end_reason, "disconnected");
                // the link comes back and a new session opens
                assert!(wait_state(&mut ev_rx, "running").await);
                let sessions = crate::session::read_sessions(&dir2, 4);
                assert!(!sessions.is_empty(), "the first walk was recorded");
                sup.abort();
                let _ = connects.load(std::sync::atomic::Ordering::Relaxed);
            })
            .await;
    }

    /// K6: a read-only sessions.jsonl is detected at startup, the record
    /// is held in memory, and after the file becomes writable the next
    /// persist appends it (no restart needed).
    #[test]
    fn persist_failure_holds_and_retries() {
        let dir = tmpdir("k6");
        std::fs::create_dir_all(&dir).unwrap();
        // K6: make sessions.jsonl un-openable for append. Tests may run
        // as root, where mode bits don't stop writes — replace it with a
        // directory, which fails with EISDIR for everyone.
        std::fs::remove_dir_all(dir.join("sessions.jsonl")).ok();
        std::fs::create_dir_all(dir.join("sessions.jsonl")).unwrap();

        let (events, _ev_rx) = tokio::sync::broadcast::channel(8);
        let (_, api_rx) = mpsc::channel(8);
        let mut core = Core::new(
            cfg(),
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir.clone(),
        );
        core.check_state_dir();
        assert!(core.persist_error.is_some(), "unopenable file reported");

        // L4: persist() with nothing held and the file still broken —
        // the error must survive (no wiping by the totals save)
        core.persist();
        assert!(
            core.persist_error.is_some(),
            "the error is not cleared while the file is still broken"
        );

        // a session ends while the file can't be appended: held, not lost
        core.begin_connection(None);
        core.on_packet(&st(p::BeltState::Running, 2.0, 100, 140, 5, 100.0));
        core.on_disconnect(DeadReason::Link);
        assert!(
            !core.pending_records.is_empty(),
            "the record is held in memory"
        );
        // held records don't double-count sessions
        assert_eq!(core.totals.sessions, 0, "counted on save, not on hold");
        // L4: every Shutdown exit dumps the held records into the log,
        // even without a live link (on_disconnect covers them all)
        core.on_disconnect(DeadReason::Shutdown);
        assert!(
            core.pending_records.is_empty(),
            "shutdown drains the held records"
        );
        // a second walk is held and then really saved by the retry below
        core.begin_connection(None);
        core.on_packet(&st(p::BeltState::Running, 2.0, 100, 140, 5, 100.0));
        core.on_disconnect(DeadReason::Link);
        assert!(!core.pending_records.is_empty(), "held again for the retry");

        // fix it: replace the impostor directory with a real file
        std::fs::remove_dir_all(dir.join("sessions.jsonl")).unwrap();
        std::fs::write(dir.join("sessions.jsonl"), b"").unwrap();
        core.persist();
        assert!(core.pending_records.is_empty(), "retried and saved");
        assert!(core.persist_error.is_none());
        assert_eq!(core.totals.sessions, 1);
        let recs = crate::session::read_sessions(&dir, 4);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].end_reason, "disconnected");
    }

    /// K7/L5: the connect frame (`68 34`) has no steps value; the running
    /// `68 32` frame does. The core keeps the last count across frames
    /// without one, and a missing value must not trip the counter-reset
    /// path (a drop to 0 would re-add the old count as an offset).
    #[test]
    fn steps_frame_kinds_and_stale_steps() {
        let dir = tmpdir("steps43");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        c.begin_connection(None);
        // the connect frame yields steps: None
        let frame = st_connect_frame(p::BeltState::Stopped, 0.0, 0.0, 0, 0, 0.0);
        assert_eq!(p::decode_status(&frame).unwrap().steps, None);
        c.on_packet(&frame);
        c.on_packet(&st_target(p::BeltState::Running, 2.0, 2.0, 0, 0, 0, 0.0));
        // a running 68 32 frame carries 268 steps (the previous walk's end)
        let mut with_steps = st_target(p::BeltState::Running, 2.0, 2.0, 0, 0, 0, 0.0);
        with_steps[1] = 0x32;
        with_steps[43] = 1;
        with_steps[44] = 12; // 268
        c.on_packet(&with_steps);
        let ses = c.snapshot().session.expect("session open");
        assert_eq!(ses.steps, 268, "steps from u16 @43");
        // a short frame also yields steps: None
        let short = with_steps[..31].to_vec();
        assert_eq!(p::decode_status(&short).unwrap().steps, None);
        // a follow-up 68 34 frame keeps the last value, no reset trip
        let frame = st_connect_frame(p::BeltState::Running, 2.0, 2.0, 5, 0, 1.0);
        assert_eq!(p::decode_status(&frame).unwrap().steps, None);
        c.on_packet(&frame);
        assert_eq!(
            c.snapshot().session.as_ref().unwrap().steps,
            268,
            "missing steps value keeps the last count"
        );
        // stop resets the counters: the first running frame of the new
        // walk still says 268 and the next one drops to 0 — a lone steps
        // drop with distance/duration growing is a stale carry-over, not
        // a reset, so the new session must not gain the old 268 (L5)
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        c.on_packet(&st_target(p::BeltState::Running, 2.0, 2.0, 0, 0, 0, 0.0));
        let mut stale = st_target(p::BeltState::Running, 2.0, 2.0, 0, 0, 0, 0.5);
        stale[1] = 0x32;
        stale[43] = 1;
        stale[44] = 12; // still 268
        c.on_packet(&stale);
        assert_eq!(
            c.snapshot().session.as_ref().unwrap().steps,
            268,
            "the stale steps are shown until the fresh value arrives"
        );
        let mut fresh = st_target(p::BeltState::Running, 2.0, 2.0, 0, 0, 0, 1.0);
        fresh[1] = 0x32;
        fresh[43] = 0;
        fresh[44] = 0; // the treadmill reset steps by the next frame
        c.on_packet(&fresh);
        assert_eq!(
            c.snapshot().session.as_ref().unwrap().steps,
            0,
            "re-baselined without adding the old 268 as an offset"
        );
        assert_eq!(c.session_offset.steps, 0, "no offset inflation");
        // the totals never double-count: a reconnect re-baselines the
        // deltas (§4.4), so the stale 268 belongs to the old session
        assert_eq!(
            c.snapshot().session.as_ref().unwrap().steps,
            0,
            "the fresh value is mirrored"
        );
    }

    /// G2 end-to-end: sleeping → the user picks 2.5 → Start → the belt
    /// ramps to its own 1.0 default, the daemon re-asserts 2.5 with a
    /// SetSpeed retry, and the target stays 2.5.
    #[tokio::test(start_paused = true)]
    async fn supervise_start_speed_reasserted() {
        let dir = tmpdir("sup8");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 30,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let sim = crate::sim::SimLink::new(80);
        let link = crate::link::Link::Sim(sim.clone());
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await, "boots asleep");

                // the user picks 2.5, then Start (both from the widget)
                api_tx.send(ApiCmd::SpeedAbs(2.5)).await.unwrap();
                api_tx.send(ApiCmd::Start(None)).await.unwrap();

                assert!(wait_state(&mut ev_rx, "running").await, "woken and running");
                // the belt ramps to its own 1.0; the daemon re-asserts 2.5
                // and the belt follows. The user-visible part of the bug
                // (round-10/11): after the Start, every running snapshot
                // must show the requested target 2.5 — a snapshot with a
                // different target fails the test instead of just not
                // counting as settled.
                let mut settled = false;
                for _ in 0..300 {
                    tokio::time::advance(Duration::from_millis(100)).await;
                    while let Ok(snap) = ev_rx.try_recv() {
                        if snap.state.as_str() == "running" {
                            assert!(
                                (snap.target_speed_kmh - 2.5).abs() < 1e-9,
                                "target drifted to {} after the Start",
                                snap.target_speed_kmh
                            );
                        }
                        if (snap.speed_kmh - 2.5).abs() < 1e-6 {
                            settled = true;
                        }
                    }
                    if settled {
                        break;
                    }
                }
                assert!(settled, "belt settled at the requested 2.5 with target 2.5");
                sup.abort();
            })
            .await;
    }

    /// I3: SIGTERM while running records the open session as `shutdown`
    /// and writes totals.json (verified on the simulator by the user;
    /// pinned here).
    #[tokio::test(start_paused = true)]
    async fn supervise_shutdown_records_session() {
        let dir = tmpdir("sup9");
        let dir2 = dir.clone();
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 0,
                ..cfg()
            }, // always connected
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let sim = crate::sim::SimLink::new(80);
        let link = crate::link::Link::Sim(sim.clone());
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(
                    wait_state(&mut ev_rx, "stopped").await,
                    "boots connected (idle_timeout = 0)"
                );
                api_tx.send(ApiCmd::Start(Some(3.0))).await.unwrap();
                assert!(wait_state(&mut ev_rx, "running").await);
                // let the belt walk a while (a flag breaks both loops)
                'walk: for _ in 0..60 {
                    tokio::time::advance(Duration::from_secs(1)).await;
                    while let Ok(snap) = ev_rx.try_recv() {
                        if snap.session.is_some_and(|s| s.distance_m > 20) {
                            break 'walk;
                        }
                    }
                }
                // SIGTERM
                ctrl_tx.send(Ctrl::Shutdown).await.unwrap();
                let sup_result = sup.await;
                assert!(
                    sup_result.is_ok(),
                    "supervise finished on shutdown without panicking"
                );
                // the session is recorded as shutdown with a real distance
                let recs = crate::session::read_sessions(&dir2, 4);
                assert!(!recs.is_empty(), "a session was written");
                assert_eq!(recs[0].end_reason, "shutdown");
                assert!(recs[0].distance_m > 0);
                // totals.json exists with sessions == 1
                let totals = crate::session::load_totals(&dir2);
                assert_eq!(totals.sessions, 1);
                // the sim belt is stopped (counters reset)
                let mut c = sim.connect().await.unwrap();
                let pkt = c.recv_notification().await.unwrap();
                assert_eq!(
                    crate::protocol::decode_status(&pkt).unwrap().belt_state(),
                    crate::protocol::BeltState::Stopped
                );
            })
            .await;
    }

    /// I3: shutdown during the mid-walk reconnect backoff must not write
    /// the (already closed) session twice.
    #[tokio::test(start_paused = true)]
    async fn supervise_shutdown_during_reconnect_writes_once() {
        let dir = tmpdir("sup10");
        let dir2 = dir.clone();
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 60,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        // one short walk then the link is gone for good
        struct LostForGood;
        impl crate::link::LinkT for LostForGood {
            type Conn = FakeConn;
            async fn connect(&mut self) -> Result<FakeConn, LinkError> {
                Ok(FakeConn::new(
                    VecDeque::from([st(p::BeltState::Running, 2.0, 0, 0, 0, 12.0)]),
                    false,
                ))
            }
        }
        let link = LostForGood;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await, "boots asleep");
                api_tx.send(ApiCmd::Connect).await.unwrap();
                assert!(wait_state(&mut ev_rx, "running").await);
                // the link dies mid-walk; the supervisor reconnects with a
                // long budget (idle_timeout = 60)
                assert!(wait_state(&mut ev_rx, "disconnected").await);
                // SIGTERM during the reconnect backoff
                ctrl_tx.send(Ctrl::Shutdown).await.unwrap();
                let sup_result = sup.await;
                assert!(sup_result.is_ok());
                // exactly one session record, already closed as
                // disconnected, NOT written again as shutdown
                let recs = crate::session::read_sessions(&dir2, 10);
                assert_eq!(recs.len(), 1, "no duplicate record");
                assert_eq!(recs[0].end_reason, "disconnected");
            })
            .await;
    }

    /// Remote start while sleeping (§4.5a): the wake credits the walk once.
    #[tokio::test(start_paused = true)]
    async fn supervise_remote_start_while_sleeping() {
        let dir = tmpdir("sup6");
        let (events, mut ev_rx) = tokio::sync::broadcast::channel(64);
        let (api_tx, api_rx) = mpsc::channel(8);
        let (_ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let core = Core::new(
            DaemonCfg {
                idle_timeout: 3,
                ..cfg()
            },
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        let sim = crate::sim::SimLink::new(80);
        let link = crate::link::Link::Sim(sim.clone());
        let core_metrics = core.metrics.clone();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sup = tokio::task::spawn_local(supervise(core, link, ctrl_rx));
                assert!(wait_state(&mut ev_rx, "sleeping").await);
                // walk with the remote while asleep: counters already at
                // 2.5 km / 3500 steps when the daemon wakes
                sim.remote_start_with(1.5, 2500, 3500, 30, 1200.0);
                let adv = tokio::task::spawn_local(async move {
                    loop {
                        tokio::time::advance(Duration::from_millis(100)).await;
                    }
                });
                // wake via /api/connect (no belt command)
                api_tx.send(ApiCmd::Connect).await.unwrap();
                assert!(
                    wait_state(&mut ev_rx, "running").await,
                    "woken into the remote-started walk"
                );
                let d0 = core_metrics.distance_m_total.get();
                assert!(d0 > 0, "walk credited");
                sup.abort();
                adv.abort();
            })
            .await;
    }

    // ------------------------------------------------------ §4.5a sleep

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_puts_the_pump_to_sleep() {
        let dir = tmpdir("idle");
        let t = mkcore_with_idle(&dir, 2); // 2 s of stopped = sleep
        let mut core = t.core;
        let (_ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let mut conn = FakeConn::new(VecDeque::new(), true);
        conn.script
            .push_back(st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        core.begin_connection(None);
        let reason = pump(&mut core, &mut conn, &mut ctrl_rx).await;
        assert_eq!(reason, DeadReason::Idle);
        conn.disconnect().await;
        core.enter_sleep();
        assert_eq!(core.state, State::Sleeping);
        assert!(!core.snapshot().connected);
        // entering sleep drops the queued command (§4.5a)
        core.queue_api(ApiCmd::Start(Some(2.0))).ok();
        core.enter_sleep();
        assert!(core.queue.is_empty(), "wake timeout drops the command");
    }

    #[tokio::test(start_paused = true)]
    async fn wake_on_start_runs_the_belt() {
        // full simulated sleep → wake on start → running: the link is
        // re-established (via SimLink, real notifications) and the queued
        // start goes out on the first notification cycle
        let dir = tmpdir("wake");
        let (events, _ev) = tokio::sync::broadcast::channel(8);
        let (api_tx, api_rx) = mpsc::channel(8);
        let mut core = Core::new(
            cfg(),
            Totals::default(),
            Arc::new(Metrics::new()),
            events,
            api_rx,
            dir,
        );
        core.cfg.idle_timeout = 2;
        core.enter_sleep();
        assert_eq!(core.state, State::Sleeping);
        let _ = &api_tx; // the daemon would hold this sender

        // the client starts the belt while asleep
        core.queue_api(ApiCmd::Start(Some(3.0))).ok();
        assert!(core.take_wake(), "start while sleeping must request a wake");

        // the daemon reconnects and pumps; the scripted connection shows
        // a running belt so the queued start goes out on the reply cycle
        core.begin_connection(None);
        core.arm_late_credit();
        let mut conn = FakeConn::new(VecDeque::new(), true);
        conn.script
            .push_back(st(p::BeltState::Running, 3.0, 0, 0, 0, 0.0));
        conn.script
            .push_back(st(p::BeltState::Running, 3.0, 1, 2, 0, 0.5));
        conn.script
            .push_back(st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        let (mut ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let _ = &mut ctrl_tx;
        let reason = pump(&mut core, &mut conn, &mut ctrl_rx).await;
        assert_eq!(reason, DeadReason::Idle, "belt stopped again, idles");
        assert!(
            conn.written
                .iter()
                .any(|p| p.len() == 23 && p[8] == 0x01 && p[12] == 0x04),
            "the queued start went out: {:?}",
            conn.written
        );
    }

    #[test]
    fn late_credit_runs_once_and_is_capped() {
        let dir = tmpdir("credit");
        let mut t = mkcore_with_idle(&dir, 300);
        let c = &mut t.core;
        c.begin_connection(None);
        c.arm_late_credit();
        // the belt was remote-started while asleep: counters already at 50 m
        c.on_packet(&st(p::BeltState::Running, 2.0, 50, 70, 5, 60.0));
        assert_eq!(c.snapshot().last_end_reason, None);
        let ses = c.snapshot().session.expect("session opens");
        assert_eq!(ses.distance_m, 50, "mirrored counters are the live view");
        assert_eq!(c.totals.distance_m, 50, "credited once");
        // further packets must not double-credit
        c.on_packet(&st(p::BeltState::Running, 2.0, 60, 80, 6, 65.0));
        assert_eq!(c.totals.distance_m, 60, "delta accounting continues");
        // a plain reconnect does NOT credit (continuation baseline §4.4)
        c.on_disconnect(DeadReason::Link);
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 100, 120, 8, 100.0));
        assert_eq!(c.totals.distance_m, 60, "no double counting");
        // cap: absurd late-credit values clamp at 20 km / 30 000 steps
        // (daemon order: enter_sleep → begin_connection → arm)
        c.on_disconnect(DeadReason::Link); // end the open session first
        c.enter_sleep();
        c.begin_connection(None);
        c.arm_late_credit();
        c.on_packet(&st(p::BeltState::Running, 2.0, 40_000, 50_000, 9, 200.0));
        assert_eq!(c.totals.distance_m, 60 + 20_000, "distance capped");
        assert_eq!(c.totals.steps, 80 + 30_000, "steps capped");
    }

    #[test]
    fn sleep_refused_while_moving() {
        let mut t = mkcore(&tmpdir("sleep409"));
        let c = &mut t.core;
        c.begin_connection(None);
        c.on_packet(&st(p::BeltState::Running, 2.0, 5, 10, 1, 10.0));
        assert_eq!(
            c.queue_api(ApiCmd::Sleep),
            Err(ApiErr::Disabled),
            "409 while running"
        );
        c.on_packet(&st(p::BeltState::Stopped, 0.0, 0, 0, 0, 0.0));
        assert_eq!(c.queue_api(ApiCmd::Sleep), Ok(()));
        assert!(c.take_sleep());
    }

    #[tokio::test]
    async fn pump_writes_reply_per_notification() {
        let dir = tmpdir("pump");
        let t = mkcore(&dir);
        let (_ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(4); // no control traffic
        let mut conn = FakeConn::new(VecDeque::new(), false);

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
        // F1: the first running packet showed the belt targeting its own
        // default (2.0), so the corrective SetSpeed(2.5) followed
        assert_eq!(
            &conn.written[2][6..8],
            &(2500u16).to_be_bytes(),
            "corrective set-speed"
        );
        assert_eq!(conn.written[2][12], 0x04);
        assert_eq!(conn.written[2][8], 0x05);

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

    // ------------------------------------------------------ §3.7 SupeRun

    fn superun_st(seq: u8, belt: p::BeltState, speed: f64, dist: u32, steps: u32) -> Vec<u8> {
        p::encode_superun_status(seq, belt, speed, speed, dist, steps, 0, 0.0)
    }

    /// SupeRun links get no reply per notification: a 200 ms timer writes
    /// framed heartbeats with a per-connection sequence number, and a
    /// queued command goes out framed on the next tick.
    #[tokio::test(start_paused = true)]
    async fn pump_superun_writes_on_timer() {
        let dir = tmpdir("pump-superun");
        let t = mkcore(&dir);
        let (_ctrl_tx, mut ctrl_rx) = mpsc::channel::<Ctrl>(4);
        let mut conn = FakeConn::new(
            VecDeque::from([
                superun_st(1, p::BeltState::Stopped, 0.0, 0, 0),
                superun_st(2, p::BeltState::Stopped, 0.0, 0, 0),
            ]),
            false,
        );
        conn.variant = p::Variant::SupeRun;
        let mut core = t.core;
        let core = &mut core;
        assert_eq!(core.begin_link(&conn), "00:11:22:33:44:55");
        assert_eq!(core.variant(), p::Variant::SupeRun);
        core.on_packet(&superun_st(0, p::BeltState::Stopped, 0.0, 0, 0));
        core.queue_api(ApiCmd::Start(Some(2.0))).ok();

        let reason = pump(core, &mut conn, &mut ctrl_rx).await;
        assert_eq!(reason, DeadReason::Link);
        // 2 notifications at 500 ms, the link dies at 1.5 s: ~8 ticks
        assert!(conn.written.len() >= 7, "{} writes", conn.written.len());
        for (i, w) in conn.written.iter().enumerate() {
            assert_eq!(&w[..3], &[0x4d, 0x00, i as u8], "frame + seq {i}");
            assert_eq!(w[3] as usize, w.len() - 4, "frame length");
        }
        // the queued start went out first, SupeRun-encoded
        let start =
            p::Variant::SupeRun.frame(0, &p::encode_superun_command(p::Command::Start, 2.0));
        assert_eq!(conn.written[0], start);
        assert_eq!(&conn.written[1][4..], &p::HEARTBEAT);
        // a new connection starts the sequence again
        core.begin_link(&conn);
        assert_eq!(core.wire(&p::HEARTBEAT)[2], 0);
    }

    /// SupeRun status frames reach the session in SI units, steps @43 of
    /// the `68 2f` frames included.
    #[test]
    fn superun_session_counters() {
        let dir = tmpdir("superun-session");
        let mut t = mkcore(&dir);
        let c = &mut t.core;
        let mut conn = FakeConn::new(VecDeque::new(), false);
        conn.variant = p::Variant::SupeRun;
        c.begin_link(&conn);
        c.on_packet(&superun_st(1, p::BeltState::Running, 3.0, 0, 0));
        c.on_packet(&superun_st(2, p::BeltState::Running, 3.0, 110, 150));
        let ses = c.snapshot().session.expect("session open");
        assert_eq!((ses.distance_m, ses.steps), (110, 150));
        assert!((c.speed_kmh - 3.0).abs() < 0.01, "{}", c.speed_kmh);
        assert!(!c.snapshot().imperial);
        // a classic packet on a SupeRun link is not decoded
        c.on_packet(&st(p::BeltState::Running, 5.0, 999, 999, 0, 0.0));
        assert_eq!(c.snapshot().session.unwrap().distance_m, 110);
    }

    /// Shutdown on a SupeRun link: the stop goes out on the keep-alive
    /// timer, without waiting for a notification.
    #[tokio::test(start_paused = true)]
    async fn superun_shutdown_sends_stop_on_timer() {
        let dir = tmpdir("superun-shutdown");
        let t = mkcore(&dir);
        let mut core = t.core;
        let mut conn = FakeConn::new(VecDeque::new(), false);
        conn.variant = p::Variant::SupeRun;
        core.begin_link(&conn);
        core.on_packet(&superun_st(1, p::BeltState::Running, 3.0, 50, 70));
        assert!(core.moving());
        let t0 = tokio::time::Instant::now();
        shutdown_flow(&mut core, &mut conn).await;
        assert!(!conn.written.is_empty());
        let stop = p::encode_superun_command(p::Command::Stop, 3.0);
        assert_eq!(&conn.written[0][4..], &stop, "stop first");
        assert!(tokio::time::Instant::now() - t0 <= Duration::from_secs(4));
    }
}

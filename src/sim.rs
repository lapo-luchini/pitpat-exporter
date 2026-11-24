// SPDX-License-Identifier: ISC

use crate::link::LinkError;
use crate::protocol::{BeltState, Command, Variant};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Simulated treadmill (§4.7): emits a real 31-byte notification every
/// 500 ms (so the decoder path is exercised), reacts to commands like a
/// real treadmill (start → 3 s starting → running ramping at 0.5 km/h/s;
/// counters advance from the speed; stop resets counters), and can be
/// told to drop the link (daemon wires SIGUSR1 into `request_drop`) to
/// exercise the reconnect logic. With `Variant::SupeRun` it speaks the
/// SupeRun framing and units instead (§3.7, `--simulate-model superun`).
#[derive(Clone)]
pub struct SimLink {
    weight_kg: u8,
    variant: Variant,
    drop: Arc<AtomicBool>,
    /// Shared across connects: the treadmill persists between BLE
    /// sessions (§4.5a tests need a belt started by the "remote" while
    /// no one is connected).
    belt: Arc<std::sync::Mutex<SimBelt>>,
}

pub struct SimConn {
    /// The simulated treadmill's fixed address (§6.2 device view).
    pub address: String,
    pub variant: Variant,
    belt: Arc<std::sync::Mutex<SimBelt>>,
    weight_kg: u8,
    drop: Arc<AtomicBool>,
    last_tick: Option<Instant>,
    /// When the pending notification is due: kept across a dropped
    /// `recv_notification` call so it stays cancel-safe (§3.7).
    next_at: Option<Instant>,
    /// SupeRun: the treadmill's own frame sequence number.
    seq: u8,
}

struct SimBelt {
    state: BeltState,
    target: f64,
    speed: f64,
    distance_m: u32,
    /// fractional part below 1 m (counters advance <1 m per tick)
    distance_f: f64,
    steps: u32,
    /// fractional steps/kcal: whole-unit truncation would stay at 0
    /// forever at ~0.4 m per tick (REVIEW item 7)
    steps_f: f64,
    kcal: u16,
    kcal_f: f64,
    duration_s: f64,
    starting_ends_at: Option<Instant>,
    /// §4.5a: when the remote started the belt with nobody connected,
    /// counters catch up to real elapsed time on the first tick.
    remote_running_since: Option<Instant>,
    pending: Option<Command>,
    commanded_speed: f64,
}

impl SimLink {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(weight_kg: u8) -> Self {
        Self::with_variant(weight_kg, Variant::PitPat)
    }

    pub fn with_variant(weight_kg: u8, variant: Variant) -> Self {
        Self {
            weight_kg,
            variant,
            drop: Arc::new(AtomicBool::new(false)),
            belt: Arc::new(std::sync::Mutex::new(SimBelt::new())),
        }
    }

    /// §4.5a hook (tests + hand-testing): the belt is started with the
    /// physical remote while the daemon is asleep/disconnected. Counters
    /// advance on the next ticks of whichever connection exists.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn remote_start(&self, speed_kmh: f64) {
        self.remote_start_with(speed_kmh, 0, 0, 0, 0.0);
    }

    /// §4.5a hook with explicit counters: a remote-started walk has been
    /// going for a while, so the belt's offline counters are nonzero.
    /// `distance_m`/`steps`/`kcal`/`duration_s` are the values the next
    /// connection will report immediately.
    #[allow(clippy::too_many_arguments)]
    pub fn remote_start_with(
        &self,
        speed_kmh: f64,
        distance_m: u32,
        steps: u32,
        kcal: u16,
        duration_s: f64,
    ) {
        let mut belt = self.belt.lock().expect("sim belt");
        belt.distance_m = distance_m;
        belt.distance_f = distance_m as f64;
        belt.steps = steps;
        belt.steps_f = steps as f64;
        belt.kcal = kcal;
        belt.kcal_f = kcal as f64;
        belt.duration_s = duration_s;
        belt.state = BeltState::Running;
        belt.commanded_speed = speed_kmh;
        belt.target = speed_kmh;
        belt.speed = speed_kmh;
        belt.starting_ends_at = None;
        belt.pending = None;
        belt.remote_running_since = Some(Instant::now());
    }

    /// The shared drop flag so a signal task can flip it from elsewhere.
    pub fn drop_flag(&self) -> Arc<AtomicBool> {
        self.drop.clone()
    }

    pub async fn connect(&self) -> Result<SimConn, LinkError> {
        // a fresh connection revives the link (SIGUSR1 was just a probe)
        self.drop.store(false, Ordering::Relaxed);
        Ok(SimConn {
            address: "00:11:22:33:44:55".to_string(),
            variant: self.variant,
            belt: self.belt.clone(),
            weight_kg: self.weight_kg,
            drop: self.drop.clone(),
            last_tick: None,
            next_at: None,
            seq: 0,
        })
    }
}

impl SimBelt {
    fn new() -> Self {
        Self {
            state: BeltState::Stopped,
            target: 1.0,
            speed: 0.0,
            distance_m: 0,
            distance_f: 0.0,
            steps: 0,
            steps_f: 0.0,
            kcal: 0,
            kcal_f: 0.0,
            duration_s: 0.0,
            starting_ends_at: None,
            remote_running_since: None,
            pending: None,
            commanded_speed: 1.0,
        }
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Start => {
                // F1: the real treadmill ignores the start command's speed
                // field and always ramps to its own default (min speed);
                // the daemon corrects with a SetSpeed afterwards
                self.target = 1.0;
                if self.state != BeltState::Running && self.state != BeltState::Starting {
                    self.starting_ends_at =
                        Some(tokio::time::Instant::now() + Duration::from_secs(3));
                    self.state = BeltState::Starting;
                    self.speed = 0.0;
                }
            }
            Command::SetSpeed => {
                // G2: the pessimistic hardware assumption — set-speed is
                // ignored while the starting countdown runs; only Running
                // packets accept it (this exercises the daemon's retry)
                if self.state == BeltState::Running {
                    self.target = self.commanded_speed;
                }
            }
            Command::Pause => {
                if self.state == BeltState::Running || self.state == BeltState::Starting {
                    self.state = BeltState::Paused;
                    self.speed = 0.0;
                }
            }
            Command::Stop => {
                self.state = BeltState::Stopped;
                self.speed = 0.0;
                self.distance_m = 0;
                self.distance_f = 0.0;
                self.steps = 0;
                self.steps_f = 0.0;
                self.kcal = 0;
                self.kcal_f = 0.0;
                self.duration_s = 0.0;
            }
        }
    }

    fn tick(&mut self, dt: Duration, weight_kg: u8) -> (BeltState, u32, u32, u16, f64) {
        if let Some(cmd) = self.pending.take() {
            self.apply(cmd);
        }
        let dt_s = dt.as_secs_f64();

        match self.state {
            BeltState::Starting
                if self
                    .starting_ends_at
                    .is_some_and(|t| tokio::time::Instant::now() >= t) =>
            {
                self.state = BeltState::Running;
                self.speed = 0.0;
            }
            BeltState::Running => {
                // ramp at 0.5 km/h per second toward the target
                let step = 0.5 * dt_s;
                self.speed = if self.speed < self.target {
                    (self.speed + step).min(self.target)
                } else {
                    (self.speed - step).max(self.target)
                };
                // speed in km/h, dt in s → metres: 1 km/h for 1 s is
                // 1000/3600 m
                let metres = self.speed * dt_s * 1000.0 / 3600.0;
                self.distance_f += metres;
                self.distance_m = self.distance_f as u32;
                // steps ≈ distance / 0.7 m (§4.7)
                self.steps_f += metres / 0.7;
                self.steps = self.steps_f as u32;
                // kcal ≈ 0.5 × weight_kg × km
                self.kcal_f += 0.5 * weight_kg as f64 * metres / 1000.0;
                self.kcal = self.kcal_f as u16;
                self.duration_s += dt_s;
            }
            _ => {}
        }

        (
            self.state,
            self.distance_m,
            self.steps,
            self.kcal,
            self.duration_s,
        )
    }
}

/// Parse a command packet back into (command, speed_kmh × retries).
fn parse_command(pkt: &[u8]) -> Option<(Command, f64)> {
    if pkt.len() != 23 || pkt[0] != 0x6a || pkt[22] != 0x43 {
        return None;
    }
    let speed = u16::from_be_bytes([pkt[6], pkt[7]]) as f64 / 1000.0;
    let cmd = match pkt[12] {
        0x04 => {
            if pkt[8] == 0x05 {
                Command::SetSpeed
            } else {
                Command::Start
            }
        }
        0x02 => Command::Pause,
        0x00 => Command::Stop,
        _ => return None,
    };
    Some((cmd, speed))
}

/// Parse an unframed SupeRun command packet (§3.7) the same way.
fn parse_superun_command(pkt: &[u8]) -> Option<(Command, f64)> {
    if pkt.len() != 23 || pkt[0] != 0x6a || pkt[22] != 0x43 {
        return None;
    }
    let speed = u16::from_be_bytes([pkt[6], pkt[7]]) as f64 / 1600.0 * 1.609344;
    let cmd = match pkt[12] {
        0x0c if pkt[8] == 0x01 => Command::SetSpeed,
        0x0c => Command::Start,
        0x0a => Command::Pause,
        0x08 => Command::Stop,
        _ => return None,
    };
    Some((cmd, speed))
}

pub const NOTIFY_PERIOD: Duration = Duration::from_millis(500);

impl SimConn {
    /// Drop the link explicitly: the simulator has no LE link, but the
    /// flag is armed again so a reconnect starts fresh.
    pub async fn disconnect(&mut self) {}

    pub async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
        // cancel-safe (§3.7): a dropped call keeps its deadline
        let at = *self
            .next_at
            .get_or_insert_with(|| Instant::now() + NOTIFY_PERIOD);
        tokio::time::sleep_until(at).await;
        self.next_at = None;
        if self.drop.load(Ordering::Relaxed) {
            return Err(LinkError::Dead("simulated link dropped (SIGUSR1)".into()));
        }
        let tick = Instant::now();
        // Counters advance from real elapsed time, but a burst of lost
        // ticks must not redeem itself at once (the Starting/Stopped time
        // does not count toward the moving counters anyway).
        let dt = self
            .last_tick
            .replace(tick)
            .map_or(NOTIFY_PERIOD, |t| tick.duration_since(t))
            .min(NOTIFY_PERIOD);
        let (state, dist, steps, kcal, dur) = {
            let mut belt = self.belt.lock().expect("sim belt");
            // a remote-started belt kept counting while we slept (§4.5a)
            if let Some(since) = belt.remote_running_since.take() {
                if belt.state == BeltState::Running {
                    let offline = tick.duration_since(since).as_secs_f64().min(3600.0);
                    let metres = belt.speed * offline * 1000.0 / 3600.0;
                    belt.distance_f += metres;
                    belt.distance_m = belt.distance_f as u32;
                    belt.steps_f += metres / 0.7;
                    belt.steps = belt.steps_f as u32;
                    belt.kcal_f += 0.5 * self.weight_kg as f64 * metres / 1000.0;
                    belt.kcal = belt.kcal_f as u16;
                    belt.duration_s += offline;
                }
            }
            belt.tick(dt, self.weight_kg)
        };
        let (speed, target) = {
            let belt = self.belt.lock().expect("sim belt");
            (belt.speed, belt.target)
        };
        Ok(match self.variant {
            Variant::PitPat => {
                crate::protocol::encode_status(state, false, speed, target, dist, steps, kcal, dur)
                    .to_vec()
            }
            Variant::SupeRun => {
                self.seq = self.seq.wrapping_add(1);
                crate::protocol::encode_superun_status(
                    self.seq, state, speed, target, dist, steps, kcal, dur,
                )
            }
        })
    }

    pub async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
        let (pkt, parsed) = match self.variant {
            Variant::PitPat => (pkt, parse_command(pkt)),
            Variant::SupeRun => {
                // an unframed packet is ignored, like the hardware would
                let Some(inner) = crate::protocol::superun_unframe(pkt) else {
                    return Ok(());
                };
                (inner, parse_superun_command(inner))
            }
        };
        if pkt.len() == 5 {
            return Ok(()); // heartbeat: nothing to apply
        }
        if let Some((cmd, speed)) = parsed {
            let mut belt = self.belt.lock().expect("sim belt");
            belt.commanded_speed = speed;
            belt.pending = Some(cmd);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queue a start with 3.0 km/h on a raw connection and check that the
    /// belt reaches `running` and the ramp ends at the target speed.
    #[tokio::test(start_paused = true)]
    async fn start_rampes_to_target() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();

        c.recv_notification().await.unwrap(); // stopped packet
        c.write_packet(&crate::protocol::encode_command(Command::Start, 3.0, 80, 1))
            .await
            .unwrap();

        // ~3 s starting at some point; poll until running
        let mut saw_running = false;
        for _ in 0..40 {
            let pkt = c.recv_notification().await.unwrap();
            let st = crate::protocol::decode_status(&pkt).unwrap();
            if st.belt_state() == BeltState::Running {
                saw_running = true;
                break;
            }
        }
        assert!(saw_running, "sim never reached running");

        // F1: the belt ignores the start speed and ramps to its own
        // default (min speed 1.0), whatever the start command carried
        for _ in 0..40 {
            let pkt = c.recv_notification().await.unwrap();
            let st = crate::protocol::decode_status(&pkt).unwrap();
            if (st.speed_mps * 3.6 - 1.0).abs() < 1e-9 {
                assert!(
                    (st.target_speed_kmh - 1.0).abs() < 1e-9,
                    "target @5 should read 1.0"
                );
                return; // settled at the belt's own default
            }
        }
        panic!("speed never settled at the belt's default");
    }

    /// After the belt's default ramp a SetSpeed applies normally (that is
    /// how the daemon corrects the start speed, F1).
    #[tokio::test(start_paused = true)]
    async fn set_speed_after_start_applies() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();
        c.recv_notification().await.unwrap(); // stopped packet
        // one packet per notification cycle: start first, then the
        // daemon's corrective SetSpeed arrives with the next notification
        c.write_packet(&crate::protocol::encode_command(Command::Start, 3.0, 80, 1))
            .await
            .unwrap();
        let starting = c.recv_notification().await.unwrap();
        assert_eq!(
            crate::protocol::decode_status(&starting)
                .unwrap()
                .belt_state(),
            BeltState::Starting
        );
        // G2: during the countdown SetSpeed is ignored (the pessimistic
        // hardware assumption) and consumed
        c.write_packet(&crate::protocol::encode_command(
            Command::SetSpeed,
            3.0,
            80,
            1,
        ))
        .await
        .unwrap();
        let pkt = c.recv_notification().await.unwrap();
        let st = crate::protocol::decode_status(&pkt).unwrap();
        if st.belt_state() == BeltState::Starting {
            assert!(
                (st.target_speed_kmh - 1.0).abs() < 1e-9,
                "still the default"
            );
        }
        // wait out the countdown, re-send once running, and it applies
        for _ in 0..30 {
            let p = c.recv_notification().await.unwrap();
            if crate::protocol::decode_status(&p).unwrap().belt_state() == BeltState::Running {
                break;
            }
        }
        c.write_packet(&crate::protocol::encode_command(
            Command::SetSpeed,
            3.0,
            80,
            1,
        ))
        .await
        .unwrap();
        for _ in 0..60 {
            let pkt = c.recv_notification().await.unwrap();
            let st = crate::protocol::decode_status(&pkt).unwrap();
            if (st.speed_mps * 3.6 - 3.0).abs() < 1e-9 {
                assert!((st.target_speed_kmh - 3.0).abs() < 1e-9, "target @5 = 3.0");
                return;
            }
        }
        panic!("SetSpeed after start never applied");
    }

    #[tokio::test(start_paused = true)]
    async fn stop_res_counters() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();
        c.recv_notification().await.unwrap();
        c.write_packet(&crate::protocol::encode_command(Command::Start, 6.0, 80, 1))
            .await
            .unwrap();
        let mut dist = 0;
        for _ in 0..60 {
            if let Ok(pkt) = c.recv_notification().await {
                if let Some(st) = crate::protocol::decode_status(&pkt) {
                    dist = st.distance_m;
                }
            }
        }
        assert!(dist > 0, "sim never moved");
        c.write_packet(&crate::protocol::encode_command(Command::Stop, 1.0, 80, 1))
            .await
            .unwrap();
        let pkt = c.recv_notification().await.unwrap();
        let st = crate::protocol::decode_status(&pkt).unwrap();
        assert_eq!(st.distance_m, 0);
        assert_eq!(st.steps, Some(0));
        assert_eq!(st.kcal, 0);
    }

    /// §4.5a: remote_start makes the NEXT connection report a moving
    /// belt with growing counters (the crediting scenario). Counters
    /// must not burst either: per-notification distance stays under
    /// 1 m at low speed (the old std::time tick bug redeemed lost time
    /// in one packet).
    #[tokio::test(start_paused = true)]
    async fn remote_start_survives_reconnect() {
        let link = SimLink::new(80);
        link.remote_start(1.5);
        let mut c = link.connect().await.unwrap();
        let pkt = c.recv_notification().await.unwrap();
        let st = crate::protocol::decode_status(&pkt).unwrap();
        assert_eq!(st.belt_state(), BeltState::Running);
        assert!((st.speed_mps * 3.6 - 1.5).abs() < 1e-9);
        // ~0.2 m per 500 ms tick at 1.5 km/h: the u32 counters only move
        // after the fractional accumulator crosses whole metres
        let mut last = st.distance_m;
        for _ in 0..12 {
            let next = c.recv_notification().await.unwrap();
            let st2 = crate::protocol::decode_status(&next).unwrap();
            assert!(st2.distance_m >= last, "counters never go back");
            last = st2.distance_m;
        }
        assert!(last > 0, "counters advance over ~3 s of walking");
    }

    #[tokio::test(start_paused = true)]
    async fn distance_grows_linearly() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();
        c.write_packet(&crate::protocol::encode_command(Command::Start, 3.0, 80, 1))
            .await
            .unwrap();
        let mut prev = 0u32;
        for _ in 0..10 {
            let pkt = c.recv_notification().await.unwrap();
            let st = crate::protocol::decode_status(&pkt).unwrap();
            let d = st.distance_m.saturating_sub(prev);
            prev = st.distance_m;
            assert!(d <= 1, "distance burst: +{d} m in one packet");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drop_flag_turns_notifications() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();
        c.recv_notification().await.unwrap();
        c.write_packet(&crate::protocol::encode_command(Command::Start, 2.0, 80, 1))
            .await
            .unwrap();
        link.drop_flag().store(true, Ordering::Relaxed);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), c.recv_notification())
                .await
                .unwrap(),
            Err(LinkError::Dead(_))
        ));
    }

    /// §3.7: the SupeRun simulator only takes framed packets, answers
    /// with framed `68 2f` frames (speeds km/h × 1000), and a framed SetSpeed
    /// after the start ramp applies.
    #[tokio::test(start_paused = true)]
    async fn superun_framed_start_and_speed() {
        use crate::protocol::{HEARTBEAT, Variant::SupeRun};
        let link = SimLink::with_variant(80, SupeRun);
        let mut c = link.connect().await.unwrap();
        let first = c.recv_notification().await.unwrap();
        assert_eq!((first[0], first[4], first[5]), (0x4d, 0x68, 0x2f));
        let cmd = |seq, c, kmh| SupeRun.frame(seq, &SupeRun.encode_command(c, kmh, 80, 1));
        // an unframed start is ignored
        c.write_packet(&crate::protocol::encode_command(Command::Start, 3.0, 80, 1))
            .await
            .unwrap();
        c.write_packet(&SupeRun.frame(0, &HEARTBEAT)).await.unwrap();
        let st = crate::protocol::decode_status(&c.recv_notification().await.unwrap());
        assert_eq!(st.unwrap().belt_state(), BeltState::Stopped);
        c.write_packet(&cmd(1, Command::Start, 3.0)).await.unwrap();
        let mut running = false;
        for _ in 0..20 {
            let st = crate::protocol::decode_status(&c.recv_notification().await.unwrap());
            if st.unwrap().belt_state() == BeltState::Running {
                running = true;
                break;
            }
        }
        assert!(running, "sim never reached running");
        c.write_packet(&cmd(2, Command::SetSpeed, 3.0))
            .await
            .unwrap();
        for _ in 0..60 {
            let st = crate::protocol::decode_status(&c.recv_notification().await.unwrap()).unwrap();
            if (st.speed_mps * 3.6 - 3.0).abs() < 0.01 {
                assert!((st.target_speed_kmh - 3.0).abs() < 0.01);
                return;
            }
        }
        panic!("SetSpeed never applied");
    }

    /// §3.7: a dropped recv_notification keeps its deadline, so a
    /// 200 ms keep-alive tick can never starve the 500 ms notifications.
    #[tokio::test(start_paused = true)]
    async fn recv_notification_is_cancel_safe() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();
        let t0 = Instant::now();
        for _ in 0..5 {
            let r = tokio::time::timeout(Duration::from_millis(200), c.recv_notification()).await;
            if r.is_ok() {
                break;
            }
        }
        // 3 × 200 ms passes the 500 ms deadline: the packet arrived on time
        assert!(Instant::now().duration_since(t0) <= Duration::from_millis(600));
    }
}

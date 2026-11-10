// SPDX-License-Identifier: ISC

use crate::link::LinkError;
use crate::protocol::{BeltState, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Simulated treadmill (§4.7): emits a real 31-byte notification every
/// 500 ms (so the decoder path is exercised), reacts to commands like a
/// real treadmill (start → 3 s starting → running ramping at 0.5 km/h/s;
/// counters advance from the speed; stop resets counters), and can be
/// told to drop the link (daemon wires SIGUSR1 into `request_drop`) to
/// exercise the reconnect logic.
pub struct SimLink {
    weight_kg: u8,
    drop: Arc<AtomicBool>,
}

pub struct SimConn {
    belt: SimBelt,
    weight_kg: u8,
    drop: Arc<AtomicBool>,
    last_tick: Option<Instant>,
}

struct SimBelt {
    state: BeltState,
    target: f64,
    speed: f64,
    distance_m: u32,
    steps: u32,
    kcal: u16,
    duration_s: f64,
    starting_ends_at: Option<Instant>,
    pending: Option<Command>,
    commanded_speed: f64,
}

impl SimLink {
    pub fn new(weight_kg: u8) -> Self {
        Self {
            weight_kg,
            drop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn request_drop(&self) {
        self.drop.store(true, Ordering::Relaxed);
    }

    pub async fn connect(&self) -> Result<SimConn, LinkError> {
        Ok(SimConn {
            belt: SimBelt::new(),
            weight_kg: self.weight_kg,
            drop: self.drop.clone(),
            last_tick: None,
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
            steps: 0,
            kcal: 0,
            duration_s: 0.0,
            starting_ends_at: None,
            pending: None,
            commanded_speed: 1.0,
        }
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Start => {
                self.target = self.commanded_speed;
                if self.state != BeltState::Running && self.state != BeltState::Starting {
                    self.starting_ends_at = Some(Instant::now() + Duration::from_secs(3));
                    self.state = BeltState::Starting;
                    self.speed = 0.0;
                }
            }
            Command::SetSpeed => {
                self.target = self.commanded_speed;
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
                self.steps = 0;
                self.kcal = 0;
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
            BeltState::Starting if self.starting_ends_at.is_some_and(|t| Instant::now() >= t) => {
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
                let km = self.speed * dt_s / 3.6;
                self.distance_m += (km * 1000.0).round() as u32;
                self.steps += (km * 1000.0 / 0.7) as u32;
                self.kcal = ((self.kcal as f64) + 0.5 * weight_kg as f64 * km) as u16;
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

pub const NOTIFY_PERIOD: Duration = Duration::from_millis(500);

impl SimConn {
    pub async fn recv_notification(&mut self) -> Result<Vec<u8>, LinkError> {
        tokio::time::sleep(NOTIFY_PERIOD).await;
        if self.drop.load(Ordering::Relaxed) {
            return Err(LinkError::Dead("simulated link dropped (SIGUSR1)".into()));
        }
        let tick = Instant::now();
        // Counters advance from real elapsed time, even if the tick is late.
        let dt = self
            .last_tick
            .replace(tick)
            .map_or(NOTIFY_PERIOD, |t| tick.duration_since(t));
        let (state, dist, steps, kcal, dur) = self.belt.tick(dt, self.weight_kg);
        Ok(
            crate::protocol::encode_status(state, false, self.belt.speed, dist, steps, kcal, dur)
                .to_vec(),
        )
    }

    pub async fn write_packet(&mut self, pkt: &[u8]) -> Result<(), LinkError> {
        if pkt.len() == 5 {
            return Ok(()); // heartbeat: nothing to apply
        }
        if let Some((cmd, speed)) = parse_command(pkt) {
            self.belt.commanded_speed = speed;
            self.belt.pending = Some(cmd);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queue a start with 3.0 km/h on a raw connection and check that the
    /// belt reaches `running` and the ramp ends at the target speed.
    #[tokio::test(flavor = "current_thread")]
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

        // Belt keeps ramping to 3.0 (0.5 km/h per second), so after a few
        // more ticks the speed settles at the target.
        for _ in 0..40 {
            let pkt = c.recv_notification().await.unwrap();
            let st = crate::protocol::decode_status(&pkt).unwrap();
            if (st.speed_mps * 3.6 - 3.0).abs() < 1e-9 {
                return; // settled at the target speed
            }
        }
        panic!("speed never settled at target");
    }

    #[tokio::test(flavor = "current_thread")]
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
        assert_eq!(st.steps, 0);
        assert_eq!(st.kcal, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_flag_turns_notifications() {
        let link = SimLink::new(80);
        let mut c = link.connect().await.unwrap();
        c.recv_notification().await.unwrap();
        c.write_packet(&crate::protocol::encode_command(Command::Start, 2.0, 80, 1))
            .await
            .unwrap();
        link.request_drop();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), c.recv_notification())
                .await
                .unwrap(),
            Err(LinkError::Dead(_))
        ));
    }
}

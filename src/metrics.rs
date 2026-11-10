// SPDX-License-Identifier: ISC

use prometheus_client::encoding::text;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicU64;

/// Belt state label for `pitpat_state{state="…"}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct StateLabel {
    state: &'static str,
}

/// All 5 states, always present (§7).
const STATES: [&str; 5] = ["disconnected", "stopped", "starting", "running", "paused"];

#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct BuildInfo {
    version: String,
}

/// Float gauge: the default Gauge is an integer gauge, and several
/// quantities here are real-valued (m/s, seconds).
type FGauge = Gauge<f64, AtomicU64>;

/// All `pitpat_*` metrics (§7).
pub struct Metrics {
    pub connected: Gauge,
    pub speed_mps: FGauge,
    pub target_speed_mps: FGauge,
    pub session_distance_m: Gauge,
    pub session_steps: Gauge,
    pub session_kcal: Gauge,
    pub session_duration_s: FGauge,
    pub last_notification: FGauge,
    state: Family<StateLabel, Gauge>,
    // lifetime counters (persisted, monotonic)
    pub distance_m_total: Counter,
    pub steps_total: Counter,
    pub energy_kcal_total: Counter,
    // float "counter": prometheus-client's Counter is integer-only, and a
    // gauge whose value only ever grows moves like a counter
    moving_seconds_total: FGauge,
    pub sessions_total: Counter,
    pub reconnects_total: Counter,
    pub invalid_packets_total: Counter,
    registry: Registry,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let sub = registry.sub_registry_with_prefix("pitpat");

        macro_rules! reg_g {
            ($name:expr) => {{
                let m: Gauge = Gauge::default();
                sub.register($name, "".to_string(), m.clone());
                m
            }};
        }
        macro_rules! reg_f {
            ($name:expr) => {{
                let m: FGauge = Gauge::default();
                sub.register($name, "".to_string(), m.clone());
                m
            }};
        }
        macro_rules! reg_c {
            ($name:expr) => {{
                let m: Counter = Counter::default();
                sub.register($name, "".to_string(), m.clone());
                m
            }};
        }

        let connected = reg_g!("connected");
        let speed_mps = reg_f!("speed_meters_per_second");
        let target_speed_mps = reg_f!("target_speed_meters_per_second");
        let session_distance_m = reg_g!("session_distance_meters");
        let session_steps = reg_g!("session_steps");
        let session_kcal = reg_g!("session_energy_kilocalories");
        let session_duration_s = reg_f!("session_duration_seconds");
        let last_notification = reg_f!("last_notification_timestamp_seconds");
        let state: Family<StateLabel, Gauge> = Family::default();
        sub.register(
            "state",
            "1 for the current belt state, 0 for the others".to_string(),
            state.clone(),
        );
        let distance_m_total = reg_c!("distance_meters_total");
        let steps_total = reg_c!("steps_total");
        let energy_kcal_total = reg_c!("energy_kilocalories_total");
        let moving_seconds_total = reg_f!("moving_seconds_total");
        let sessions_total = reg_c!("sessions_total");
        let reconnects_total = reg_c!("reconnects_total");
        let invalid_packets_total = reg_c!("invalid_packets_total");

        let bu: Family<BuildInfo, Gauge> = Family::default();
        bu.get_or_create(&BuildInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .set(1);
        sub.register("build_info", "build information, always 1".to_string(), bu);

        Self {
            registry,
            connected,
            speed_mps,
            target_speed_mps,
            session_distance_m,
            session_steps,
            session_kcal,
            session_duration_s,
            last_notification,
            state,
            distance_m_total,
            steps_total,
            energy_kcal_total,
            moving_seconds_total,
            sessions_total,
            reconnects_total,
            invalid_packets_total,
        }
    }

    pub fn render(&self, out: &mut String) -> std::fmt::Result {
        text::encode(out, &self.registry)
    }

    /// Push the current belt state into `pitpat_state{state=…}`.
    pub fn set_state(&self, state: &str) {
        for s in STATES {
            self.state
                .get_or_create(&StateLabel { state: s })
                .set(i64::from(u8::from(s == state)));
        }
    }

    /// Re-emit lifetime totals at startup (counters must start at their
    /// pre-restart value so that `increase()` stays valid across restarts).
    pub fn replay_totals(&self, t: &crate::session::Totals) {
        // counters can only be incremented; at startup, replay the persisted
        // value so `increase()` spans restarts with only a plain reset
        self.distance_m_total.inc_by(t.distance_m);
        self.steps_total.inc_by(t.steps);
        self.energy_kcal_total.inc_by(t.kcal);
        self.moving_seconds_total.inc_by(t.moving_s);
        self.sessions_total.inc_by(t.sessions);
    }

    /// Apply a delta from a notification/status message.
    pub fn bump_totals(&self, dist: u32, steps: u32, kcal: u32, moving_s: f64) {
        self.distance_m_total.inc_by(u64::from(dist));
        self.steps_total.inc_by(u64::from(steps));
        self.energy_kcal_total.inc_by(u64::from(kcal));
        self.moving_seconds_total.inc_by(moving_s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_metrics() {
        let m = Metrics::new();
        m.set_state("running");
        m.replay_totals(&crate::session::Totals {
            distance_m: 1,
            steps: 2,
            kcal: 3,
            moving_s: 4.0,
            sessions: 5,
        });
        m.invalid_packets_total.inc_by(7);
        let mut buf = String::new();
        m.render(&mut buf).unwrap();
        assert!(!buf.contains("no help"));
        for name in [
            "pitpat_connected",
            "pitpat_state{state=\"paused\"}",
            "pitpat_speed_meters_per_second",
            "pitpat_distance_meters_total",
            "pitpat_reconnects_total",
            "pitpat_build_info{version=",
            "pitpat_last_notification_timestamp_seconds",
        ] {
            assert!(buf.contains(name), "missing {name} in:\n{buf}");
        }
        assert!(buf.contains("pitpat_state{state=\"running\"} 1"));
        assert!(buf.contains("pitpat_state{state=\"paused\"} 0"));
    }
}

// SPDX-License-Identifier: ISC

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One completed session record (§4.3), one JSON line of `sessions.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRec {
    pub start: Timestamp,
    pub end: Timestamp,
    /// Distance in metres.
    pub distance_m: u32,
    pub steps: u32,
    /// Energy in kcal.
    pub kcal: u16,
    /// Treadmill-reported moving time in seconds.
    pub duration_s: f64,
    /// distance / duration, km/h.
    pub avg_speed_kmh: f64,
    /// Highest belt speed seen during the session, km/h.
    pub max_speed_kmh: f64,
    pub end_reason: String,
}

/// Why a session ended.
#[derive(Debug, Clone, Copy)]
pub enum EndReason {
    Stopped,
    Disconnected,
    Shutdown,
    TargetReached,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Stopped => "stopped",
            EndReason::Disconnected => "disconnected",
            EndReason::Shutdown => "shutdown",
            EndReason::TargetReached => "target_reached",
        }
    }
}

/// A session shorter than 10 s **and** under 10 m is noise (§4.3).
pub fn is_noise(rec: &SessionRec) -> bool {
    rec.duration_s < 10.0 && rec.distance_m < 10
}

/// Append a record to `$STATE_DIR/sessions.jsonl`.
pub fn append_session(dir: &Path, rec: &SessionRec) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("sessions.jsonl");
    let mut line = serde_json::to_string(rec).expect("serialize session");
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

/// Read the recorded sessions, newest first, corrupted lines skipped (§12).
pub fn read_sessions(dir: &Path, limit: usize) -> Vec<SessionRec> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string(dir.join("sessions.jsonl")) else {
        return out;
    };
    for line in text.lines().rev() {
        match serde_json::from_str::<SessionRec>(line) {
            Ok(r) => out.push(r),
            Err(err) if !line.trim().is_empty() => {
                tracing::warn!(error = %err, "skipping corrupt session line");
            }
            Err(_) => {}
        }
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Aggregate counters for "today" (§6.2): sum of finished sessions started
/// today, in local time.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Summary {
    pub distance_m: u64,
    pub steps: u64,
    pub kcal: u64,
    pub duration_s: f64,
}

/// Timestamp rounded down to the start of the local calendar day.
pub fn today_start(now: Timestamp) -> Timestamp {
    let z = now.to_zoned(jiff::tz::TimeZone::system());
    z.date()
        .to_zoned(jiff::tz::TimeZone::system())
        .map_or(now, |zd| zd.timestamp())
}

/// Sum today's finished sessions (local date) plus, optionally, the open one.
pub fn today_summary(dir: &Path, open: Option<&SessionRec>, now: Timestamp) -> Summary {
    let midnight = today_start(now);
    let mut out = Summary::default();
    let Ok(text) = std::fs::read_to_string(dir.join("sessions.jsonl")) else {
        if let Some(open) = open {
            if open.start >= midnight {
                add(&mut out, open);
            }
        }
        return out;
    };
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<SessionRec>(line) else {
            continue;
        };
        if rec.start >= midnight {
            add(&mut out, &rec);
        }
    }
    if let Some(open) = open {
        if open.start >= midnight {
            add(&mut out, open);
        }
    }
    out
}

fn add(out: &mut Summary, rec: &SessionRec) {
    out.distance_m += rec.distance_m as u64;
    out.steps += rec.steps as u64;
    out.kcal += rec.kcal as u64;
    out.duration_s += rec.duration_s;
}

// ---------------------------------------------------------------- totals

/// Lifetime totals (§4.4), persisted as `totals.json`, updated with deltas
/// from the treadmill's session counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub distance_m: u64,
    pub steps: u64,
    pub kcal: u64,
    pub moving_s: f64,
    pub sessions: u64,
}

/// Treadmill counter snapshot used for the delta logic.
#[derive(Debug, Clone, Copy)]
pub struct Counters {
    pub distance_m: u32,
    pub steps: u32,
    pub kcal: u16,
    pub duration_s: f64,
}

/// Per-connection delta tracker (§4.4): the first packet after (re)connecting
/// is the baseline (a possible continuation already counted), normal deltas
/// go through as-is, resets count from zero, absurd deltas are rejected.
#[derive(Debug, Default)]
pub struct DeltaTracker {
    last: Option<Counters>,
}

const MAX_DISTANCE_DELTA_M: u32 = 50;
const MAX_STEPS_DELTA: u32 = 100;

impl DeltaTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the lifetime totals from a fresh notification.
    pub fn apply(&mut self, totals: &mut Totals, c: Counters) -> bool {
        let Some(last) = self.last else {
            // first packet after (re)connecting: baseline only
            self.last = Some(c);
            return false;
        };
        if c.distance_m.saturating_sub(last.distance_m) > MAX_DISTANCE_DELTA_M
            && c.distance_m > last.distance_m
        {
            tracing::warn!(
                distance_m = c.distance_m,
                last = last.distance_m,
                "absurd distance delta rejected"
            );
            return false;
        }
        if c.steps > last.steps && c.steps - last.steps > MAX_STEPS_DELTA {
            tracing::warn!(
                steps = c.steps,
                last = last.steps,
                "absurd steps delta rejected"
            );
            return false;
        }

        let dist_d = d(c.distance_m, last.distance_m);
        let steps_d = d(c.steps, last.steps);
        let kcal_d = d(u32::from(c.kcal), u32::from(last.kcal));
        let dur_d = f(c.duration_s, last.duration_s);

        totals.distance_m += dist_d as u64;
        totals.steps += steps_d as u64;
        totals.kcal += kcal_d as u64;
        totals.moving_s += dur_d;

        self.last = Some(c);
        true
    }

    /// Clear the baseline (e.g. the device disconnected).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

fn d(new: u32, last: u32) -> u32 {
    if new >= last {
        new - last
    } else {
        // counter reset: count from zero
        new
    }
}

fn f(new: f64, last: f64) -> f64 {
    (new - last).max(0.0)
}

/// Persist totals atomically (temp file + rename, §4.4).
pub fn save_totals(dir: &Path, totals: &Totals) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let dst = dir.join("totals.json");
    let tmp = dir.join(format!("totals.json.tmp.{}", std::process::id()));
    let data = serde_json::to_vec(totals).expect("serialize totals");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &dst)
}

/// Load totals from `$STATE_DIR/totals.json`; a corrupted file is renamed
/// aside and totals restart from zero (§12).
pub fn load_totals(dir: &Path) -> Totals {
    let path = dir.join("totals.json");
    match std::fs::read(&path) {
        Ok(data) => match serde_json::from_slice::<Totals>(&data) {
            Ok(t) => t,
            Err(err) => {
                let ts = jiff::Timestamp::now().as_second();
                let bad = dir.join(format!("totals.json.corrupt-{ts}"));
                if std::fs::rename(&path, &bad).is_ok() {
                    tracing::error!(file = %bad.display(), error = %err, "totals.json corrupt, renamed");
                } else {
                    tracing::error!(error = %err, "totals.json corrupt and unrenamable, starting from zero");
                }
                Totals::default()
            }
        },
        Err(_) => Totals::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(dist: u32, steps: u32, kcal: u16, dur: f64) -> Counters {
        Counters {
            distance_m: dist,
            steps,
            kcal,
            duration_s: dur,
        }
    }

    #[test]
    fn deltas_normal_and_reset() {
        let mut dt = DeltaTracker::new();
        let mut t = Totals::default();
        assert!(
            !dt.apply(&mut t, c(0, 0, 0, 0.0)),
            "baseline packet counts nothing"
        );
        assert!(dt.apply(&mut t, c(1, 2, 1, 0.5)));
        assert!(dt.apply(&mut t, c(3, 4, 2, 1.5)));
        assert_eq!(t.distance_m, 3);
        assert_eq!(t.steps, 4);
        assert_eq!(t.kcal, 2);
        assert_eq!(t.moving_s, 1.5);
        // reset: distance goes down, count from zero
        assert!(dt.apply(&mut t, c(1, 1, 0, 0.2)));
        assert_eq!(t.distance_m, 4);
        assert_eq!(t.steps, 5);
        assert_eq!(t.kcal, 2);
    }

    #[test]
    fn deltas_absurd_rejected() {
        let mut dt = DeltaTracker::new();
        let mut t = Totals::default();
        dt.apply(&mut t, c(0, 0, 0, 0.0));
        // 60 m in one packet: rejected(, the baseline stays)
        assert!(!dt.apply(&mut t, c(60, 0, 0, 5.0)));
        // still, next sane packet resumes counting
        assert!(dt.apply(&mut t, c(2, 1, 0, 1.0)));
        assert_eq!(t.distance_m, 2);
        // 200 steps in one packet: rejected
        assert!(!dt.apply(&mut t, c(3, 200, 0, 2.0)));
        assert_eq!(t.steps, 1);
    }

    #[test]
    fn deltas_reconnect_ignores_first() {
        // A treadmill whose counters kept counting across a reconnect must
        // not double-count: the first packet after reconnecting is a baseline.
        let mut dt = DeltaTracker::new();
        let mut t = Totals {
            distance_m: 100,
            steps: 140,
            kcal: 10,
            moving_s: 120.0,
            sessions: 0,
        };
        // counters kept going while the link was down: first packet after
        // reconnecting must be a baseline, not a delta
        dt.apply(&mut t, c(104, 142, 11, 124.0));
        assert_eq!(t.distance_m, 100, "continuation must not double-count");
        assert_eq!(t.steps, 140);
        dt.apply(&mut t, c(107, 145, 12, 127.0));
        assert_eq!(t.distance_m, 103);
        assert_eq!(t.moving_s, 123.0);
    }

    #[test]
    fn noise_rule() {
        let ts = jiff::Timestamp::now();
        let rec = |d: u32, dur: f64| SessionRec {
            start: ts,
            end: ts,
            distance_m: d,
            steps: 0,
            kcal: 0,
            duration_s: dur,
            avg_speed_kmh: 0.0,
            max_speed_kmh: 0.0,
            end_reason: "stopped".into(),
        };
        assert!(is_noise(&rec(5, 5.0)), "short and short distance = noise");
        assert!(!is_noise(&rec(20, 5.0)), "20 m: not noise");
        assert!(!is_noise(&rec(5, 30.0)), "30 s: not noise");
    }

    #[test]
    fn totals_roundtrip_and_corrupt() {
        let dir = std::env::temp_dir().join(format!("pitpat-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = Totals {
            distance_m: 812345,
            steps: 1100000,
            kcal: 35000,
            moving_s: 900000.0,
            sessions: 312,
        };
        save_totals(&dir, &t).unwrap();
        assert_eq!(load_totals(&dir), t);

        std::fs::write(dir.join("totals.json"), b"{bogus").unwrap();
        let t = load_totals(&dir);
        assert_eq!(t, Totals::default());
        let mut renamed: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        renamed.sort();
        assert!(
            renamed
                .iter()
                .any(|n| n.to_string_lossy().starts_with("totals.json.corrupt-"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_jsonl_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pitpat-sess-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ts = jiff::Timestamp::now();
        let rec = |i: u32| SessionRec {
            start: ts,
            end: ts,
            distance_m: i,
            steps: 0,
            kcal: 0,
            duration_s: 1.0,
            avg_speed_kmh: 0.0,
            max_speed_kmh: 0.0,
            end_reason: "stopped".into(),
        };
        for i in 0..5 {
            append_session(&dir, &rec(i)).unwrap();
        }
        // inject a corrupt line
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.join("sessions.jsonl"))
                .unwrap();
            f.write_all(b"garbage\n").unwrap();
        }
        let sessions = read_sessions(&dir, 10);
        assert_eq!(sessions.len(), 5, "corrupt line skipped");
        assert_eq!(sessions[0].distance_m, 4, "newest first");
        let sessions = read_sessions(&dir, 2);
        assert_eq!(sessions.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

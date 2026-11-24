// SPDX-License-Identifier: ISC

//! Replay tests over the committed hardware captures (`tests/captures/`,
//! see its README): the frames logged by the old daemon are fed to the
//! core exactly in order, so the decoder and the session rules are
//! checked against real hardware without the treadmill (REVIEW round-17
//! N3). `device-a/` is the fba0 pad (`68 32`/`68 34` frames); `device-b/`
//! is the SupeRun-built PitPat-T01 (framed `4d 04 … 68 2f`, §3.7), read by
//! its own decoder (§3.2a) and with its own replay test here.

use jiff::civil::DateTime;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::DaemonCfg;
use crate::core::Core;
use crate::metrics::Metrics;
use crate::protocol as p;
use crate::session::{SessionRec, Totals};

/// One parsed `bytes=…` line of a capture file.
struct Frame {
    /// Wall-clock time of the log line, second resolution.
    ts: Option<DateTime>,
    /// The hex bytes after `bytes=`.
    bytes: Vec<u8>,
    /// The `state=` text the old daemon logged next to the frame.
    state: Option<String>,
    /// `true` for `write bytes=` lines, `false` for notifications.
    write: bool,
}

/// Parse a capture file into frames, in file order. Lines without
/// `bytes=` (header notes, unrelated log lines) are skipped; the hex run
/// ends at the first token that is not a byte (`state=…`, `speed_kmh=…`).
fn parse_capture(year: i16, text: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for line in text.lines() {
        let Some(pos) = line.find("bytes=") else {
            continue;
        };
        let head = line[..pos].split_whitespace().collect::<Vec<_>>();
        // journalctl "short" prefix: "Nov 17 12:05:03 host unit[pid]: …"
        let ts = head
            .first()
            .zip(head.get(1))
            .zip(head.get(2))
            .and_then(|((m, d), hms)| parse_ts(year, m, d, hms));
        let write = line[..pos].ends_with("write ");
        let bytes: Vec<u8> = line[pos + 6..]
            .split_whitespace()
            .map_while(parse_hex_byte)
            .collect();
        let state = line[pos..]
            .split_whitespace()
            .find_map(|w| w.strip_prefix("state="))
            .map(str::to_string);
        if !bytes.is_empty() {
            frames.push(Frame {
                ts,
                bytes,
                state,
                write,
            });
        }
    }
    frames
}

fn parse_hex_byte(word: &str) -> Option<u8> {
    if word.len() <= 2 && word.chars().all(|c| c.is_ascii_hexdigit()) {
        u8::from_str_radix(word, 16).ok()
    } else {
        None
    }
}

fn parse_ts(year: i16, mon: &str, day: &str, hms: &str) -> Option<DateTime> {
    let mon = match mon {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: i8 = day.parse().ok()?;
    let mut it = hms.split(':');
    let h: i8 = it.next()?.parse().ok()?;
    let m: i8 = it.next()?.parse().ok()?;
    let s: i8 = it.next()?.parse().ok()?;
    // The nix test sandbox has no system time-zone database, so
    // `in_tz("Europe/Rome")` panics there (the daemon build found it out).
    // Capture lines are wall-clock times in the walker's zone (the
    // captures README says which); the tests only compare them for order
    // and print them in failures, so the civil (zone-less) value is
    // enough and needs no tzdb at all.
    Some(jiff::civil::date(year, mon, day).at(h, m, s, 0))
}

/// The committed captures, name plus contents (embedded, so
/// `cargo test` and the nix build both see them, wherever they run from).
const CAPTURES: &[(&str, i16, &str)] = &[
    (
        "2026-11-13-first-connect-68-34.log",
        2026,
        include_str!("../tests/captures/device-a/2026-11-13-first-connect-68-34.log"),
    ),
    (
        "2026-11-16-paused-68-32.log",
        2026,
        include_str!("../tests/captures/device-a/2026-11-16-paused-68-32.log"),
    ),
    (
        "2026-11-17-step-styles.log",
        2026,
        include_str!("../tests/captures/device-a/2026-11-17-step-styles.log"),
    ),
    (
        "2026-11-17-walk-60-steps.log",
        2026,
        include_str!("../tests/captures/device-a/2026-11-17-walk-60-steps.log"),
    ),
    (
        "2026-11-18-walk-20-steps.log",
        2026,
        include_str!("../tests/captures/device-b/2026-11-18-walk-20-steps.log"),
    ),
    (
        "2026-11-24-walk-2-9.log",
        2026,
        include_str!("../tests/captures/device-b/2026-11-24-walk-2-9.log"),
    ),
];

fn all_captures() -> Vec<(&'static str, Vec<Frame>)> {
    CAPTURES
        .iter()
        .map(|(name, year, text)| (*name, parse_capture(*year, text)))
        .collect()
}

#[test]
fn captures_are_parsed_in_order() {
    for (name, frames) in all_captures() {
        assert!(!frames.is_empty(), "{name}: nothing parsed");
        let mut last = None;
        for f in &frames {
            if let (Some(prev), Some(now)) = (last, f.ts) {
                assert!(now >= prev, "{name}: timestamps out of order at {now}");
            }
            last = f.ts.or(last);
        }
    }
}

/// Decoder tests on every file (REVIEW round-17 N3): every status frame
/// decodes, the decoded state matches the `state=` text the old daemon
/// logged next to the frame, and `68 32` and T01 frames carry `steps`
/// while `68 34` frames do not (K7).
#[test]
fn every_capture_frame_decodes() {
    for (name, frames) in all_captures() {
        for f in &frames {
            if f.write {
                continue;
            }
            let st = p::decode_status(&f.bytes)
                .unwrap_or_else(|| panic!("{name}: frame at {:?} does not decode", f.ts));
            if let Some(state) = &f.state {
                assert_eq!(
                    format!("{:?}", st.belt_state()),
                    state.as_str(),
                    "{name}: state text mismatch at {:?}",
                    f.ts
                );
            }
            let with_steps = match p::decoder_for(&f.bytes).name() {
                "device-a" => f.bytes[1] == 0x32,
                name => name == "superun-t01",
            };
            assert_eq!(
                st.steps.is_some(),
                with_steps,
                "{name}: steps presence at {:?}",
                f.ts
            );
        }
    }
}

/// Session tests on the two 2026-11-17 walks (REVIEW round-17 N3): feed
/// the raw frames to the core and check the recorded session — one per
/// walk (pause/resume and the 15 s step-off do not split or pause it),
/// and the final step count is the last value before stop (268 / 446),
/// not 0: the old daemon recorded `steps=0` for the first walk.
#[test]
fn replay_walk_60_steps() {
    let recs = replay("2026-11-17-walk-60-steps.log");
    assert_eq!(recs.len(), 1, "one session for the walk");
    assert_eq!(recs[0].steps, 268, "final step count, not 0");
    assert_eq!(recs[0].distance_m, 119);
    assert_eq!(recs[0].end_reason, "stopped");
}

#[test]
fn replay_step_styles() {
    let recs = replay("2026-11-17-step-styles.log");
    assert_eq!(recs.len(), 1, "one session for the walk");
    assert_eq!(recs[0].steps, 446, "final step count, not 0");
    assert_eq!(recs[0].end_reason, "stopped");
}

/// device-b (PitPat-T01), 5.5 min at a steady 2.9 km/h: the status speed
/// is km/h × 1000. The belt's own distance (@7) and session time (@20)
/// counters, from the first to the last metre tick, give its true speed;
/// the decoded speed must agree to 0.01 km/h (as mph × 1600 it would read
/// 2.917, 0.018 off).
#[test]
fn replay_t01_speed_scale_2_9() {
    let mut ticks: Vec<(u32, f64)> = Vec::new(); // (distance m, session time s) at each new metre
    let (core, metrics, _) = feed("2026-11-24-walk-2-9.log", |c, _| {
        if let Some(s) = c.snapshot().session {
            if ticks.last().map(|t| t.0) != Some(s.distance_m) {
                ticks.push((s.distance_m, s.duration_s));
            }
        }
    });
    assert_eq!(
        metrics.invalid_packets_total.get(),
        0,
        "every frame decodes"
    );
    let snap = core.snapshot();
    assert!((snap.speed_kmh - 2.9).abs() < 1e-9, "{}", snap.speed_kmh);
    let (a, b) = (ticks[1], ticks[ticks.len() - 1]); // ticks[0] is mid-metre
    let belt_kmh = f64::from(b.0 - a.0) / (b.1 - a.1) * 3.6;
    assert!(
        (belt_kmh - snap.speed_kmh).abs() < 0.01,
        "odometer {belt_kmh}"
    );
    let ses = snap.session.expect("one open session");
    assert_eq!((ses.steps, ses.distance_m), (1740, 902));
}

/// device-b (PitPat-T01): the frames decode from their headers alone, as
/// no SupeRun link is set up here. The belt runs for the whole capture,
/// so the walk is one open session whose steps and distance follow the
/// belt: 213 -> 232 over the 20 steps counted in the third recording,
/// 100 -> 110 m.
#[test]
fn replay_t01_walk_20_steps() {
    // the counted recording starts at 09:51:01 (see the capture's header)
    let counted = parse_ts(2026, "Nov", "18", "09:51:01");
    let mut at_counted = None;
    let (core, metrics, frames) = feed("2026-11-18-walk-20-steps.log", |c, ts| {
        if at_counted.is_none() && ts == counted {
            let s = c.snapshot().session.expect("session open");
            at_counted = Some((s.start, s.steps, s.distance_m));
        }
    });
    assert_eq!(
        metrics.invalid_packets_total.get(),
        0,
        "every frame decodes"
    );
    let (start, steps, dist) = at_counted.expect("a frame at 09:51:01");
    assert_eq!((steps, dist), (213, 100));
    let ses = core.snapshot().session.expect("one open session");
    assert_eq!(ses.start, start, "not split");
    assert_eq!((ses.steps, ses.distance_m), (232, 110));
    assert!((core.snapshot().speed_kmh - 2.3).abs() < 1e-9);
    assert!(
        frames.iter().any(|f| f.write),
        "keep-alive writes are parsed too"
    );
}

/// Feed every notification frame of one capture to a fresh core, calling
/// `after` with the core and the frame time after each one.
fn feed(
    name: &str,
    mut after: impl FnMut(&Core, Option<DateTime>),
) -> (Core, Arc<Metrics>, Vec<Frame>) {
    let (_, frames) = all_captures()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("{name}: capture not committed"));
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pitpat-replay-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (events, _ev_rx) = tokio::sync::broadcast::channel(8);
    let (_, api_rx) = tokio::sync::mpsc::channel(32);
    let metrics = Arc::new(Metrics::new());
    let mut core = Core::new(
        DaemonCfg::from(&clap::Parser::parse_from(["pitpat"])),
        Totals::default(),
        metrics.clone(),
        events,
        api_rx,
        dir,
    );
    core.begin_connection(None);
    for f in &frames {
        if !f.write {
            core.on_packet(&f.bytes);
            after(&core, f.ts);
        }
    }
    (core, metrics, frames)
}

/// Feed every notification frame of one capture to a fresh core and
/// return the session records it wrote to `sessions.jsonl`.
fn replay(name: &str) -> Vec<SessionRec> {
    let (_, frames) = all_captures()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("{name}: capture not committed"));
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pitpat-replay-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (events, _ev_rx) = tokio::sync::broadcast::channel(8);
    let (_, api_rx) = tokio::sync::mpsc::channel(32);
    let mut core = Core::new(
        DaemonCfg::from(&clap::Parser::parse_from(["pitpat"])),
        Totals::default(),
        Arc::new(Metrics::new()),
        events,
        api_rx,
        dir.clone(),
    );
    core.begin_connection(None);
    for f in &frames {
        if !f.write {
            core.on_packet(&f.bytes);
        }
    }
    let path = dir.join("sessions.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{name}: reading {path:?}: {err}"));
    text.lines()
        .map(|l| serde_json::from_str(l).expect("valid session record"))
        .collect()
}

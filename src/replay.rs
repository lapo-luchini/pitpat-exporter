// SPDX-License-Identifier: ISC

//! Replay tests over the committed hardware captures (`tests/captures/`,
//! see its README): the frames logged by the old daemon are fed to the
//! core exactly in order, so the decoder and the session rules are
//! checked against real hardware without the treadmill (REVIEW round-17
//! N3). All files are device-a; a `device-b/` folder gets its own decoder
//! (§3.2) and its own replay tests here.

use jiff::Timestamp;
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
    ts: Option<Timestamp>,
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

fn parse_ts(year: i16, mon: &str, day: &str, hms: &str) -> Option<Timestamp> {
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
    Some(
        jiff::civil::date(year, mon, day)
            .at(h, m, s, 0)
            .in_tz("Europe/Rome")
            .expect("Europe/Rome is a valid time zone")
            .timestamp(),
    )
}

/// The committed device-a captures, name plus contents (embedded, so
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

/// Decoder tests on every file (REVIEW round-17 N3): every `68 32`/`68 34`
/// frame decodes, the decoded state matches the `state=` text the old
/// daemon logged next to the frame, and `68 32` frames carry `steps`
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
            assert_eq!(
                st.steps.is_some(),
                f.bytes[1] == 0x32,
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

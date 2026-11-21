// SPDX-License-Identifier: ISC

/// Fixed 5-byte heartbeat packet sent when no command is queued.
pub const HEARTBEAT: [u8; 5] = [0x6a, 0x05, 0xfd, 0xf8, 0x43];

/// Treadmill GATT service UUID (PitPat).
pub const SERVICE_UUID: bluer::Uuid =
    bluer::Uuid::from_u128(0x0000_fba0_0000_1000_8000_0080_5f9b_34fb);
/// Command write characteristic.
pub const WRITE_UUID: bluer::Uuid =
    bluer::Uuid::from_u128(0x0000_fba1_0000_1000_8000_0080_5f9b_34fb);
/// Status notify characteristic.
pub const NOTIFY_UUID: bluer::Uuid =
    bluer::Uuid::from_u128(0x0000_fba2_0000_1000_8000_0080_5f9b_34fb);

/// SupeRun-built models (e.g. "PitPat-T01"): service ffff (§3.7).
pub const SUPERUN_SERVICE_UUID: bluer::Uuid =
    bluer::Uuid::from_u128(0x0000_ffff_0000_1000_8000_0080_5f9b_34fb);
/// SupeRun command write characteristic.
pub const SUPERUN_WRITE_UUID: bluer::Uuid =
    bluer::Uuid::from_u128(0x0000_ff01_0000_1000_8000_0080_5f9b_34fb);
/// SupeRun status notify characteristic.
pub const SUPERUN_NOTIFY_UUID: bluer::Uuid =
    bluer::Uuid::from_u128(0x0000_ff02_0000_1000_8000_0080_5f9b_34fb);

/// SupeRun keep-alive period: these models are not replied to per
/// notification, the host writes on a fixed timer instead (§3.7).
pub const SUPERUN_KEEPALIVE: std::time::Duration = std::time::Duration::from_millis(200);

const MPH_TO_MPS: f64 = 0.44704;
const MI_TO_KM: f64 = 1.609344;

/// Which protocol family the connected treadmill speaks (§3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Variant {
    /// The original PitPat: service fba0, bare packets, one reply per
    /// notification (§3.1).
    #[default]
    PitPat,
    /// SupeRun-built PitPat models (e.g. "PitPat-T01"): service ffff,
    /// every packet framed as `4d 00 <seq> <len>`, a 200 ms timer
    /// keep-alive, speed in mph × 1600 (§3.7).
    SupeRun,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::PitPat => "pitpat",
            Variant::SupeRun => "superun",
        }
    }

    /// (service, write, notify) characteristic UUIDs.
    pub fn uuids(self) -> (bluer::Uuid, bluer::Uuid, bluer::Uuid) {
        match self {
            Variant::PitPat => (SERVICE_UUID, WRITE_UUID, NOTIFY_UUID),
            Variant::SupeRun => (
                SUPERUN_SERVICE_UUID,
                SUPERUN_WRITE_UUID,
                SUPERUN_NOTIFY_UUID,
            ),
        }
    }

    /// `None`: reply once per notification (§3.1); `Some(p)`: write on a
    /// timer every `p`, never in reply to a notification (§3.7).
    pub fn keepalive(self) -> Option<std::time::Duration> {
        match self {
            Variant::PitPat => None,
            Variant::SupeRun => Some(SUPERUN_KEEPALIVE),
        }
    }

    /// The (unframed) command packet for this variant.
    pub fn encode_command(
        self,
        command: Command,
        speed_kmh: f64,
        weight_kg: u8,
        user_id: u64,
    ) -> [u8; 23] {
        match self {
            Variant::PitPat => encode_command(command, speed_kmh, weight_kg, user_id),
            Variant::SupeRun => encode_superun_command(command, speed_kmh),
        }
    }

    /// Wrap an outgoing packet (command or heartbeat) for the wire.
    pub fn frame(self, seq: u8, pkt: &[u8]) -> Vec<u8> {
        match self {
            Variant::PitPat => pkt.to_vec(),
            Variant::SupeRun => superun_frame(seq, pkt),
        }
    }

    /// Decode a notification exactly as received from the treadmill.
    pub fn decode_status(self, raw: &[u8]) -> Option<Status> {
        match self {
            Variant::PitPat => decode_status(raw),
            Variant::SupeRun => decode_superun_status(raw),
        }
    }
}

/// Command packet (host → treadmill), 23 bytes (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    SetSpeed,
    Pause,
    Stop,
}

/// Encode a 23-byte command packet.
///
/// `speed_kmh` is clamped to the valid u16 range; for start/set-speed it carries the
/// requested speed, for pause/stop the current target speed.
pub fn encode_command(command: Command, speed_kmh: f64, weight_kg: u8, user_id: u64) -> [u8; 23] {
    let speed = (speed_kmh * 1000.0).round().clamp(0.0, 65535.0) as u16;
    let kind = match command {
        Command::SetSpeed => 0x05,
        Command::Start | Command::Pause | Command::Stop => 0x01,
    };
    let cmd = match command {
        Command::Start | Command::SetSpeed => 0x04,
        Command::Pause => 0x02,
        Command::Stop => 0x00,
    };
    let mut p = [0x00; 23];
    p[0] = 0x6a;
    p[1] = 0x17;
    p[6] = (speed >> 8) as u8;
    p[7] = speed as u8;
    p[8] = kind;
    p[9] = 0x00; // incline: walking pads don't have one
    p[10] = weight_kg;
    p[11] = 0x00;
    p[12] = cmd; // bit 3 cleared = km/h mode
    p[13..21].copy_from_slice(&user_id.to_be_bytes());
    p[21] = p[1..21].iter().fold(0u8, |acc, &b| acc ^ b);
    p[22] = 0x43;
    p
}

/// Decoded status notification, normalised to SI units (§3.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    /// Belt speed in m/s.
    pub speed_mps: f64,
    /// The treadmill's own target speed, km/h (u16 @5; REVIEW round-8 F1:
    /// this model ignores the speed field of the start command, so this
    /// is what it actually targets).
    pub target_speed_kmh: f64,
    /// Session distance in metres.
    pub distance_m: u32,
    /// Session step count; `None` when the frame carries no steps value
    /// (K7: u16 @43 in 50-byte `68 32` frames only).
    pub steps: Option<u32>,
    /// Session energy in kcal.
    pub kcal: u16,
    /// Session (moving) duration in seconds.
    pub duration_s: f64,
    /// Treadmill flags byte.
    pub flags: u8,
}

/// Belt state deduced from the flags byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeltState {
    Stopped,
    Starting,
    Running,
    Paused,
}

impl Status {
    pub fn imperial(&self) -> bool {
        self.flags & 0x80 != 0
    }

    pub fn belt_state(&self) -> BeltState {
        match self.flags & 0x18 {
            0x18 => BeltState::Starting,
            0x08 => BeltState::Running,
            0x10 => BeltState::Paused,
            _ => BeltState::Stopped,
        }
    }
}

/// Per-model decoder boundary (§3.2): one implementation per frame
/// layout, picked from the frame header. A second treadmill model (a
/// `device-b/` capture in `tests/captures/`) gets its own implementation
/// and its own replay tests, without touching device-a's.
pub trait FrameDecoder {
    /// Model name, for logs and tests.
    fn name(&self) -> &'static str;
    /// Decode a status notification; `None` when the packet is not valid.
    fn decode_status(&self, pkt: &[u8]) -> Option<Status>;
}

/// device-a (the model seen on hardware so far): 50-byte `68 32` walk
/// frames and 52-byte `68 34` connect frames (§3.2).
pub struct DeviceA;

impl FrameDecoder for DeviceA {
    fn name(&self) -> &'static str {
        "device-a"
    }

    fn decode_status(&self, pkt: &[u8]) -> Option<Status> {
        if pkt.len() < 31 {
            return None;
        }
        let be16 = |i: usize| u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        // K7: steps live at u16 @43, but only in the 50-byte `68 32` frames
        // the treadmill sends while the belt runs. The 52-byte `68 34` frame
        // right after connect has an ASCII serial in its tail: no steps value.
        let steps = if pkt.len() >= 45 && pkt[1] == 0x32 {
            Some(u32::from(be16(43)))
        } else {
            None
        };
        let be32 = |i: usize| u32::from_be_bytes([pkt[i], pkt[i + 1], pkt[i + 2], pkt[i + 3]]);

        let flags = pkt[26];
        let imperial = flags & 0x80 != 0;
        const MPH_TO_MPS: f64 = 0.44704;
        const MI_TO_M: f64 = 1609.344;

        let speed_mps = if imperial {
            be16(3) as f64 / 1000.0 * MPH_TO_MPS
        } else {
            be16(3) as f64 / 1000.0 / 3.6
        };
        // the treadmill's own target speed, same unit and scaling
        let target_speed_kmh = if imperial {
            be16(5) as f64 / 1000.0 * 1.609344
        } else {
            be16(5) as f64 / 1000.0
        };
        let distance_m = if imperial {
            (be32(7) as f64 / 1000.0 * MI_TO_M).round() as u32
        } else {
            be32(7) // distance × 1000 per km == metres in metric mode
        };

        Some(Status {
            speed_mps,
            target_speed_kmh,
            distance_m,
            steps,
            kcal: be16(18),
            duration_s: (be32(20) as f64 / 1000.0),
            flags,
        })
    }
}

/// No capture of this model yet: every packet is rejected (and logged as
/// invalid), so the daemon never invents a frame layout it has not seen.
/// Its decoder arrives together with the first `device-b/` capture.
struct UnknownModel;

impl FrameDecoder for UnknownModel {
    fn name(&self) -> &'static str {
        "unknown"
    }

    fn decode_status(&self, _pkt: &[u8]) -> Option<Status> {
        None
    }
}

/// Pick the decoder from the frame header (`68 32`/`68 34` → device-a).
/// A new header from another model gets its own arm here, together with
/// its implementation and capture.
pub fn decoder_for(pkt: &[u8]) -> &'static dyn FrameDecoder {
    match (pkt.first(), pkt.get(1)) {
        (Some(&0x68), Some(&0x32) | Some(&0x34)) => &DeviceA,
        _ => &UnknownModel,
    }
}

/// Decode a status notification with the decoder of the packet's model.
///
/// Returns `None` for any packet that is not a valid status notification
/// (the caller must log it and count it as invalid).
pub fn decode_status(pkt: &[u8]) -> Option<Status> {
    decoder_for(pkt).decode_status(pkt)
}

// ------------------------------------------------------------ SupeRun

/// Frame an outgoing SupeRun packet: `4d 00 <seq> <len>` + packet (§3.7).
pub fn superun_frame(seq: u8, pkt: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pkt.len() + 4);
    out.extend_from_slice(&[0x4d, 0x00, seq, pkt.len() as u8]);
    out.extend_from_slice(pkt);
    out
}

/// The packet inside a SupeRun frame; `None` if `raw` is not framed.
/// The treadmill's own frames start `4d 04 <seq> <len>`.
pub fn superun_unframe(raw: &[u8]) -> Option<&[u8]> {
    (raw.len() >= 4 && raw[0] == 0x4d).then(|| &raw[4..])
}

/// Encode a SupeRun 23-byte command packet (unframed, §3.7): same 6a…43
/// shape as §3.3, but speed is mph × 1600, byte 8 is `0x01` for
/// set-speed and `0x05` otherwise, byte 12 is `0x0c` start/set-speed,
/// `0x0a` pause, `0x08` stop, and the user ID slot carries a constant.
/// The weight byte is fixed at `0x58` like the reference controller.
pub fn encode_superun_command(command: Command, speed_kmh: f64) -> [u8; 23] {
    let speed = if command == Command::Stop {
        0
    } else {
        (speed_kmh / MI_TO_KM * 1600.0).round().clamp(0.0, 65535.0) as u16
    };
    let mut p = [0x00; 23];
    p[0] = 0x6a;
    p[1] = 0x17;
    p[6] = (speed >> 8) as u8;
    p[7] = speed as u8;
    p[8] = if command == Command::SetSpeed {
        0x01
    } else {
        0x05
    };
    p[10] = 0x58;
    p[12] = match command {
        Command::Start | Command::SetSpeed => 0x0c,
        Command::Pause => 0x0a,
        Command::Stop => 0x08,
    };
    if command != Command::Stop {
        p[17..21].copy_from_slice(&[0x00, 0x0c, 0x55, 0x70]);
    }
    p[21] = p[1..21].iter().fold(0u8, |acc, &b| acc ^ b);
    p[22] = 0x43;
    p
}

/// Decode a framed SupeRun status notification (§3.7). The inner packet
/// has the §3.2 layout, but speeds are mph × 1600 and distance is metres
/// even though the T01 sets the imperial flag: values come out in SI and
/// the flag is cleared, so nothing downstream converts them again.
/// Speeds are rounded to 0.01 km/h: a commanded 3.0 km/h comes back as
/// 3.0005 from mph × 1600, and the §3.6 reassertion compares exactly.
pub fn decode_superun_status(raw: &[u8]) -> Option<Status> {
    let pkt = superun_unframe(raw)?;
    let mut st = decode_status(pkt)?;
    let kmh = |i: usize| {
        let mph1600 = u16::from_be_bytes([pkt[i], pkt[i + 1]]) as f64;
        (mph1600 / 1600.0 * MI_TO_KM * 100.0).round() / 100.0
    };
    st.speed_mps = kmh(3) / 3.6;
    st.target_speed_kmh = kmh(5);
    st.distance_m = u32::from_be_bytes([pkt[7], pkt[8], pkt[9], pkt[10]]);
    st.flags &= !0x80;
    Some(st)
}

/// Build a 47-byte SupeRun `68 2f` status frame, framed as the treadmill
/// sends it (simulator and tests).
#[allow(clippy::too_many_arguments)]
pub fn encode_superun_status(
    seq: u8,
    belt: BeltState,
    speed_kmh: f64,
    target_speed_kmh: f64,
    distance_m: u32,
    steps: u32,
    kcal: u16,
    duration_s: f64,
) -> Vec<u8> {
    let full = encode_status(belt, false, 0.0, 0.0, distance_m, steps, kcal, duration_s);
    let mut p = full[..47].to_vec();
    p[1] = 0x2f;
    let mph1600 = |v: f64| (v / MI_TO_KM * 1600.0).round().clamp(0.0, 65535.0) as u16;
    p[3..5].copy_from_slice(&mph1600(speed_kmh).to_be_bytes());
    p[5..7].copy_from_slice(&mph1600(target_speed_kmh).to_be_bytes());
    p[26] |= 0x80; // the T01 reports imperial, its units are fixed anyway
    p[46] = 0x43;
    let mut raw = superun_frame(seq, &p);
    raw[1] = 0x04;
    raw
}

/// Build a 31-byte status notification (used by the simulator and tests).
#[allow(clippy::too_many_arguments)]
pub fn encode_status(
    belt: BeltState,
    imperial: bool,
    speed_kmh: f64,
    target_speed_kmh: f64,
    distance_m: u32,
    steps: u32,
    kcal: u16,
    duration_s: f64,
) -> [u8; 50] {
    // K7: a 50-byte `68 32` frame, the kind that carries the step counter
    let mut p = [0x00; 50];
    p[0] = 0x68;
    p[1] = 0x32;
    let speed = (if imperial {
        speed_kmh / 1.609344 * 1000.0
    } else {
        speed_kmh * 1000.0
    })
    .round()
    .clamp(0.0, 65535.0) as u16;
    let dist = if imperial {
        (distance_m as f64 / 1609.344 * 1000.0).round() as u32
    } else {
        distance_m
    };
    p[3] = (speed >> 8) as u8;
    p[4] = speed as u8;
    // the treadmill's own target speed, same unit and scaling (§3.2 @5)
    let target = (if imperial {
        target_speed_kmh / 1.609344 * 1000.0
    } else {
        target_speed_kmh * 1000.0
    })
    .round()
    .clamp(0.0, 65535.0) as u16;
    p[5] = (target >> 8) as u8;
    p[6] = target as u8;
    p[7..11].copy_from_slice(&dist.to_be_bytes());
    // K7: steps live at u16 @43 in these frames (bytes 43..45, BE)
    p[43] = (steps >> 8) as u8;
    p[44] = steps as u8;
    p[18] = (kcal >> 8) as u8;
    p[19] = kcal as u8;
    p[20..24].copy_from_slice(&((duration_s * 1000.0) as u32).to_be_bytes());
    p[26] = if imperial { 0x80 } else { 0x00 }
        | match belt {
            BeltState::Starting => 0x18,
            BeltState::Running => 0x08,
            BeltState::Paused => 0x10,
            BeltState::Stopped => 0x00,
        };
    p[49] = 0x43; // end byte
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split(' ')
            .map(|x| u8::from_str_radix(x, 16).unwrap())
            .collect()
    }

    const USER_ID: u64 = 58965456623;
    const WEIGHT: u8 = 80;

    #[test]
    fn golden_vectors() {
        assert_eq!(HEARTBEAT, hex("6a 05 fd f8 43")[..]);
        assert_eq!(
            encode_command(Command::Start, 2.5, WEIGHT, USER_ID),
            hex("6a 17 00 00 00 00 09 c4 01 00 50 00 04 00 00 00 0d ba 9d 76 ef 3c 43")[..]
        );
        assert_eq!(
            encode_command(Command::SetSpeed, 3.2, WEIGHT, USER_ID),
            hex("6a 17 00 00 00 00 0c 80 05 00 50 00 04 00 00 00 0d ba 9d 76 ef 79 43")[..]
        );
        assert_eq!(
            encode_command(Command::Pause, 1.0, WEIGHT, USER_ID),
            hex("6a 17 00 00 00 00 03 e8 01 00 50 00 02 00 00 00 0d ba 9d 76 ef 1c 43")[..]
        );
        assert_eq!(
            encode_command(Command::Stop, 1.0, WEIGHT, USER_ID),
            hex("6a 17 00 00 00 00 03 e8 01 00 50 00 00 00 00 00 0d ba 9d 76 ef 1e 43")[..]
        );
    }

    fn status(belt: BeltState, speed: f64, dist: u32, steps: u32, kcal: u16, dur: f64) -> Vec<u8> {
        encode_status(belt, false, speed, speed, dist, steps, kcal, dur).to_vec()
    }

    #[test]
    fn decode_metric_each_state() {
        for (belt, flag) in [
            (BeltState::Stopped, 0x00),
            (BeltState::Starting, 0x18),
            (BeltState::Running, 0x08),
            (BeltState::Paused, 0x10),
        ] {
            let pkt = status(belt, 3.2, 1234, 1700, 52, 1403.5);
            let s = decode_status(&pkt).unwrap();
            assert!(matches!(s.belt_state(), st if st == belt), "{belt:?}");
            assert_eq!(s.flags & 0x18, flag);
            assert!(!s.imperial());
            assert!((s.speed_mps - 3.2 / 3.6).abs() < 1e-9);
            assert_eq!(s.distance_m, 1234);
            assert_eq!(s.steps, Some(1700));
            assert_eq!(s.kcal, 52);
            assert!((s.duration_s - 1403.5).abs() < 1e-9);
        }
    }

    #[test]
    fn decode_imperial() {
        let pkt = encode_status(
            BeltState::Running,
            true,
            2.0 * 1.609344,
            2.0 * 1.609344,
            1609,
            840,
            30,
            1931.0,
        );
        let s = decode_status(&pkt).unwrap();
        assert!(s.imperial());
        // 2 mph ≈ 0.894 m/s
        assert!((s.speed_mps - 2.0 * 0.44704).abs() < 0.001);
        // 1 mi of distance read back in metres
        assert!((s.distance_m as f64 - 1609.344).abs() < 2.0);
    }

    // SupeRun golden packets, from the reference controller
    // (hdxsfbr/walkingpad pitpat_controller/protocol.py), speeds in km/h
    #[test]
    fn superun_golden_vectors() {
        let v = Variant::SupeRun;
        let w = |seq, c, kmh| v.frame(seq, &v.encode_command(c, kmh, WEIGHT, USER_ID));
        assert_eq!(v.frame(7, &HEARTBEAT), hex("4d 00 07 05 6a 05 fd f8 43"));
        assert_eq!(
            w(3, Command::Start, 2.0),
            hex("4d 00 03 17 6a 17 00 00 00 00 07 c4 05 00 58 00 0c 00 00 00 00 00 0c 55 70 ac 43")
        );
        assert_eq!(
            w(4, Command::SetSpeed, 4.5),
            hex("4d 00 04 17 6a 17 00 00 00 00 11 7a 01 00 58 00 0c 00 00 00 00 00 0c 55 70 00 43")
        );
        assert_eq!(
            w(5, Command::Pause, 3.0),
            hex("4d 00 05 17 6a 17 00 00 00 00 0b a7 05 00 58 00 0a 00 00 00 00 00 0c 55 70 c5 43")
        );
        assert_eq!(
            w(6, Command::Stop, 3.0),
            hex("4d 00 06 17 6a 17 00 00 00 00 00 00 05 00 58 00 08 00 00 00 00 00 00 00 00 42 43")
        );
        // the classic variant is unchanged and unframed
        assert_eq!(Variant::PitPat.frame(9, &HEARTBEAT), HEARTBEAT.to_vec());
    }

    /// Two real PitPat-T01 notifications (2026-11-18 trace, 20 counted
    /// steps between them; the count started just after a step).
    #[test]
    fn superun_t01_trace() {
        let a = hex(
            "4d 04 ea 2f 68 2f 00 08 fc 08 fc 00 00 00 64 00 00 00 00 00 00 00 00 08 00 02 59 90 5a 1b 8b 17 70 00 05 00 00 00 00 00 00 00 00 00 18 04 66 00 d5 8f 43",
        );
        let b = hex(
            "4d 04 08 2f 68 2f 00 08 fc 08 fc 00 00 00 6e 00 00 00 00 00 00 00 00 09 00 02 98 10 5a 1b 8b 17 70 00 05 00 00 00 00 00 00 00 00 00 18 04 66 00 e8 f8 43",
        );
        let a = Variant::SupeRun.decode_status(&a).unwrap();
        let b = Variant::SupeRun.decode_status(&b).unwrap();
        assert_eq!(a.belt_state(), BeltState::Running);
        assert!(!a.imperial(), "units already normalised");
        assert_eq!((a.steps, b.steps), (Some(213), Some(232)));
        // metres, despite the imperial flag: +10 m in 16 s at 2.3 km/h
        assert_eq!((a.distance_m, b.distance_m), (100, 110));
        assert!(
            (a.speed_mps * 3.6 - 2.31).abs() < 0.01,
            "{}",
            a.speed_mps * 3.6
        );
        assert!((a.target_speed_kmh - 2.31).abs() < 0.01);
        assert_eq!(a.kcal, 8);
        // the same bytes are not a valid classic packet's speed/distance
        assert!(Variant::SupeRun.decode_status(&hex("68 2f 00")).is_none());
    }

    #[test]
    fn superun_status_roundtrip() {
        let raw = encode_superun_status(1, BeltState::Running, 4.0, 4.0, 420, 600, 12, 60.0);
        assert_eq!(&raw[..4], &[0x4d, 0x04, 0x01, 0x2f]);
        assert_eq!(raw.len(), 51);
        let s = decode_superun_status(&raw).unwrap();
        assert!((s.speed_mps * 3.6 - 4.0).abs() < 0.01);
        assert_eq!((s.distance_m, s.steps, s.kcal), (420, Some(600), 12));
        assert!(decode_superun_status(&raw[4..]).is_none(), "unframed");
        // every speed on the 0.1 km/h grid reads back exactly (§3.6)
        for tenths in 10..=60 {
            let v = tenths as f64 / 10.0;
            let raw = encode_superun_status(1, BeltState::Running, v, v, 0, 0, 0, 0.0);
            let s = decode_superun_status(&raw).unwrap();
            assert!((s.target_speed_kmh - v).abs() < 1e-9, "{v}");
            assert!((s.speed_mps * 3.6 - v).abs() < 1e-9, "{v}");
        }
        assert!(decode_superun_status(&raw[..14]).is_none(), "short");
    }

    #[test]
    fn short_packet_invalid() {
        let mut pkt = status(BeltState::Running, 1.0, 0, 0, 0, 0.0);
        pkt.truncate(30); // 30-byte packet → invalid
        assert!(decode_status(&pkt).is_none());
        assert!(decode_status(&[]).is_none());
    }
}

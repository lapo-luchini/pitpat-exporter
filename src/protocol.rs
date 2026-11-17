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

/// Decode a 31-byte status notification.
///
/// Returns `None` for any packet that is not a valid status notification
/// (the caller must log it and count it as invalid).
pub fn decode_status(pkt: &[u8]) -> Option<Status> {
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

    #[test]
    fn short_packet_invalid() {
        let mut pkt = status(BeltState::Running, 1.0, 0, 0, 0, 0.0);
        pkt.truncate(30); // 30-byte packet → invalid
        assert!(decode_status(&pkt).is_none());
        assert!(decode_status(&[]).is_none());
    }
}

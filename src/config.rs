// SPDX-License-Identifier: ISC

use clap::Parser;
use std::path::PathBuf;

/// Daemon configuration (§5): CLI flags of `pitpat daemon`, each readable
/// from an env var `PITPAT_<NAME>`. No config file.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "pitpat",
    about = "PitPat treadmill control + Prometheus export"
)]
pub struct Config {
    /// HTTP listen address.
    #[arg(long, env = "PITPAT_LISTEN", default_value = "127.0.0.1:9839")]
    pub listen: String,

    /// File holding the shared secret; required if --listen is not loopback.
    #[arg(long, env = "PITPAT_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,

    /// BlueZ adapter: name (e.g. hci0) or controller MAC address, which is
    /// stable across controller resets; default = BlueZ default adapter.
    #[arg(long, env = "PITPAT_ADAPTER")]
    pub adapter: Option<String>,

    /// Treadmill MAC address; default = scan for the fba0 advertising UUID.
    #[arg(long, env = "PITPAT_ADDRESS")]
    pub address: Option<String>,

    /// Directory for sessions.jsonl and totals.json.
    #[arg(long, env = "PITPAT_STATE_DIR", default_value_os_t = default_state_dir())]
    pub state_dir: PathBuf,

    /// User weight in kg, sent with commands (used for calories).
    #[arg(long, env = "PITPAT_WEIGHT_KG", default_value_t = 80, value_parser = clap::value_parser!(u8).range(20..=250))]
    pub weight_kg: u8,

    /// User ID sent with every command.
    #[arg(long, env = "PITPAT_USER_ID", default_value_t = 58965456623)]
    pub user_id: u64,

    /// Minimum allowed belt speed in km/h.
    #[arg(long, env = "PITPAT_MIN_SPEED", default_value_t = 1.0)]
    pub min_speed: f64,

    /// Maximum allowed belt speed in km/h.
    #[arg(long, env = "PITPAT_MAX_SPEED", default_value_t = 6.0)]
    pub max_speed: f64,

    /// Target speed used by "start" (km/h).
    #[arg(long, env = "PITPAT_DEFAULT_SPEED", default_value_t = 2.0)]
    pub default_speed: f64,

    /// Speed presets shown in the GUIs (km/h, up to 6 values).
    #[arg(long, env = "PITPAT_PRESETS", value_delimiter = ',', num_args = 1..=6, default_value = "2,3,4")]
    pub presets: Vec<f64>,

    /// Use the simulator instead of real Bluetooth.
    #[arg(long, env = "PITPAT_SIMULATE")]
    pub simulate: bool,

    /// tracing filter (e.g. info, debug, pitpat=debug,bluer=warn).
    #[arg(long, env = "PITPAT_LOG", default_value = "info")]
    pub log: String,
}

/// Loopback check for a `host:port` listen address (§6.1): parsed through
/// SocketAddr so IPv6 forms like `[::1]:9839` work, plus a literal
/// `localhost` fallback (no name resolution).
pub fn is_loopback_listen(listen: &str) -> bool {
    if listen == "localhost" {
        return true;
    }
    match listen.parse::<std::net::SocketAddr>() {
        Ok(addr) => addr.ip().is_loopback(),
        Err(_) => listen
            .rsplit_once(':')
            .is_some_and(|(h, p)| h == "localhost" && !p.is_empty() && p.parse::<u16>().is_ok()),
    }
}

impl Config {
    /// Validate speed bounds; presets must fit inside [min, max].
    pub fn validate(&self) -> Result<(), String> {
        if !(self.min_speed >= 0.0 && self.max_speed > self.min_speed) {
            return Err(format!(
                "min_speed ({}) must be >= 0 and < max_speed ({})",
                self.min_speed, self.max_speed
            ));
        }
        if !(self.default_speed >= self.min_speed && self.default_speed <= self.max_speed) {
            return Err(format!(
                "default_speed ({}) must be within [min_speed, max_speed] [{}, {}]",
                self.default_speed, self.min_speed, self.max_speed
            ));
        }
        for &p in &self.presets {
            if !(p >= self.min_speed && p <= self.max_speed) {
                return Err(format!(
                    "preset {p} outside [{}, {}]",
                    self.min_speed, self.max_speed
                ));
            }
        }
        Ok(())
    }
}

/// Config subset the core state machine and packet encoders need.
#[derive(Debug, Clone)]
pub struct DaemonCfg {
    pub weight_kg: u8,
    pub user_id: u64,
    pub min_speed: f64,
    pub max_speed: f64,
    pub default_speed: f64,
    pub presets: Vec<f64>,
}

impl From<&Config> for DaemonCfg {
    fn from(c: &Config) -> Self {
        Self {
            weight_kg: c.weight_kg,
            user_id: c.user_id,
            min_speed: c.min_speed,
            max_speed: c.max_speed,
            default_speed: c.default_speed,
            presets: c.presets.clone(),
        }
    }
}

fn default_state_dir() -> PathBuf {
    if let Ok(dirs) = std::env::var("STATE_DIRECTORY") {
        if let Some(first) = dirs.split(':').next() {
            if !first.is_empty() {
                return PathBuf::from(first);
            }
        }
    }
    if let Ok(x) = std::env::var("XDG_STATE_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("pitpat");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home).join(".local/state/pitpat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates() {
        let mut cfg = Config::parse_from(["pitpat"]);
        assert!(cfg.validate().is_ok());
        cfg.min_speed = 6.0;
        cfg.max_speed = 1.0;
        assert!(cfg.validate().is_err());
        let cfg = Config::parse_from(["pitpat", "--min-speed", "3.2", "--max-speed", "4.0"]);
        assert!(cfg.validate().is_err(), "default 2.0 below min 3.2");
    }
}

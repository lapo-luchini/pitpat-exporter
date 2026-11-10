// SPDX-License-Identifier: ISC

//! Client subcommands: talk HTTP/JSON to the local daemon (§8).

use clap::Args;
use serde_json::json;
use std::process::ExitCode;
use std::time::Duration;

/// Flags shared by all client subcommands (§5, bottom).
#[derive(Args, Debug)]
pub struct Client {
    #[arg(long, env = "PITPAT_URL", default_value = "http://127.0.0.1:9839")]
    pub url: String,
    #[arg(long, env = "PITPAT_TOKEN_FILE")]
    pub token_file: Option<std::path::PathBuf>,
}

/// Exit codes: 0 ok, 1 daemon error response, 2 daemon unreachable (§8).
pub fn exit_for(r: Result<String, CliError>) -> ExitCode {
    match r {
        Ok(text) => {
            if !text.is_empty() {
                println!("{}", text);
            }
            ExitCode::SUCCESS
        }
        Err(CliError::Daemon(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
        Err(CliError::Unreachable(msg)) => {
            eprintln!("unreachable: {msg}");
            ExitCode::from(2)
        }
    }
}

pub enum CliError {
    /// 4xx/5xx from the daemon.
    Daemon(String),
    /// No HTTP connection at all.
    Unreachable(String),
}

fn new_agent() -> ureq::Agent {
    let cfg = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build();
    cfg.into()
}

pub struct Api {
    url: String,
    token: Option<String>,
}

impl Api {
    pub fn new(c: &Client) -> Self {
        let token = c
            .token_file
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| t.trim().to_string());
        Self {
            url: c.url.trim_end_matches('/').to_string(),
            token,
        }
    }

    /// One HTTP exchange (5 s timeout). Mutating methods get the JSON
    /// Content-Type (§6.1) plus the Bearer token when one is configured.
    pub fn call(
        &self,
        agent: &ureq::Agent,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, CliError> {
        let url = format!("{}{}", self.url, path);
        let res = if method == "GET" {
            let mut b = agent.get(url);
            b = match self.token.clone() {
                Some(t) => b.header("Authorization", format!("Bearer {t}")),
                None => b,
            };
            b.call()
        } else if method == "DELETE" {
            let mut b = agent.delete(url).header("Content-Type", "application/json");
            b = match self.token.clone() {
                Some(t) => b.header("Authorization", format!("Bearer {t}")),
                None => b,
            };
            b.call()
        } else {
            let mut b = agent.post(url).header("Content-Type", "application/json");
            b = match self.token.clone() {
                Some(t) => b.header("Authorization", format!("Bearer {t}")),
                None => b,
            };
            b.send(body.unwrap_or("{}"))
        };
        match res {
            Ok(mut res) => {
                let code = res.status().as_u16();
                let Ok(text) = res.body_mut().read_to_string() else {
                    return Err(CliError::Daemon(format!("{code} (unreadable body)")));
                };
                if (200..300).contains(&code) {
                    Ok(text)
                } else {
                    let detail = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
                        .unwrap_or(text);
                    Err(CliError::Daemon(format!("{code}: {detail}")))
                }
            }
            Err(err) => Err(CliError::Unreachable(err.to_string())),
        }
    }
}

fn state(agent: &ureq::Agent, api: &Api) -> Result<serde_json::Value, CliError> {
    let body = api.call(agent, "GET", "/api/state", None)?;
    serde_json::from_str(&body).map_err(|e| CliError::Daemon(e.to_string()))
}

pub fn status(api: &Api, json_out: bool) -> Result<String, CliError> {
    let agent = new_agent();
    let st = state(&agent, api)?;
    if json_out {
        return Ok(serde_json::to_string_pretty(&st).expect("pretty"));
    }
    let conn = st
        .get("connected")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let s = st.get("state").and_then(|v| v.as_str()).unwrap_or("?");
    let speed = st.get("speed_kmh").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let dist = st
        .get("session")
        .filter(|s| !s.is_null())
        .and_then(|s| s.get("distance_m"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let life = st
        .get("lifetime")
        .and_then(|l| l.get("distance_m"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Ok(format!(
        "{} speed={:.1} km/h session={:.2} km lifetime={:.1} km",
        if conn { s } else { "disconnected" },
        speed,
        dist / 1000.0,
        life / 1000.0,
    ))
}

fn empty_body() -> Option<&'static str> {
    Some("{}")
}

pub fn start(api: &Api, speed: Option<f64>) -> Result<String, CliError> {
    let agent = new_agent();
    match speed {
        Some(v) => {
            let body = serde_json::to_string(&json!({ "speed_kmh": v }))
                .map_err(|e| CliError::Daemon(e.to_string()))?;
            api.call(&agent, "POST", "/api/start", Some(&body))
        }
        None => api.call(&agent, "POST", "/api/start", Some("{}")),
    }
    .map(|_| {
        format!(
            "start queued ({})",
            speed.map_or("default speed".to_string(), |v| v.to_string())
        )
    })
}

pub fn pause(api: &Api) -> Result<String, CliError> {
    let agent = new_agent();
    api.call(&agent, "POST", "/api/pause", empty_body())
        .map(|_| "paused".into())
}

pub fn stop(api: &Api) -> Result<String, CliError> {
    let agent = new_agent();
    api.call(&agent, "POST", "/api/stop", empty_body())
        .map(|_| "stopping".into())
}

/// running → pause; otherwise start (KDE shortcut convenience).
pub fn toggle(api: &Api) -> Result<String, CliError> {
    let agent = new_agent();
    let st = state(&agent, api)?;
    let running = st.get("state").and_then(|v| v.as_str()) == Some("running");
    if running {
        pause(api)
    } else {
        start(api, None)
    }
}

/// Parse `3.2` | `+0.1` | `-0.1` | `preset 2` (§8).
pub fn speed(api: &Api, arg: &str) -> Result<String, CliError> {
    let agent = new_agent();
    let arg = arg.trim();
    let body = if let Some(n) = arg.strip_prefix("preset").map(str::trim) {
        let i: usize = n
            .parse()
            .map_err(|_| CliError::Daemon("preset must be a number".into()))?;
        serde_json::to_string(&json!({ "preset": i })).expect("serialize")
    } else if arg.starts_with('+') || arg.starts_with('-') {
        let d: f64 = arg
            .parse()
            .map_err(|_| CliError::Daemon("delta must be numeric".into()))?;
        serde_json::to_string(&json!({ "delta_kmh": d })).expect("serialize")
    } else {
        let v: f64 = arg.parse().map_err(|_| {
            CliError::Daemon("speed must be numeric, a ±0.1 delta, or 'preset N'".into())
        })?;
        serde_json::to_string(&json!({ "speed_kmh": v })).expect("serialize")
    };
    api.call(&agent, "POST", "/api/speed", Some(&body))
        .map(|_| "speed queued".into())
}

/// `3km`/`2500m` → metres.
pub fn parse_distance(s: &str) -> Option<u32> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(v) = s.strip_suffix("km") {
        v.trim().parse::<f64>().ok().map(|v| (v * 1000.0) as u32)
    } else if let Some(v) = s.strip_suffix('m') {
        v.trim().parse::<u32>().ok()
    } else {
        s.parse::<u32>().ok()
    }
}

/// `30m`, `1h15m`, `1h`, `90s`, plain seconds → seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let mut total = 0.0f64;
    let mut num = String::new();
    let mut matched = false;
    for c in s.trim().chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().ok()?;
            num.clear();
            total += match c.to_ascii_lowercase() {
                'h' => v * 3600.0,
                'm' => v * 60.0,
                's' => v,
                _ => return None,
            };
            matched = true;
        }
    }
    if matched {
        if num.is_empty() {
            Some(total)
        } else {
            num.parse::<f64>().ok().map(|v| total + v)
        }
    } else {
        s.trim().parse().ok()
    }
}

pub fn target(
    api: &Api,
    distance: Option<String>,
    time: Option<String>,
    clear: bool,
) -> Result<String, CliError> {
    let agent = new_agent();
    if clear {
        return api
            .call(&agent, "DELETE", "/api/target", None)
            .map(|_| "target cleared".into());
    }
    if distance.is_none() && time.is_none() {
        return Err(CliError::Daemon("--distance and/or --time required".into()));
    }
    let mut b = serde_json::Map::new();
    if let Some(d) = distance {
        let dm =
            parse_distance(&d).ok_or_else(|| CliError::Daemon(format!("bad distance: {d}")))?;
        b.insert("distance_m".into(), json!(dm));
    }
    if let Some(t) = time {
        let ts = parse_duration(&t).ok_or_else(|| CliError::Daemon(format!("bad time: {t}")))?;
        b.insert("duration_s".into(), json!(ts));
    }
    api.call(
        &agent,
        "POST",
        "/api/target",
        Some(&serde_json::to_string(&json!(b)).expect("s")),
    )
    .map(|_| "target set".into())
}

pub fn sessions(api: &Api, n: usize) -> Result<String, CliError> {
    let agent = new_agent();
    let body = api.call(&agent, "GET", &format!("/api/sessions?limit={n}"), None)?;
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&body).map_err(|e| CliError::Daemon(e.to_string()))?;
    let mut out = String::new();
    for s in list {
        out.push_str(&format!(
            "{}  {:>7.2} km  {:>6} steps  {:>4} kcal  {:>5.0} min\n",
            s.get("start").and_then(|v| v.as_str()).unwrap_or("?"),
            s.get("distance_m").and_then(|v| v.as_f64()).unwrap_or(0.0) / 1000.0,
            s.get("steps").and_then(|v| v.as_u64()).unwrap_or(0),
            s.get("kcal").and_then(|v| v.as_u64()).unwrap_or(0),
            s.get("duration_s").and_then(|v| v.as_f64()).unwrap_or(0.0) / 60.0,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(parse_distance("3km"), Some(3000));
        assert_eq!(parse_distance("3 km"), Some(3000));
        assert_eq!(parse_distance("2500m"), Some(2500));
        assert_eq!(parse_distance("2500"), Some(2500));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m"), Some(1800.0));
        assert_eq!(parse_duration("1h15m"), Some(4500.0));
        assert_eq!(parse_duration("90s"), Some(90.0));
        assert_eq!(parse_duration("1800"), Some(1800.0));
        assert_eq!(parse_duration("1h"), Some(3600.0));
        assert!(parse_duration("30x").is_none());
    }
}

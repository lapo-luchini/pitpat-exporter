// SPDX-License-Identifier: ISC

//! axum routes, auth, Host/CSRF checks (§6).

use crate::core::{ApiCmd, QUEUE_CAP, Snapshot};
use crate::metrics::Metrics;
use axum::extract::{DefaultBodyLimit, Query, State as AxState};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt as _;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};

/// Everything the router needs; the latest snapshot lives in a shared
/// slot refreshed by the events bridge task in main.
pub struct AppState {
    pub snap: Mutex<Option<Snapshot>>,
    pub api_tx: mpsc::Sender<ApiCmd>,
    pub events: broadcast::Sender<Snapshot>,
    pub token: Option<String>,
    /// Host part of the configured listen address.
    pub listen_host: String,
    pub presets: Vec<f64>,
    pub min_speed: f64,
    pub max_speed: f64,
    pub state_dir: std::path::PathBuf,
    pub metrics: Arc<Metrics>,
    pub page: &'static str,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/sessions", get(api_sessions))
        .route("/api/start", post(api_start))
        .route("/api/pause", post(api_pause))
        .route("/api/stop", post(api_stop))
        .route("/api/speed", post(api_speed))
        .route("/api/target", post(api_target).delete(api_target_clear))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(state.clone(), secure))
        .with_state(state)
}

/// Host allowlist for all requests (§6.1), Content-Type + Bearer token for
/// mutating ones. No CORS headers are ever sent, so browser cross-site
/// mutating requests must pass a preflight.
async fn secure(
    AxState(state): AxState<SharedState>,
    method: Method,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return err_json(StatusCode::BAD_REQUEST, "missing Host header");
    };
    let host = host.to_ascii_lowercase();
    let (ip, _port) = host.rsplit_once(':').unwrap_or((host.as_str(), ""));
    let bare = ip.trim_start_matches('[').trim_end_matches(']');
    let allowed = ip == state.listen_host
        || host == state.listen_host
        || matches!(ip, "localhost" | "127.0.0.1" | "[::1]")
        || matches!(bare, "localhost" | "127.0.0.1" | "::1");
    if !allowed {
        return err_json(StatusCode::from_u16(421).unwrap(), "untrusted Host header");
    }

    if method == Method::POST || method == Method::DELETE {
        let ct = headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .map(|h| {
                h.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            });
        if ct.as_deref() != Some("application/json") {
            return err_json(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            );
        }
        if let Some(token) = &state.token {
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "));
            let ok = auth.map(|a| constant_time_eq(a, token)).unwrap_or(false);
            if !ok {
                return err_json(StatusCode::UNAUTHORIZED, "bad or missing token");
            }
        }
    }
    next.run(request).await
}

/// Constant-time equality for bearer tokens.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn err_json(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

// ------------------------------------------------------------------ reads

async fn index(AxState(state): AxState<SharedState>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        state.page,
    )
        .into_response()
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

async fn metrics(AxState(state): AxState<SharedState>) -> Response {
    let mut buf = String::new();
    match state.metrics.render(&mut buf) {
        Ok(()) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            buf,
        )
            .into_response(),
        Err(_) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "metric encoding failed"),
    }
}

fn current(state: &AppState) -> Option<Snapshot> {
    state.snap.lock().expect("snap mutex").clone()
}

async fn api_state(AxState(state): AxState<SharedState>) -> Response {
    match current(&state) {
        Some(snap) => Json(state_json(&state, &snap)).into_response(),
        None => err_json(StatusCode::SERVICE_UNAVAILABLE, "daemon starting"),
    }
}

fn state_json(state: &AppState, s: &Snapshot) -> serde_json::Value {
    // the open session is included in "today" (§6.2)
    let open = s.session.as_ref().map(|x| crate::session::SessionRec {
        start: x.start,
        end: x.start,
        distance_m: x.distance_m,
        steps: x.steps,
        kcal: x.kcal,
        duration_s: x.duration_s,
        avg_speed_kmh: 0.0,
        max_speed_kmh: x.max_speed_kmh,
        end_reason: "open".into(),
    });
    let today =
        crate::session::today_summary(&state.state_dir, open.as_ref(), jiff::Timestamp::now());
    let t = &s.totals;
    serde_json::json!({
        "connected": s.connected,
        "address": s.address,
        "state": s.state.as_str(),
        "imperial": s.imperial,
        "speed_kmh": s.speed_kmh,
        "target_speed_kmh": s.target_speed_kmh,
        "presets_kmh": state.presets,
        "min_speed_kmh": state.min_speed,
        "max_speed_kmh": state.max_speed,
        "session": s.session.as_ref().map(|x| serde_json::json!({
            "start": x.start,
            "distance_m": x.distance_m,
            "steps": x.steps,
            "kcal": x.kcal,
            "duration_s": x.duration_s,
            "max_speed_kmh": x.max_speed_kmh,
        })),
        "target": {
            "distance_m": s.target.distance_m,
            "duration_s": s.target.duration_s,
        },
        "today": today,
        "lifetime": {
            "distance_m": t.distance_m,
            "steps": t.steps,
            "kcal": t.kcal,
            "duration_s": t.moving_s,
            "sessions": t.sessions,
        },
    })
}

async fn api_sessions(
    AxState(state): AxState<SharedState>,
    Query(q): Query<SessionsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(20).min(500);
    Json(crate::session::read_sessions(&state.state_dir, limit)).into_response()
}

#[derive(Deserialize, Default)]
struct SessionsQuery {
    limit: Option<usize>,
}

// ------------------------------------------------------------------ SSE

async fn api_events(AxState(state): AxState<SharedState>) -> Response {
    let rx = state.events.subscribe();
    let seeding = current(&state);
    let initial_stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
    > = Box::pin(futures::stream::iter(
        seeding.into_iter().map(|snap| Ok(seed_event(snap))),
    ));
    let tail: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
    > = Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        let data = loop {
            match rx.recv().await {
                Ok(s) => break snapshot_json(&s).to_string(),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        };
        Some((Ok(state_event(data)), rx))
    }));
    Sse::new(initial_stream.chain(tail))
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

fn seed_event(s: Snapshot) -> Event {
    Event::default()
        .event("state")
        .data(snapshot_json(&s).to_string())
}

fn state_event(data: String) -> Event {
    Event::default().event("state").data(data)
}

fn snapshot_json(s: &Snapshot) -> serde_json::Value {
    serde_json::json!({
        "state": s.state.as_str(),
        "connected": s.connected,
        "speed_kmh": s.speed_kmh,
        "target_speed_kmh": s.target_speed_kmh,
        "session": s.session.as_ref().map(|x| serde_json::json!({
            "start": x.start,
            "distance_m": x.distance_m,
            "steps": x.steps,
            "kcal": x.kcal,
            "duration_s": x.duration_s,
            "max_speed_kmh": x.max_speed_kmh,
        })),
        "target": {
            "distance_m": s.target.distance_m,
            "duration_s": s.target.duration_s,
        },
        "imperial": s.imperial,
    })
}

// -------------------------------------------------------------- commands

/// Fast prechecks + enqueue: 409 when disconnected, 503 on a full queue
/// (the authoritative check runs in the core drain, §6.2).
fn queue(
    state: &AppState,
    cmd: ApiCmd,
    needs_connection: bool,
) -> Result<serde_json::Value, (StatusCode, &'static str)> {
    let (connected, full) = match current(state) {
        None => return Err((StatusCode::SERVICE_UNAVAILABLE, "daemon starting")),
        Some(s) => (s.connected, s.queue_len >= QUEUE_CAP),
    };
    if needs_connection && !connected {
        return Err((StatusCode::CONFLICT, "not connected"));
    }
    if full {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "command queue full"));
    }
    match state.api_tx.try_send(cmd) {
        Ok(()) => Ok(serde_json::json!({ "queued": true })),
        Err(mpsc::error::TrySendError::Full(_)) => {
            Err((StatusCode::SERVICE_UNAVAILABLE, "command queue full"))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            Err((StatusCode::SERVICE_UNAVAILABLE, "daemon stopped"))
        }
    }
}

fn respond(r: Result<serde_json::Value, (StatusCode, &'static str)>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err((code, msg)) => err_json(code, msg),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StartBody {
    #[serde(default)]
    speed_kmh: Option<f64>,
}

async fn api_start(AxState(state): AxState<SharedState>, Json(b): Json<StartBody>) -> Response {
    if let Some(v) = b.speed_kmh {
        if !v.is_finite() || !(0.0..=100.0).contains(&v) {
            return err_json(StatusCode::BAD_REQUEST, "speed_kmh out of range");
        }
    }
    respond(queue(&state, ApiCmd::Start(b.speed_kmh), true))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Empty {}

async fn api_pause(AxState(state): AxState<SharedState>, Json(_): Json<Empty>) -> Response {
    respond(queue(&state, ApiCmd::Pause, true))
}

async fn api_stop(AxState(state): AxState<SharedState>, Json(_): Json<Empty>) -> Response {
    respond(queue(&state, ApiCmd::Stop, true))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeedBody {
    speed_kmh: Option<f64>,
    delta_kmh: Option<f64>,
    preset: Option<usize>,
}

async fn api_speed(AxState(state): AxState<SharedState>, Json(b): Json<SpeedBody>) -> Response {
    let n = b.speed_kmh.is_some() as usize
        + b.delta_kmh.is_some() as usize
        + b.preset.is_some() as usize;
    if n != 1 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "exactly one of speed_kmh, delta_kmh, preset",
        );
    }
    let cmd = if let Some(v) = b.speed_kmh {
        if !v.is_finite() || !(0.0..=100.0).contains(&v) {
            return err_json(StatusCode::BAD_REQUEST, "speed_kmh out of range");
        }
        ApiCmd::SpeedAbs(v)
    } else if let Some(d) = b.delta_kmh {
        if !d.is_finite() || !(-10.0..=10.0).contains(&d) {
            return err_json(StatusCode::BAD_REQUEST, "delta_kmh out of range");
        }
        ApiCmd::SpeedDelta(d)
    } else {
        let p = b.preset.unwrap();
        if p == 0 || p > state.presets.len() {
            return err_json(StatusCode::BAD_REQUEST, "preset is 1-based");
        }
        ApiCmd::Preset(p - 1)
    };
    respond(queue(&state, cmd, true))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetBody {
    distance_m: Option<u32>,
    duration_s: Option<f64>,
}

async fn api_target(AxState(state): AxState<SharedState>, Json(b): Json<TargetBody>) -> Response {
    if b.distance_m.is_none() && b.duration_s.is_none() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "distance_m and/or duration_s required",
        );
    }
    if let Err(msg) = validate_target(b.distance_m, b.duration_s) {
        return err_json(StatusCode::BAD_REQUEST, msg);
    }
    respond(queue(
        &state,
        ApiCmd::TargetSet(b.distance_m, b.duration_s),
        false,
    ))
}

async fn api_target_clear(AxState(state): AxState<SharedState>) -> Response {
    respond(queue(&state, ApiCmd::TargetClear, false))
}

fn validate_target(distance_m: Option<u32>, duration_s: Option<f64>) -> Result<(), &'static str> {
    if distance_m.is_none() && duration_s.is_none() {
        return Err("distance_m and/or duration_s required");
    }
    if distance_m.is_some_and(|v| v == 0 || v > 100_000_000) {
        return Err("distance_m out of range");
    }
    if duration_s.is_some_and(|v| !v.is_finite() || !(1.0..=100_000_000.0).contains(&v)) {
        return Err("duration_s out of range");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Snapshot, State, Target};
    use crate::metrics::Metrics;
    use crate::session::Totals;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn mkstate(token: Option<String>, dir: &std::path::Path) -> SharedState {
        let (events, _) = tokio::sync::broadcast::channel(8);
        let (api_tx, api_rx) = mpsc::channel(8);
        // keep the receiver alive without polling it (the daemon would be
        // the consumer in a real run): leak it for the test's lifetime
        std::mem::forget(api_rx);
        let snap = Mutex::new(Some(snapshot_min()));
        let metrics = Metrics::new();
        Arc::new(AppState {
            snap,
            api_tx,
            events,
            token,
            listen_host: "127.0.0.1".into(),
            presets: vec![2.0, 3.0, 4.0],
            min_speed: 1.0,
            max_speed: 6.0,
            state_dir: dir.to_path_buf(),
            metrics: Arc::new(metrics),
            page: include_str!("../web/index.html"),
        })
    }

    fn snapshot_min() -> Snapshot {
        Snapshot {
            connected: true,
            address: Some("AA:BB".into()),
            state: State::Stopped,
            imperial: false,
            speed_kmh: 0.0,
            target_speed_kmh: 2.0,
            queue_len: 0,
            session: None,
            target: Target::default(),
            totals: Totals::default(),
        }
    }

    fn req(
        method: Method,
        uri: &'static str,
        body: &'static str,
        host: &'static str,
        ct: Option<&'static str>,
        auth: Option<&'static str>,
    ) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("Host", host);
        if let Some(ct) = ct {
            b = b.header("Content-Type", ct);
        }
        if let Some(a) = auth {
            b = b.header("Authorization", a);
        }
        b.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn wrong_host_rejected() {
        let dir = std::env::temp_dir().join(format!("pitpat-http-{}", std::process::id()));
        let app = router(mkstate(None, &dir));
        let res = app
            .oneshot(req(
                Method::GET,
                "/api/state",
                "",
                "evil.example",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::from_u16(421).unwrap());
        let app = router(mkstate(None, &dir));
        let res = app
            .oneshot(req(
                Method::GET,
                "/api/state",
                "",
                "127.0.0.1:9839",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_needs_json_content_type() {
        let dir = std::env::temp_dir().join(format!("pitpat-http-ct-{}", std::process::id()));
        let app = router(mkstate(None, &dir));
        let res = app
            .oneshot(req(
                Method::POST,
                "/api/start",
                "{}",
                "localhost",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let app = router(mkstate(None, &dir));
        let res = app
            .oneshot(req(
                Method::POST,
                "/api/start",
                "{}",
                "localhost",
                Some("application/json"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("queued"));
    }

    #[tokio::test]
    async fn token_required_and_constant_time() {
        let dir = std::env::temp_dir().join(format!("pitpat-http-tok-{}", std::process::id()));
        let app = router(mkstate(Some("secret".into()), &dir));
        let res = app
            .oneshot(req(
                Method::POST,
                "/api/stop",
                "{}",
                "localhost",
                Some("application/json"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let app = router(mkstate(Some("secret".into()), &dir));
        let res = app
            .oneshot(req(
                Method::POST,
                "/api/stop",
                "{}",
                "localhost",
                Some("application/json"),
                Some("Bearer secret"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // GETs are read-only: no token needed
        let app = router(mkstate(Some("secret".into()), &dir));
        let res = app
            .oneshot(req(Method::GET, "/metrics", "", "localhost", None, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("pitpat_distance_meters_total"));
        // healthz says ok
        let app = router(mkstate(None, &dir));
        let res = app
            .oneshot(req(Method::GET, "/healthz", "", "localhost", None, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn state_json_has_required_fields() {
        let dir = std::env::temp_dir().join("pitpat-http-json");
        std::fs::create_dir_all(&dir).unwrap();
        let st = mkstate(None, &dir);
        let snap = snapshot_min();
        let j = state_json(&st, &snap);
        for field in [
            "connected",
            "address",
            "state",
            "imperial",
            "speed_kmh",
            "target_speed_kmh",
            "presets_kmh",
            "min_speed_kmh",
            "max_speed_kmh",
            "session",
            "target",
            "today",
            "lifetime",
        ] {
            assert!(j.get(field).is_some(), "missing {field}");
        }
        assert_eq!(
            j.get("lifetime").and_then(|v| v.get("distance_m")),
            Some(&serde_json::json!(0))
        );
        std::mem::forget(dir.clone());
    }
}

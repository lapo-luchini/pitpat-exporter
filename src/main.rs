// SPDX-License-Identifier: ISC

//! clap entry point: dispatch `daemon` to the server loop, the rest to the
//! HTTP client (§2).

mod ble;
mod cli;
mod config;
mod core;
mod http;
mod link;
mod metrics;
mod protocol;
mod session;
mod sim;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "pitpat",
    about = "PitPat treadmill control + Prometheus export"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Background service owning the BLE connection (§4);
    /// all flags also readable from PITPAT_* env vars (§5).
    Daemon {
        #[command(flatten)]
        cfg: config::Config,
    },
    /// One-line human summary, or the raw /api/state JSON with --json.
    Status {
        #[command(flatten)]
        client: cli::Client,
        #[arg(long)]
        json: bool,
    },
    /// Start the belt, optionally at a given speed (km/h).
    Start {
        #[command(flatten)]
        client: cli::Client,
        speed: Option<f64>,
    },
    Pause {
        #[command(flatten)]
        client: cli::Client,
    },
    /// running → pause; otherwise start.
    Toggle {
        #[command(flatten)]
        client: cli::Client,
    },
    Stop {
        #[command(flatten)]
        client: cli::Client,
    },
    /// `3.2` | `+0.1` | `-0.1` | `preset 2`.
    Speed {
        #[command(flatten)]
        client: cli::Client,
        value: String,
    },
    /// Set the auto-stop target; `pitpat target clear` clears it.
    Target {
        #[command(flatten)]
        client: cli::Client,
        #[command(subcommand)]
        cmd: Option<TargetSel>,
        #[arg(long = "distance")]
        distance: Option<String>,
        #[arg(long = "time")]
        time: Option<String>,
    },
    /// Recent finished sessions.
    Sessions {
        #[command(flatten)]
        client: cli::Client,
        #[arg(short = 'n', default_value_t = 10)]
        n: usize,
    },
}

#[derive(Subcommand, Debug)]
enum TargetSel {
    Clear,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { cfg } => run_daemon(cfg),
        Cmd::Status { client, json } => cli::exit_for(cli::status(&cli::Api::new(&client), json)),
        Cmd::Start { client, speed } => cli::exit_for(cli::start(&cli::Api::new(&client), speed)),
        Cmd::Pause { client } => cli::exit_for(cli::pause(&cli::Api::new(&client))),
        Cmd::Toggle { client } => cli::exit_for(cli::toggle(&cli::Api::new(&client))),
        Cmd::Stop { client } => cli::exit_for(cli::stop(&cli::Api::new(&client))),
        Cmd::Speed { client, value } => cli::exit_for(cli::speed(&cli::Api::new(&client), &value)),
        Cmd::Target {
            client,
            cmd,
            distance,
            time,
        } => {
            let clear = matches!(cmd, Some(TargetSel::Clear));
            cli::exit_for(cli::target(&cli::Api::new(&client), distance, time, clear))
        }
        Cmd::Sessions { client, n } => cli::exit_for(cli::sessions(&cli::Api::new(&client), n)),
    }
}

// ------------------------------------------------------------------ daemon

fn run_daemon(cfg: config::Config) -> ExitCode {
    init_logging(&cfg);
    // a non-loopback listen without --token-file is refused (§5/§6.1)
    let loopback = config::is_loopback_listen(&cfg.listen);
    if !loopback && cfg.token_file.is_none() {
        let listen_host = cfg
            .listen
            .rsplit_once(':')
            .map_or(cfg.listen.as_str(), |(h, _)| h);
        eprintln!("refusing --listen {listen_host} without --token-file");
        return ExitCode::FAILURE;
    }
    if let Err(err) = cfg.validate() {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    // a token that cannot be read must not silently disable auth
    // (REVIEW item 3)
    let token = match &cfg.token_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(t) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    eprintln!(
                        "refusing to start: --token-file {} is empty",
                        path.display()
                    );
                    return ExitCode::FAILURE;
                }
                Some(t)
            }
            Err(err) => {
                eprintln!(
                    "refusing to start: cannot read --token-file {}: {err}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    // current_thread keeps RSS low (§1, goal 6)
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(daemon_main(cfg, token))
}

fn init_logging(cfg: &config::Config) {
    use tracing_subscriber::EnvFilter;
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log))
        .with_writer(std::io::stderr);
    // no timestamps when running under journald (§12)
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        fmt.without_time().init();
    } else {
        fmt.init();
    }
}

async fn daemon_main(cfg: config::Config, token: Option<String>) -> ExitCode {
    use tokio::sync::{broadcast, mpsc};

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "pitpat daemon starting"
    );

    let totals = session::load_totals(&cfg.state_dir);
    let metrics = std::sync::Arc::new(metrics::Metrics::new());
    metrics.replay_totals(&totals);

    let (events, _ev) = broadcast::channel::<core::Snapshot>(8);
    let (api_tx, api_rx) = mpsc::channel::<core::ApiCmd>(32);
    let listen_host = cfg
        .listen
        .rsplit_once(':')
        .map_or(cfg.listen.as_str(), |(h, _)| h);
    let http_state = std::sync::Arc::new(http::AppState {
        snap: std::sync::Mutex::new(None),
        api_tx: api_tx.clone(),
        events: events.clone(),
        token,
        listen_host: listen_host.to_string(),
        loopback: config::is_loopback_listen(&cfg.listen),
        presets: cfg.presets.clone(),
        min_speed: cfg.min_speed,
        max_speed: cfg.max_speed,
        state_dir: cfg.state_dir.clone(),
        metrics: metrics.clone(),
        page: include_str!("../web/index.html"),
    });

    // bridge: latest snapshot from the event channel into the shared slot
    let bridge_state = http_state.clone();
    let mut bridge_rx = events.subscribe();
    let bridge = tokio::spawn(async move {
        let slot = &bridge_state.snap;
        loop {
            match bridge_rx.recv().await {
                Ok(s) => *slot.lock().expect("snap mutex") = Some(s),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let router = http::router(http_state.clone());
    let listener = match tokio::net::TcpListener::bind(&cfg.listen).await {
        Ok(l) => l,
        Err(err) => {
            tracing::error!(error = %err, listen = %cfg.listen, "cannot bind HTTP");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(listen = %cfg.listen, "HTTP ready");
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).await {
            tracing::error!(error = %err, "HTTP server failed");
        }
    });

    // signal bookkeeping (§4.6): TERM/INT → Ctrl::Shutdown; USR1 sets the
    // simulators's drop flag (§4.7). Everything flows through one task.
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<core::Ctrl>(4);
    let sim_flag = if cfg.simulate {
        tracing::info!("simulator link enabled (--simulate)");
        Some(sim::SimLink::new(cfg.weight_kg))
    } else {
        None
    };
    tokio::spawn(signal_task(
        ctrl_tx,
        sim_flag.as_ref().map(|s| s.drop_flag()),
    ));

    let mut link = match sim_flag {
        Some(s) => link::Link::Sim(s),
        None => link::Link::Ble(ble::BleLink::new(cfg.adapter.clone(), cfg.address.clone())),
    };
    let mut core = core::Core::new(
        config::DaemonCfg::from(&cfg),
        totals,
        metrics.clone(),
        events,
        api_rx,
        cfg.state_dir.clone(),
    );

    // reconnect loop: exponential backoff 1 → 2 → … 30 s, forever (§4.1)
    let mut backoff_s = 1u64;
    loop {
        core.begin_connection(None);
        // a shutdown signal must not wait for the whole scan/connect
        // attempt (REVIEW item 6)
        let connect_outcome = {
            let connect = link.connect();
            futures::pin_mut!(connect);
            tokio::select! {
                outcome = &mut connect => Some(outcome),
                c = ctrl_rx.recv() => match c {
                    Some(core::Ctrl::Shutdown) | None => None,
                },
            }
        };
        let mut conn = match connect_outcome {
            Some(Ok(conn)) => {
                let addr = conn.address();
                core.begin_connection(Some(addr.clone()));
                tracing::info!(address = %addr, "link established");
                backoff_s = 1;
                conn
            }
            Some(Err(err)) => {
                tracing::warn!(?err, "connection failed");
                core.on_disconnect(core::DeadReason::Link);
                core.metrics.reconnects_total.inc();
                if sleep_or_shutdown(backoff_s, &mut ctrl_rx).await {
                    core.on_disconnect(core::DeadReason::Shutdown);
                    break;
                }
                backoff_s = (backoff_s * 2).min(30);
                continue;
            }
            None => {
                // shutdown while connecting
                core.on_disconnect(core::DeadReason::Shutdown);
                break;
            }
        };
        let reason = core::pump(&mut core, &mut conn, &mut ctrl_rx).await;
        if reason == core::DeadReason::Shutdown {
            shutdown_flow(&mut core, &mut conn).await;
            break;
        }
        tracing::warn!(?reason, address = %conn.address(), "link lost, reconnecting");
        // drop the LE link even when the notify stream died: BlueZ would
        // keep the treadmill locked against its phone app otherwise
        // (REVIEW item 6)
        conn.disconnect().await;
        core.on_disconnect(reason);
        core.metrics.reconnects_total.inc();
        if sleep_or_shutdown(backoff_s, &mut ctrl_rx).await {
            core.on_disconnect(core::DeadReason::Shutdown);
            break;
        }
        backoff_s = (backoff_s * 2).min(30);
    }

    server.abort();
    bridge.abort();
    core.persist();
    ExitCode::SUCCESS
}

/// SIGTERM/SIGINT → Ctrl::Shutdown (§4.6). With `--simulate`, SIGUSR1
/// flips the simulator's drop flag so reconnect logic can be tested by
/// hand (§4.7).
async fn signal_task(
    tx: tokio::sync::mpsc::Sender<core::Ctrl>,
    sim_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) {
    use std::sync::atomic::Ordering;
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    loop {
        if sim_flag.is_some() {
            let mut usr1 = signal(SignalKind::user_defined1()).expect("SIGUSR1 handler");
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = usr1.recv() => {
                    tracing::info!("SIGUSR1: dropping simulated link");
                    if let Some(f) = &sim_flag {
                        f.store(true, Ordering::Relaxed);
                    }
                }
            }
        } else {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
            }
        }
    }
    tracing::info!("shutdown signal received");
    let _ = tx.send(core::Ctrl::Shutdown).await;
}

/// On SIGTERM while the belt is moving (§4.6): send stop, wait up to 3 s
/// for the treadmill to confirm via the notification cycle, then close
/// the session (end_reason shutdown), persist totals, disconnect.
async fn shutdown_flow(core: &mut core::Core, conn: &mut link::Conn) {
    if core.moving() {
        tracing::info!("stopping the belt before exit");
        core.queue_api(core::ApiCmd::Stop).ok();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while core.state != core::State::Stopped {
            if std::time::Instant::now() >= deadline {
                tracing::warn!("belt not confirmed stopped within 3 s, exiting anyway");
                break;
            }
            if let Ok(Ok(raw)) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                conn.recv_notification(),
            )
            .await
            {
                core.on_packet(&raw);
                core.drain_api_cmds();
                write_reply(core, conn).await;
            }
        }
    }
    core.on_disconnect(core::DeadReason::Shutdown);
    core.persist();
    // dropping conn disconnects
}

/// One reply packet per notification; shared with the shutdown flow.
/// Backoff sleep that wakes immediately on a shutdown signal; returns
/// true when shutdown was requested.
async fn sleep_or_shutdown(
    secs: u64,
    ctrl_rx: &mut tokio::sync::mpsc::Receiver<core::Ctrl>,
) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(secs), ctrl_rx.recv())
        .await
        .unwrap_or(None)
        .is_some()
}

async fn write_reply(core: &mut core::Core, conn: &mut link::Conn) {
    let pkt = core.next_reply();
    if conn.write_packet(&pkt).await.is_ok() {
        core.write_succeeded();
    } else if pkt.as_slice() != protocol::HEARTBEAT {
        core.write_failed(pkt);
    }
}

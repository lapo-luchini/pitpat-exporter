// SPDX-License-Identifier: ISC
// transitional: remove as modules get wired into the daemon
#![allow(dead_code)]

mod ble;
mod config;
mod core;
mod http;
mod link;
mod metrics;
mod protocol;
mod session;
mod sim;

use clap::{Parser, Subcommand};

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
    Daemon,
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon => (),
    }
}

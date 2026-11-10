// SPDX-License-Identifier: ISC
// transitional: remove as modules get wired into the daemon
#![allow(dead_code)]

mod ble;
mod config;
mod link;
mod metrics;
mod protocol;
mod session;
mod sim;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pitpat",
    about = "PitPat treadmill control + Prometheus export"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Daemon,
}

fn main() {
    let _cli = Cli::parse();
}

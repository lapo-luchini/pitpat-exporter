// SPDX-License-Identifier: ISC
mod link;
mod protocol;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pitpat", about = "PitPat treadmill control + Prometheus export")]
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

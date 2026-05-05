//! Bris reference CLI: desktop and embedded Linux frontend.
//!
//! Subcommands (per `plan.org` Phase 6): `capture`, `calibrate`, `fix`,
//! `serve`, `replay`, `log`, `update`. None are implemented yet.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "bris",
    version,
    about = "Bris: digital sextant",
    long_about = "Continuous celestial navigation from a camera. \
                  See https://github.com/anomalyco/bris."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Capture frames from the camera (stub).
    Capture,
    /// Run the lens calibration workflow (stub).
    Calibrate,
    /// Compute a one-shot fix from a frame source (stub).
    Fix,
    /// Run the continuous engine and serve NMEA output (stub).
    Serve,
    /// Re-derive a fix from saved frames (stub).
    Replay,
    /// Sight log management (list/show/delete/restore/export) (stub).
    Log,
    /// Download and apply almanac/catalog/leap-second updates (stub).
    Update,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Capture
        | Command::Calibrate
        | Command::Fix
        | Command::Serve
        | Command::Replay
        | Command::Log
        | Command::Update => {
            anyhow::bail!("not yet implemented; see plan.org for the development roadmap");
        }
    }
}

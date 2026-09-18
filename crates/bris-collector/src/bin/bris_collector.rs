//! `bris-collector` binary entrypoint.
//!
//! Two families of subcommand:
//!
//! - `serve` (default when no subcommand is given) — reads
//!   configuration from environment variables, opens the store,
//!   binds the HTTP server, and serves until SIGTERM / SIGINT.
//! - Operator-driven maintenance: `reindex`, `soft-delete`,
//!   `retention-sweep`. These are never invoked automatically
//!   (not at startup, not as a deploy side effect, not from the
//!   HTTP surface) — the durable-store invariant requires that
//!   soft-delete and hard-delete are explicit operator actions.

use std::sync::Arc;

use bris_collector::routes::AppState;
use bris_collector::store::Store;
use bris_collector::{build_app, Config};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// `bris-collector` — diagnostic-submission HTTP service and
/// its operator-driven store maintenance commands.
#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP server (default action).
    Serve,
    /// Rebuild the `SQLite` index mirror from the on-disk
    /// `manifest.json` files under `<data-root>/submissions/`.
    /// The index is a cache; this repairs it after loss,
    /// corruption, or drift (e.g. a crash between a submission's
    /// rename-into-place and its index insert).
    Reindex,
    /// Soft-delete one submission by id: writes a `deleted.json`
    /// sidecar (the manifest itself is never rewritten) and
    /// flips `soft_deleted_at` in the index. Files stay on disk.
    SoftDelete {
        /// The submission's ULID.
        id: String,
    },
    /// Hard-delete every soft-deleted submission whose
    /// `soft_deleted_at` is older than `--retention-days`.
    /// Explicit operator action only; never a deploy or startup
    /// side effect.
    RetentionSweep {
        /// Age (in days) past which a soft-deleted submission is
        /// eligible for hard deletion.
        #[arg(long, default_value_t = 30)]
        retention_days: i64,
        /// Report what would be deleted without touching disk
        /// or the index.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Remove any orphaned staging directories under
    /// `<data-root>/tmp/` left behind by a crash between
    /// assembling a submission and its atomic rename into place.
    SweepTmp,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,bris_collector=debug")),
        )
        .json()
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Reindex => {
            let config = Config::from_env().map_err(anyhow::Error::msg)?;
            let store = Store::open(&config.data_root)?;
            let stats = store.rebuild_index()?;
            tracing::info!(
                scanned = stats.scanned,
                indexed = stats.indexed,
                skipped = stats.skipped,
                "index rebuilt"
            );
            Ok(())
        }
        Command::SoftDelete { id } => {
            let config = Config::from_env().map_err(anyhow::Error::msg)?;
            let store = Store::open(&config.data_root)?;
            store.soft_delete(&id)?;
            tracing::info!(submission_id = %id, "submission soft-deleted");
            Ok(())
        }
        Command::RetentionSweep {
            retention_days,
            dry_run,
        } => {
            let config = Config::from_env().map_err(anyhow::Error::msg)?;
            let store = Store::open(&config.data_root)?;
            let stats = store.retention_sweep(retention_days, dry_run)?;
            tracing::info!(
                hard_deleted = stats.hard_deleted,
                retained = stats.retained,
                dry_run,
                retention_days,
                "retention sweep complete"
            );
            Ok(())
        }
        Command::SweepTmp => {
            let config = Config::from_env().map_err(anyhow::Error::msg)?;
            let store = Store::open(&config.data_root)?;
            let removed = store.sweep_tmp()?;
            tracing::info!(removed, "orphaned staging directories swept");
            Ok(())
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env().map_err(anyhow::Error::msg)?;
    if config.bearer_token.is_empty() {
        anyhow::bail!("BRIS_COLLECTOR_BEARER_TOKEN must be set; refusing to start without auth");
    }

    tracing::info!(
        data_root = %config.data_root.display(),
        bind = %config.bind,
        max_submission_bytes = config.max_submission_bytes,
        "bris-collector starting"
    );

    let store = Store::open(&config.data_root)?;
    let state = Arc::new(AppState {
        config: config.clone(),
        store,
    });
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            s.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutdown signal received");
}

//! `bris-collector` binary entrypoint.
//!
//! Reads configuration from an optional TOML config file
//! (`BRIS_COLLECTOR_CONFIG`) overlaid by `BRIS_COLLECTOR_*`
//! environment variables (see [`bris_collector::config`] for the
//! full surface and precedence), opens the store, binds the HTTP
//! server, and serves until SIGTERM / SIGINT. A reference config
//! file and k8s spec fragment ship under `deploy/reference/`.
//!
//! Also exposes operator-driven maintenance subcommands that
//! never run as a side effect of `serve` (the default, and the
//! command with no arguments):
//!
//! - `bris_collector index-rebuild` — rebuild the `SQLite` index
//!   mirror from the on-disk manifests (the index is a
//!   rebuildable cache, never the source of truth).
//! - `bris_collector soft-delete <id>` — mark a submission
//!   soft-deleted (sidecar marker on disk + index mirror). Files
//!   remain on disk.
//! - `bris_collector retention-sweep --retention-days <n>
//!   [--dry-run]` — hard-delete submissions soft-deleted longer
//!   than the retention window. Never run automatically; an
//!   explicit operator action only.
//!
//! All subcommands read `BRIS_COLLECTOR_DATA_ROOT` the same way
//! `serve` does.

use std::sync::Arc;

use bris_collector::routes::AppState;
use bris_collector::{build_app, store::Store, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,bris_collector=debug")),
        )
        .json()
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcommand = args.first().map_or("serve", String::as_str);

    match subcommand {
        "serve" => serve().await,
        "index-rebuild" => index_rebuild(),
        "soft-delete" => soft_delete(&args[1..]),
        "retention-sweep" => retention_sweep(&args[1..]),
        other => anyhow::bail!(
            "unknown subcommand {other:?}; expected one of: serve, index-rebuild, soft-delete, retention-sweep"
        ),
    }
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env().map_err(anyhow::Error::msg)?;
    if config.bearer_token.is_empty() {
        anyhow::bail!(
            "BRIS_COLLECTOR_BEARER_TOKEN (admin/bootstrap token) must be set; refusing to start without auth"
        );
    }

    tracing::info!(
        data_root = %config.data_root.display(),
        bind = %config.bind,
        max_submission_bytes = config.max_submission_bytes,
        retention_days = config.retention_days,
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

/// `index-rebuild` — operator-driven; never run on `serve`
/// startup.
fn index_rebuild() -> anyhow::Result<()> {
    let config = Config::from_env().map_err(anyhow::Error::msg)?;
    let store = Store::open(&config.data_root)?;
    let count = store.rebuild_index()?;
    tracing::info!(count, data_root = %config.data_root.display(), "index rebuilt");
    println!("rebuilt index: {count} submissions");
    Ok(())
}

/// `soft-delete <id>` — operator-driven; marks a submission
/// hidden from the default listing without touching its files.
fn soft_delete(args: &[String]) -> anyhow::Result<()> {
    let id = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: bris_collector soft-delete <id>"))?;
    let config = Config::from_env().map_err(anyhow::Error::msg)?;
    let store = Store::open(&config.data_root)?;
    store.soft_delete(id)?;
    println!("soft-deleted {id}");
    Ok(())
}

/// `retention-sweep --retention-days <n> [--dry-run]` —
/// operator-driven hard-delete of soft-deleted submissions past
/// the retention window. Never invoked automatically.
fn retention_sweep(args: &[String]) -> anyhow::Result<()> {
    let config = Config::from_env().map_err(anyhow::Error::msg)?;
    // Default to the configured retention window; an explicit
    // --retention-days on the command line still overrides it.
    let mut retention_days: i64 = config.retention_days;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--retention-days" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--retention-days requires a value"))?;
                retention_days = v
                    .parse()
                    .map_err(|e| anyhow::anyhow!("--retention-days: {e}"))?;
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            other => anyhow::bail!("unknown retention-sweep argument: {other}"),
        }
    }

    let store = Store::open(&config.data_root)?;
    let removed = store.retention_sweep(chrono::Duration::days(retention_days), dry_run)?;
    if dry_run {
        println!("would remove {} submissions: {:?}", removed.len(), removed);
    } else {
        println!("removed {} submissions: {:?}", removed.len(), removed);
    }
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

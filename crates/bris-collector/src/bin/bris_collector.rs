//! `bris-collector` binary entrypoint.
//!
//! Reads configuration from environment variables, opens the
//! store, binds the HTTP server, and serves until SIGTERM /
//! SIGINT.

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

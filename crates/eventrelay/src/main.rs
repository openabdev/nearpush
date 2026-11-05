use std::process::ExitCode;
use std::sync::Arc;

use eventrelay::config;
use eventrelay::http::{AppState, router};
use eventrelay::store::Store;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = match config::from_env(|key| std::env::var(key).ok()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("eventrelay: {e}");
            return ExitCode::FAILURE;
        }
    };
    let store = match Store::open(&cfg.db_path, cfg.retention) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            eprintln!("eventrelay: cannot open database: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(cfg.listen).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("eventrelay: cannot bind {}: {e}", cfg.listen);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(listen = %cfg.listen, "eventrelay listening");

    let app = router(AppState {
        store,
        auth: Arc::new(cfg.auth),
    });
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Err(e) = served {
        eprintln!("eventrelay: server error: {e}");
        return ExitCode::FAILURE;
    }
    tracing::info!("eventrelay stopped");
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGTERM handler");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutdown signal received");
}

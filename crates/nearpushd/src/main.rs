use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use nearpushd::config::{self, Config, Paths};
use nearpushd::server::{self, AppState};
use nearpushd::store::Store;
use tokio::signal::unix::{SignalKind, signal};

/// Broadcast capacity per subscriber (clamped to buffer_size); slower subscribers
/// resync from the buffer.
const LIVE_CAPACITY: usize = 256;
const DISCOVERY_RETRY: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(name = "nearpushd", version, about = "nearpush daemon")]
struct Args {
    /// Config directory (holds config.toml and token).
    #[arg(long, env = "NEARPUSH_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// State directory (holds events.jsonl).
    #[arg(long, env = "NEARPUSH_STATE_DIR")]
    state_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .init();
    if let Err(e) = run(Args::parse()).await {
        tracing::error!("{e}");
        eprintln!("nearpushd: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> std::io::Result<()> {
    let mut paths = Paths::from_env()?;
    if let Some(d) = args.config_dir {
        paths.config_dir = d;
    }
    if let Some(d) = args.state_dir {
        paths.state_dir = d;
    }
    let cfg = Config::load(&paths.config_file())?;
    let token = config::load_or_create_token(&paths.token_file())?;
    let store = Arc::new(Store::open(&paths.state_dir, cfg.buffer_size, LIVE_CAPACITY)?);
    tracing::info!(
        log = %store.path().display(),
        last_id = store.last_id(),
        version = nearpushd::assets::ui_version(),
        "event store loaded"
    );

    let sessions = nearpushd::sessions::Sessions::load(&paths.state_dir);
    if cfg.summarizer.enabled {
        tracing::info!(command = ?cfg.summarizer.command, "session summarizer enabled");
    } else {
        tracing::info!("session summarizer disabled");
    }
    let state = AppState::new(store, token, cfg.allowed_logins.clone(), cfg.allow_loopback_without_login)
        .with_sessions(sessions)
        .with_summarizer(cfg.summarizer.clone(), paths.state_dir.join("summarizer"))
        .with_icons_dir(paths.config_dir.join("icons"));
    if cfg.allowed_logins.is_empty() {
        // tailscaled may not be up yet at login, so keep trying in the background.
        let allowed = state.allowed_logins.clone();
        tokio::spawn(async move {
            loop {
                if let Some(login) = config::discover_tailnet_login().await {
                    tracing::info!(login, "allowed_logins defaulted to this node's tailnet login");
                    *allowed.write().unwrap_or_else(|p| p.into_inner()) = vec![login];
                    return;
                }
                tracing::warn!("could not read tailnet login from `tailscale status --json`; retrying");
                tokio::time::sleep(DISCOVERY_RETRY).await;
            }
        });
    } else {
        tracing::info!(logins = ?cfg.allowed_logins, "allowed logins from config");
    }

    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!(addr = %cfg.listen, "listening");

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        r = server::serve(listener, state) => r,
        _ = term.recv() => { tracing::info!("SIGTERM; exiting"); Ok(()) }
        _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT; exiting"); Ok(()) }
    }
}

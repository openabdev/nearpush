use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use nearpushd::config::{self, Config, Paths};
use nearpushd::control::Control;
use nearpushd::server::{self, AppState};
use nearpushd::store::{Store, ensure_private_dir};
use tokio::signal::unix::{SignalKind, signal};

/// Broadcast capacity per subscriber (clamped to buffer_size); slower subscribers
/// resync from the buffer.
const LIVE_CAPACITY: usize = 256;
const DISCOVERY_RETRY: Duration = Duration::from_secs(30);
/// `--log-file` is rotated to `<file>.1` at start once it passes this size.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "nearpushd", version, about = "nearpush daemon")]
struct Args {
    /// Config directory (holds config.toml and token).
    #[arg(long, env = "NEARPUSH_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// State directory (holds events.jsonl).
    #[arg(long, env = "NEARPUSH_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Append logs to this file (0600) instead of stderr. A leading `~/` is
    /// expanded from $HOME, so a static launchd plist can name it.
    #[arg(long, env = "NEARPUSH_LOG_FILE")]
    log_file: Option<PathBuf>,
}

/// Expands a leading `~/` from `$HOME`.
fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

/// Opens the log file for appending (0600, parent created), rotating it first
/// when it is over [`LOG_ROTATE_BYTES`].
fn open_log(path: &Path) -> std::io::Result<fs::File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        fs::rename(path, rotated)?;
    }
    let f = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(f)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let filter = || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match args.log_file.as_deref().map(expand_home) {
        Some(path) => match open_log(&path) {
            Ok(f) => tracing_subscriber::fmt().with_env_filter(filter()).with_ansi(false).with_writer(Mutex::new(f)).init(),
            Err(e) => {
                eprintln!("nearpushd: cannot open log file {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        None => tracing_subscriber::fmt().with_env_filter(filter()).with_ansi(false).init(),
    }
    if let Err(e) = run(args).await {
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
    let control = Control::load(&paths.state_dir);
    let pending = control.pending.clone();
    let (hidden_through, dismissing) = (control.hidden_through, control.dismissing.clone());
    let state = AppState::new(store, token, cfg.allowed_logins.clone(), cfg.allow_loopback_without_login)
        .with_sessions(sessions)
        .with_pending(pending)
        .with_dismissals(hidden_through, dismissing)
        .with_summarizer(cfg.summarizer.clone(), paths.state_dir.join("summarizer"))
        .with_classifier(cfg.classifier.clone(), paths.state_dir.join("classifier"))
        .with_icons_dir(paths.config_dir.join("icons"))
        .with_control(cfg.filter.states.clone(), cfg.classifier.provider, control, paths.state_dir.clone())
        .with_public_url(cfg.public_url.clone());
    if cfg.classifier.enabled {
        ensure_private_dir(&paths.state_dir.join("classifier"))?;
        tracing::info!(
            provider = cfg.classifier.provider.as_str(),
            model = cfg.classifier.provider.model(),
            "classifier enabled"
        );
        if let Some(rt) = state.classifier.clone() {
            let control = state.control.clone();
            rt.spawn_key_refresher(move || control.read().unwrap_or_else(|p| p.into_inner()).classifier);
        }
    } else {
        tracing::info!("classifier disabled");
    }
    {
        let c = state.control.read().unwrap_or_else(|p| p.into_inner());
        tracing::info!(paused = c.paused, states = ?c.states, source = ?c.source, "display controls");
    }
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
    if cfg.public_url.is_none() {
        let public_url = state.public_url.clone();
        tokio::spawn(async move {
            loop {
                if let Some(url) = config::discover_public_url().await {
                    tracing::info!(url, "public URL from tailscale");
                    *public_url.write().unwrap_or_else(|p| p.into_inner()) = Some(url);
                    return;
                }
                tokio::time::sleep(DISCOVERY_RETRY).await;
            }
        });
    }

    let listener = match tokio::net::TcpListener::bind(cfg.listen).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Exit 0 so launchd's KeepAlive {SuccessfulExit = false} does not
            // crash-loop against another daemon that already owns the port.
            tracing::error!(addr = %cfg.listen, "{} is already in use by another process; exiting", cfg.listen);
            eprintln!("nearpushd: {} is already in use by another process; exiting", cfg.listen);
            std::process::exit(0);
        }
        Err(e) => return Err(e),
    };
    tracing::info!(addr = %cfg.listen, "listening");
    // Relay puller: runs beside the listener and never delays startup or /push.
    let _ = nearpushd::relay::spawn_from_config(state.clone(), cfg.relay.clone(), cfg.relay.cursor_path(&paths.state_dir));

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        r = server::serve(listener, state) => r,
        _ = term.recv() => { tracing::info!("SIGTERM; exiting"); Ok(()) }
        _ = tokio::signal::ctrl_c() => { tracing::info!("SIGINT; exiting"); Ok(()) }
    }
}

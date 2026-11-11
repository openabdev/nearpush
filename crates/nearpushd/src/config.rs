//! Paths, `config.toml`, the push token and tailnet login discovery.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::classifier::ClassifierConfig;
use crate::event::AgentState;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8787";
pub const DEFAULT_BUFFER: usize = 500;
pub const MAX_BUFFER: usize = 10_000;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    /// `~/.config/nearpush` and `~/.local/state/nearpush`, overridable with
    /// `NEARPUSH_CONFIG_DIR` / `NEARPUSH_STATE_DIR`.
    pub fn from_env() -> io::Result<Paths> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("HOME is not set"))?;
        let config_dir = std::env::var_os("NEARPUSH_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/nearpush"));
        let state_dir = std::env::var_os("NEARPUSH_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state/nearpush"));
        Ok(Paths { config_dir, state_dir })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn token_file(&self) -> PathBuf {
        self.config_dir.join("token")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Must be a loopback address; tailscale serve forwards to it.
    pub listen: SocketAddr,
    /// `Tailscale-User-Login` values allowed to open the page and the socket.
    /// Empty means "the laptop's own tailnet login" (from `tailscale status --json`).
    pub allowed_logins: Vec<String>,
    /// Number of events kept in memory for replay.
    pub buffer_size: usize,
    /// Local testing only: let loopback requests without `Tailscale-User-Login`
    /// open the page and the socket.
    pub allow_loopback_without_login: bool,
    /// LLM command that turns a session's prompts into a short purpose label.
    pub summarizer: SummarizerConfig,
    /// Turn-end classifier: `your_turn` or `all_set` for each finished turn.
    pub classifier: ClassifierConfig,
    /// Which agent states the page shows (generic level events always show).
    pub filter: FilterConfig,
    /// Read by `nearpush hook`; accepted here so the shared file stays valid.
    pub hook: HookConfig,
    /// The page's public URL (shown by `/status`). Unset: derived from tailscale.
    pub public_url: Option<String>,
    /// Event relay puller (see `crates/eventrelay`).
    pub relay: RelayConfig,
}

/// Default display filter: `working` is off.
pub const DEFAULT_FILTER_STATES: &[AgentState] = &[AgentState::AllSet, AgentState::YourTurn, AgentState::Allow];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterConfig {
    pub states: Vec<AgentState>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig { states: DEFAULT_FILTER_STATES.to_vec() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookConfig {
    /// Let workflow-step / sub-agent sessions push too.
    pub include_subagents: bool,
}

/// Default summarizer: a non-interactive kiro-cli run on a cheap model with no tools trusted.
pub const DEFAULT_SUMMARIZER_COMMAND: &[&str] =
    &["kiro-cli", "chat", "--no-interactive", "--model", "claude-haiku-5.5", "--trust-tools="];
pub const DEFAULT_SUMMARIZER_TIMEOUT_SECS: u64 = 30;
/// Login-keychain generic password holding the summarizer's `KIRO_API_KEY`.
pub const DEFAULT_KEYCHAIN_SERVICE: &str = "nearpush-kiro-api-key";
pub const DEFAULT_SECURITY_PROGRAM: &str = "/usr/bin/security";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummarizerConfig {
    pub enabled: bool,
    /// argv; the prompt goes on stdin and the label is read from stdout.
    pub command: Vec<String>,
    pub timeout_secs: u64,
    /// Keychain service whose password is passed to each run as `KIRO_API_KEY`
    /// (only in the child's environment). Empty: no lookup.
    pub keychain_service: String,
    /// The `security` binary used for the lookup (tests point it at a fake).
    pub security_program: String,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        SummarizerConfig {
            enabled: true,
            command: DEFAULT_SUMMARIZER_COMMAND.iter().map(|s| s.to_string()).collect(),
            timeout_secs: DEFAULT_SUMMARIZER_TIMEOUT_SECS,
            keychain_service: DEFAULT_KEYCHAIN_SERVICE.to_string(),
            security_program: DEFAULT_SECURITY_PROGRAM.to_string(),
        }
    }
}

pub const DEFAULT_RELAY_KEYCHAIN_SERVICE: &str = "nearpush-eventrelay-token";
pub const DEFAULT_RELAY_WAIT_SECS: u64 = 25;
pub const MAX_RELAY_WAIT_SECS: u64 = 25;
/// Cursor file name inside the state directory.
pub const RELAY_CURSOR_FILE: &str = "relay-cursor.json";

/// `[relay]`: where the laptop pulls events from. The consumer token is never in
/// this file; it is read from the login keychain at start.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    pub enabled: bool,
    /// Base URL of the relay's consumer path, e.g. `https://<relay-host>.<tailnet>.ts.net:8443`.
    pub url: Option<String>,
    /// Keychain service holding the consumer token (`security find-generic-password -w`).
    pub consumer_token_keychain_service: String,
    /// Long-poll wait sent to the relay, 1..=25 seconds.
    pub wait_secs: u64,
    /// Cursor file. Default: `<state dir>/relay-cursor.json`. A leading `~/` is expanded.
    pub state_file: Option<PathBuf>,
    /// The `security` binary used for the keychain lookup (tests point it at a fake).
    pub security_program: String,
}

impl Default for RelayConfig {
    fn default() -> Self {
        RelayConfig {
            enabled: false,
            url: None,
            consumer_token_keychain_service: DEFAULT_RELAY_KEYCHAIN_SERVICE.to_string(),
            wait_secs: DEFAULT_RELAY_WAIT_SECS,
            state_file: None,
            security_program: DEFAULT_SECURITY_PROGRAM.to_string(),
        }
    }
}

impl RelayConfig {
    /// Where the cursor is stored: `state_file`, or `<state_dir>/relay-cursor.json`.
    pub fn cursor_path(&self, state_dir: &Path) -> PathBuf {
        match &self.state_file {
            Some(p) => expand_home(p),
            None => state_dir.join(RELAY_CURSOR_FILE),
        }
    }
}

/// Expands a leading `~/` from `$HOME`.
fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.parse().expect("valid default"),
            allowed_logins: Vec::new(),
            buffer_size: DEFAULT_BUFFER,
            allow_loopback_without_login: false,
            summarizer: SummarizerConfig::default(),
            classifier: ClassifierConfig::default(),
            filter: FilterConfig::default(),
            hook: HookConfig::default(),
            public_url: None,
            relay: RelayConfig::default(),
        }
    }
}

impl Config {
    /// Reads the config file; a missing file yields the defaults.
    pub fn load(path: &Path) -> io::Result<Config> {
        let cfg = match fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> io::Result<()> {
        if !self.listen.ip().is_loopback() {
            // /, /ws and /push all trust the loopback peer; a wider bind would let
            // LAN clients forge the Tailscale identity header.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("listen = {} is not a loopback address", self.listen),
            ));
        }
        if self.buffer_size == 0 || self.buffer_size > MAX_BUFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("buffer_size must be 1..={MAX_BUFFER}"),
            ));
        }
        if self.summarizer.enabled && self.summarizer.command.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "summarizer.command is empty"));
        }
        if self.summarizer.timeout_secs == 0 || self.summarizer.timeout_secs > 300 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "summarizer.timeout_secs must be 1..=300"));
        }
        self.classifier.validate()?;
        if let Some(u) = &self.public_url {
            let lower = u.to_ascii_lowercase();
            if !(lower.starts_with("https://") || lower.starts_with("http://")) || u.len() > 2048 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "public_url must start with http:// or https://"));
            }
        }
        if self.relay.wait_secs == 0 || self.relay.wait_secs > MAX_RELAY_WAIT_SECS {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "relay.wait_secs must be 1..=25"));
        }
        if self.relay.enabled {
            let Some(u) = &self.relay.url else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "relay.url is required when relay.enabled is true"));
            };
            let lower = u.to_ascii_lowercase();
            if !(lower.starts_with("https://") || lower.starts_with("http://")) || u.len() > 2048 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "relay.url must start with http:// or https://"));
            }
            if self.relay.consumer_token_keychain_service.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "relay.consumer_token_keychain_service is empty"));
            }
        }
        Ok(())
    }
}

/// Reads the push token, creating a random one (mode 0600) when the file is missing.
pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    match fs::read_to_string(path) {
        Ok(t) => {
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "token file is readable by others; chmod 600 it");
            }
            let t = t.trim().to_string();
            if t.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is empty", path.display())));
            }
            Ok(t)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut raw = [0u8; 32];
            getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
            let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
            f.write_all(token.as_bytes())?;
            f.write_all(b"\n")?;
            tracing::info!(path = %path.display(), "created push token");
            Ok(token)
        }
        Err(e) => Err(e),
    }
}

/// Where the tailscale CLI usually lives on macOS; launchd's PATH is minimal.
const TAILSCALE_CANDIDATES: &[&str] = &[
    "tailscale",
    "/usr/local/bin/tailscale",
    "/opt/homebrew/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

/// Extracts this node's owner login from `tailscale status --json` output.
pub fn login_from_status_json(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let uid = v.get("Self")?.get("UserID")?;
    let key = match uid {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => return None,
    };
    let login = v.get("User")?.get(&key)?.get("LoginName")?.as_str()?;
    (!login.is_empty()).then(|| login.to_string())
}

/// Asks the local tailscale CLI for this node's login. `None` if tailscale is
/// missing, not running or logged out.
pub async fn discover_tailnet_login() -> Option<String> {
    discover_from_status(login_from_status_json).await
}

/// This node's MagicDNS name (`Self.DNSName` without the trailing dot).
pub fn dns_name_from_status_json(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = v.get("Self")?.get("DNSName")?.as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

/// `https://<MagicDNS name>/`, the URL `tailscale serve` publishes on 443.
pub async fn discover_public_url() -> Option<String> {
    discover_from_status(dns_name_from_status_json).await.map(|n| format!("https://{n}/"))
}

async fn discover_from_status(extract: fn(&str) -> Option<String>) -> Option<String> {
    for bin in TAILSCALE_CANDIDATES {
        let out = tokio::process::Command::new(bin)
            .args(["status", "--json"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let Ok(Ok(out)) = tokio::time::timeout(std::time::Duration::from_secs(10), out).await else {
            continue;
        };
        if let Some(v) = extract(&String::from_utf8_lossy(&out.stdout)) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dns_name_from_status() {
        let json = r#"{"Self":{"DNSName":"laptop.example.ts.net."}}"#;
        assert_eq!(dns_name_from_status_json(json).as_deref(), Some("laptop.example.ts.net"));
        assert_eq!(dns_name_from_status_json(r#"{"Self":{"DNSName":""}}"#), None);
        assert_eq!(dns_name_from_status_json("not json"), None);
    }

    #[test]
    fn filter_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.filter.states, vec![AgentState::AllSet, AgentState::YourTurn, AgentState::Allow]);
        assert!(!cfg.filter.states.contains(&AgentState::Working));
        let cfg: Config = toml::from_str("[filter]\nstates = [\"working\", \"all_set\"]").unwrap();
        assert_eq!(cfg.filter.states, vec![AgentState::Working, AgentState::AllSet]);
        assert!(toml::from_str::<Config>("[filter]\nstates = [\"sleeping\"]").is_err());
        // Generic level events are not a state, so they cannot be listed (they always show).
        assert!(toml::from_str::<Config>("[filter]\nstates = [\"info\"]").is_err());
        assert!(toml::from_str::<Config>("[filter]\nstate = []").is_err());
    }

    #[test]
    fn hook_section_accepted() {
        let cfg: Config = toml::from_str("[hook]\ninclude_subagents = true").unwrap();
        assert!(cfg.hook.include_subagents);
        assert!(!Config::default().hook.include_subagents);
        assert!(toml::from_str::<Config>("[hook]\nbogus = 1").is_err());
    }

    #[test]
    fn public_url_validated() {
        let cfg: Config = toml::from_str("public_url = \"https://laptop.example.ts.net/\"").unwrap();
        assert!(cfg.validate().is_ok());
        let cfg: Config = toml::from_str("public_url = \"javascript:x\"").unwrap();
        assert!(cfg.validate().is_err());
        assert_eq!(Config::default().public_url, None);
    }

    #[test]
    fn parses_login_from_status() {
        let json = r#"{"Self":{"UserID":12345},"User":{"12345":{"LoginName":"you@example.com"}}}"#;
        assert_eq!(login_from_status_json(json).as_deref(), Some("you@example.com"));
        assert_eq!(login_from_status_json(r#"{"Self":{}}"#), None);
    }

    #[test]
    fn rejects_non_loopback_listen() {
        let cfg: Config = toml::from_str(r#"listen = "0.0.0.0:8787""#).unwrap();
        assert!(cfg.validate().is_err());
        let cfg: Config = toml::from_str(r#"listen = "127.0.0.1:9000""#).unwrap();
        assert!(cfg.validate().is_ok());
        assert!(toml::from_str::<Config>(r#"lisen = "x""#).is_err());
    }

    #[test]
    fn summarizer_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.summarizer.enabled);
        assert_eq!(cfg.summarizer.command[0], "kiro-cli");
        assert!(cfg.summarizer.command.iter().any(|a| a == "--trust-tools="));
        let cfg: Config = toml::from_str("[summarizer]\nenabled = false").unwrap();
        assert!(!cfg.summarizer.enabled);
        let cfg: Config = toml::from_str("[summarizer]\ncommand = [\"sh\", \"-c\", \"echo hi\"]\ntimeout_secs = 5").unwrap();
        assert_eq!(cfg.summarizer.command.len(), 3);
        assert!(cfg.validate().is_ok());
        assert!(toml::from_str::<Config>("[summarizer]\nbogus = 1").is_err());
        assert_eq!(Config::default().summarizer.keychain_service, "nearpush-kiro-api-key");
        let cfg: Config = toml::from_str("[summarizer]\nkeychain_service = \"\"").unwrap();
        assert!(cfg.summarizer.keychain_service.is_empty());
    }

    #[test]
    fn classifier_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.classifier.enabled);
        assert_eq!(cfg.classifier.provider, crate::classifier::Provider::Kiro);
        assert_eq!(cfg.classifier.threshold, 0.5);
        assert_eq!(cfg.classifier.timeout_secs, 30);
        assert!(cfg.validate().is_ok());
        let cfg: Config = toml::from_str("[classifier]\nprovider = \"typesafe\"").unwrap();
        assert_eq!(cfg.classifier.provider, crate::classifier::Provider::Typesafe);
        assert!(cfg.validate().is_ok());
        let e = toml::from_str::<Config>("[classifier]\nprovider = \"jev\"").unwrap_err().to_string();
        assert!(e.contains("kiro") && e.contains("openrouter") && e.contains("typesafe"), "{e}");
        assert!(toml::from_str::<Config>("[classifier]\nbogus = 1").is_err());
        // Test-only fields are not settable from the file.
        assert!(toml::from_str::<Config>("[classifier]\nsecurity_program = \"/x\"").is_err());
        let cfg: Config = toml::from_str("[classifier]\nthreshold = 1.5").unwrap();
        assert!(cfg.validate().is_err());
        let cfg: Config = toml::from_str("[classifier]\ntimeout_secs = 0").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn relay_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.relay.enabled);
        assert_eq!(cfg.relay.wait_secs, 25);
        assert_eq!(cfg.relay.consumer_token_keychain_service, "nearpush-eventrelay-token");
        assert_eq!(cfg.relay.security_program, "/usr/bin/security");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.relay.cursor_path(Path::new("/s")), PathBuf::from("/s/relay-cursor.json"));

        let cfg: Config = toml::from_str("[relay]\nenabled = true\nurl = \"https://relay-host.example.ts.net:8443\"").unwrap();
        assert!(cfg.validate().is_ok());
        let cfg: Config = toml::from_str("[relay]\nenabled = true").unwrap();
        assert!(cfg.validate().is_err(), "enabled without url");
        let cfg: Config = toml::from_str("[relay]\nenabled = true\nurl = \"javascript:x\"").unwrap();
        assert!(cfg.validate().is_err());
        for w in ["0", "26"] {
            let cfg: Config = toml::from_str(&format!("[relay]\nwait_secs = {w}")).unwrap();
            assert!(cfg.validate().is_err(), "wait_secs {w}");
        }
        assert!(toml::from_str::<Config>("[relay]\nbogus = 1").is_err());
        assert!(toml::from_str::<Config>("[relay]\ntoken = \"x\"").is_err(), "no token key in config");
        let cfg: Config = toml::from_str("[relay]\nstate_file = \"/tmp/c.json\"").unwrap();
        assert_eq!(cfg.relay.cursor_path(Path::new("/s")), PathBuf::from("/tmp/c.json"));
    }

    #[test]
    fn token_created_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/token");
        let a = load_or_create_token(&path).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(load_or_create_token(&path).unwrap(), a);
    }
}

//! Runtime controls set through `POST /control`, persisted in `<state>/control.json`.
//!
//! `paused`: events are stored (marked `muted`) but never broadcast or replayed.
//! `states`: display filter override; absent means `[filter] states` from config.toml.
//! The file is written atomically at 0600 so a restart (e.g. the menu bar app
//! toggling the summarizer) keeps the user's live choices.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::classifier::Provider;
use crate::event::AgentState;
use crate::main_card::{Dismissal, PendingCard};

pub const CONTROL_FILE: &str = "control.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Control {
    pub paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<AgentState>>,
    /// Classifier provider chosen through `POST /control`; absent keeps the config value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<Provider>,
    /// Sessions waiting on the user (see `main_card`); kept across restarts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<PendingCard>,
    /// Stored events up to this id are hidden from the main card (a dismissed card).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_through: Option<u64>,
    /// Dismissals still inside their undo window. A restart makes them final.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dismissing: Vec<Dismissal>,
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(CONTROL_FILE)
}

impl Control {
    /// A missing or unreadable file yields the defaults (not paused, config filter).
    pub fn load(state_dir: &Path) -> Control {
        match fs::read(path(state_dir)) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "ignoring unreadable control.json");
                Control::default()
            }),
            Err(_) => Control::default(),
        }
    }

    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        crate::store::ensure_private_dir(state_dir)?;
        let dest = path(state_dir);
        let tmp = dest.with_extension("json.tmp");
        {
            let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip_private() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Control::load(dir.path()), Control::default());
        let c = Control { paused: true, states: Some(vec![AgentState::Working]), ..Control::default() };
        c.save(dir.path()).unwrap();
        assert_eq!(Control::load(dir.path()), c);
        let mode = fs::metadata(path(dir.path())).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!dir.path().join("control.json.tmp").exists());
        fs::write(path(dir.path()), "garbage").unwrap();
        assert_eq!(Control::load(dir.path()), Control::default());
    }

    #[test]
    fn control_classifier_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let c = Control { classifier: Some(Provider::Typesafe), ..Control::default() };
        c.save(dir.path()).unwrap();
        assert_eq!(Control::load(dir.path()), c);
        // A control.json written before the classifier key existed loads with None.
        fs::write(path(dir.path()), r#"{"paused":true}"#).unwrap();
        let old = Control::load(dir.path());
        assert!(old.paused);
        assert_eq!(old.classifier, None);
    }
}

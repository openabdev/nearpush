//! The iPhone web page, embedded in the binary.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};

pub const INDEX_HTML: &str = include_str!("../assets/index.html");
pub const MANIFEST: &str = include_str!("../assets/manifest.webmanifest");
pub const ICON_180: &[u8] = include_bytes!("../assets/icon-180.png");
pub const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
pub const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");

/// Placeholder in `index.html` replaced with [`ui_version`] when served.
pub const VERSION_PLACEHOLDER: &str = "__NEARPUSH_VERSION__";

/// Crate version plus a hash of every embedded asset. The page reloads itself
/// when the hello frame carries a different value.
pub fn ui_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h = Sha256::new();
        for part in [INDEX_HTML.as_bytes(), MANIFEST.as_bytes(), ICON_180, ICON_192, ICON_512] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part);
        }
        let digest = h.finalize();
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        format!("{}+{hex}", env!("CARGO_PKG_VERSION"))
    })
}

pub fn index_html() -> String {
    INDEX_HTML.replace(VERSION_PLACEHOLDER, ui_version())
}

//! The iPhone web page, embedded in the binary.

use std::borrow::Cow;
use std::path::Path;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

pub const INDEX_HTML: &str = include_str!("../assets/index.html");
pub const MANIFEST: &str = include_str!("../assets/manifest.webmanifest");
pub const ICON_180: &[u8] = include_bytes!("../assets/icon-180.png");
pub const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
pub const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");
/// Official Kiro logo, from https://kiro.dev/icon.svg.
pub const KIRO_SVG: &[u8] = include_bytes!("../assets/icons/kiro.svg");
/// auth-proxy sender mark (a shield with a keyhole), drawn for this repo.
pub const AUTH_PROXY_SVG: &[u8] = include_bytes!("../assets/icons/auth-proxy.svg");

/// Built-in sender icons, used when no user override exists.
const BUILTIN_ICONS: &[(&str, &[u8])] = &[("kiro", KIRO_SVG), ("auth-proxy", AUTH_PROXY_SVG)];

pub const SVG: &str = "image/svg+xml";
pub const PNG: &str = "image/png";

/// Where an icon came from (for tests and logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconSource {
    Override,
    Builtin,
    Generic,
}

pub struct Icon {
    pub bytes: Cow<'static, [u8]>,
    pub content_type: &'static str,
    pub source: IconSource,
}

/// Resolves the icon for `sender`: `<override_dir>/<sender>.svg`, then `.png`, then
/// a built-in icon, then a generic letter tile. `None` when `sender` is not a valid
/// sender name, which is what keeps the file lookup from leaving `override_dir`.
pub fn resolve_icon(override_dir: Option<&Path>, sender: &str) -> Option<Icon> {
    if !crate::event::valid_sender(sender) {
        return None;
    }
    if let Some(dir) = override_dir {
        for (ext, content_type) in [("svg", SVG), ("png", PNG)] {
            let path = dir.join(format!("{sender}.{ext}"));
            // Only regular files; a symlink pointing elsewhere is still the user's choice
            // inside their own config dir, so metadata follows it.
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.is_file() && meta.len() <= 512 * 1024 {
                    if let Ok(bytes) = std::fs::read(&path) {
                        return Some(Icon { bytes: Cow::Owned(bytes), content_type, source: IconSource::Override });
                    }
                }
            }
        }
    }
    if let Some((_, bytes)) = BUILTIN_ICONS.iter().find(|(name, _)| *name == sender) {
        return Some(Icon { bytes: Cow::Borrowed(bytes), content_type: SVG, source: IconSource::Builtin });
    }
    Some(Icon { bytes: Cow::Owned(generic_icon(sender).into_bytes()), content_type: SVG, source: IconSource::Generic })
}

/// The sender's first letter in a rounded square, coloured from a hash of the name.
pub fn generic_icon(sender: &str) -> String {
    let letter = sender.chars().next().unwrap_or('?').to_ascii_uppercase();
    let hue = sender.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    // `sender` is validated to [a-z0-9-], so `letter` needs no escaping.
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\">\
<rect width=\"100\" height=\"100\" rx=\"22\" fill=\"hsl({hue},45%,42%)\"/>\
<text x=\"50\" y=\"69\" text-anchor=\"middle\" font-family=\"-apple-system,Helvetica,Arial,sans-serif\" \
font-size=\"58\" font-weight=\"700\" fill=\"#fff\">{letter}</text></svg>"
    )
}

/// Placeholder in `index.html` replaced with [`ui_version`] when served.
pub const VERSION_PLACEHOLDER: &str = "__NEARPUSH_VERSION__";

/// Crate version plus a hash of every embedded asset. The page reloads itself
/// when the hello frame carries a different value.
pub fn ui_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h = Sha256::new();
        for part in [INDEX_HTML.as_bytes(), MANIFEST.as_bytes(), ICON_180, ICON_192, ICON_512, KIRO_SVG] {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_resolution_order() {
        let dir = tempfile::tempdir().unwrap();
        let d = Some(dir.path());
        // Built-in, then generic.
        assert_eq!(resolve_icon(d, "kiro").unwrap().source, IconSource::Builtin);
        assert_eq!(resolve_icon(d, "auth-proxy").unwrap().source, IconSource::Builtin);
        let g = resolve_icon(d, "cli").unwrap();
        assert_eq!((g.source, g.content_type), (IconSource::Generic, SVG));
        assert!(std::str::from_utf8(&g.bytes).unwrap().contains(">C</text>"));
        // A png override beats the built-in; an svg override beats the png.
        std::fs::write(dir.path().join("kiro.png"), b"png").unwrap();
        let i = resolve_icon(d, "kiro").unwrap();
        assert_eq!((i.source, i.content_type, &*i.bytes), (IconSource::Override, PNG, &b"png"[..]));
        std::fs::write(dir.path().join("kiro.svg"), b"<svg/>").unwrap();
        let i = resolve_icon(d, "kiro").unwrap();
        assert_eq!((i.source, i.content_type), (IconSource::Override, SVG));
        // No override dir configured.
        assert_eq!(resolve_icon(None, "kiro").unwrap().source, IconSource::Builtin);
    }

    #[test]
    fn icon_names_cannot_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let icons = dir.path().join("icons");
        std::fs::create_dir(&icons).unwrap();
        std::fs::write(dir.path().join("secret.svg"), b"<svg>secret</svg>").unwrap();
        for bad in ["../secret", "..", "a/b", "kiro.svg", "", "KIRO", "%2e%2e"] {
            assert!(resolve_icon(Some(&icons), bad).is_none(), "{bad}");
        }
    }

    /// Font size in px of the first `clamp(min, vmin, max)` after `selector`, at a portrait viewport `width` px wide.
    fn clamp_px(selector: &str, width: f64) -> f64 {
        let rule = &INDEX_HTML[INDEX_HTML.find(selector).expect(selector)..];
        let args = &rule[rule.find("clamp(").unwrap() + "clamp(".len()..];
        let args: Vec<&str> = args[..args.find(')').unwrap()].split(',').map(str::trim).collect();
        let px = |s: &str| s.trim_end_matches("px").parse::<f64>().unwrap();
        let vmin = args[1].trim_end_matches("vmin").parse::<f64>().unwrap() * width / 100.0;
        px(args[0]).max(vmin.min(px(args[2])))
    }

    #[test]
    fn your_turn_question_line_is_larger_than_secondary_text() {
        assert!(INDEX_HTML.contains(r#"id="detail""#));
        assert!(INDEX_HTML.contains("card.classList.toggle('your_turn'"));
        assert!(INDEX_HTML.contains("-webkit-line-clamp: 3;"));
        for width in [375.0, 430.0] {
            let question = clamp_px("#card.your_turn #detail {", width);
            let secondary = clamp_px("#proj {", width);
            assert!(question > secondary, "{width}px: {question} <= {secondary}");
            assert!((question / secondary - 1.25f64.powi(3)).abs() < 0.05, "{width}px: {question} / {secondary}");
        }
    }

    #[test]
    fn card_text_order_is_headline_question_repo_then_small_text() {
        let body = &INDEX_HTML[INDEX_HTML.find("<body>").unwrap()..];
        let at = |id: &str| body.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("missing #{id}"));
        let order = ["headline", "detail", "proj", "label", "task", "sub", "foot", "meta", "missed", "when"];
        for pair in order.windows(2) {
            assert!(at(pair[0]) < at(pair[1]), "#{} must come before #{}", pair[0], pair[1]);
        }
    }
}

//! Omarchy theme integration: read `~/.config/omarchy/current/theme/colors.toml`
//! and turn it into GTK `@define-color` definitions the static stylesheet
//! references. Falls back to a built-in Tokyo Night palette when the theme
//! has no colors file.

use std::path::PathBuf;

/// Built-in fallback palette (Tokyo Night).
const FALLBACK: Palette = Palette {
    background: "#1a1b26",
    foreground: "#a9b1d6",
    accent: "#7aa2f7",
    danger: "#f7768e",
    viewing: "#9ece6a",
    open: "#e0af68",
};

struct Palette {
    background: &'static str,
    foreground: &'static str,
    accent: &'static str,
    danger: &'static str,
    /// Ring for the project workspace currently on screen (ANSI green).
    viewing: &'static str,
    /// Ring for a project with windows that is not on screen (ANSI yellow).
    open: &'static str,
}

/// The `~/.config/omarchy/current` directory (watched for theme switches).
pub fn omarchy_current_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/omarchy/current"))
}

fn colors_file() -> Option<PathBuf> {
    Some(omarchy_current_dir()?.join("theme/colors.toml"))
}

/// Generate the `@define-color` CSS for the current theme.
pub fn color_definitions() -> String {
    let table = colors_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.parse::<toml::Table>().ok());
    let get = |key: &str, fallback: &str| -> String {
        table
            .as_ref()
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .filter(|s| is_hex_color(s))
            .unwrap_or(fallback)
            .to_owned()
    };
    let background = get("background", FALLBACK.background);
    let foreground = get("foreground", FALLBACK.foreground);
    let accent = get("accent", FALLBACK.accent);
    let danger = get("color1", FALLBACK.danger);
    let viewing = get("color2", FALLBACK.viewing);
    let open = get("color3", FALLBACK.open);
    format!(
        "@define-color panel_bg {background};\n\
         @define-color panel_fg {foreground};\n\
         @define-color panel_accent {accent};\n\
         @define-color panel_danger {danger};\n\
         @define-color panel_viewing {viewing};\n\
         @define-color panel_open {open};\n"
    )
}

fn is_hex_color(s: &str) -> bool {
    let Some(hex) = s.strip_prefix('#') else {
        return false;
    };
    (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_definitions_are_valid_css() {
        // Even with no theme present the definitions must contain all names.
        let css = color_definitions();
        for name in [
            "panel_bg",
            "panel_fg",
            "panel_accent",
            "panel_danger",
            "panel_viewing",
            "panel_open",
        ] {
            assert!(css.contains(name), "{css}");
        }
    }

    #[test]
    fn hex_validation() {
        assert!(is_hex_color("#7aa2f7"));
        assert!(is_hex_color("#7aa2f7ff"));
        assert!(!is_hex_color("7aa2f7"));
        assert!(!is_hex_color("#zzz"));
        assert!(!is_hex_color("#7aa2f7; } * { background: red"));
    }
}

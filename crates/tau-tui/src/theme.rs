//! Theme system for the TUI.
//!
//! Modeled after pi's theme architecture: a Theme struct with named foreground
//! and background colors, loaded from a JSON file or built-in defaults.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use std::collections::HashMap;

/// A resolved color value.
#[derive(Debug, Clone, Copy)]
pub enum ThemeColor {
    /// RGB true color.
    Rgb(u8, u8, u8),
    /// Use terminal default.
    Default,
}

impl ThemeColor {
    /// Convert to a ratatui Color.
    pub fn to_ratatui(self) -> Color {
        match self {
            ThemeColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
            ThemeColor::Default => Color::Reset,
        }
    }
}

/// Parse a color value: hex "#rrggbb", or empty string for default.
fn parse_color(value: &str, vars: &HashMap<String, String>) -> ThemeColor {
    let resolved = resolve_var(value, vars);
    if resolved.is_empty() {
        return ThemeColor::Default;
    }
    if let Some(hex) = resolved.strip_prefix('#')
        && hex.len() == 6
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        )
    {
        return ThemeColor::Rgb(r, g, b);
    }
    ThemeColor::Default
}

/// Resolve variable references (e.g. "accent" → vars["accent"]).
fn resolve_var(value: &str, vars: &HashMap<String, String>) -> String {
    if value.is_empty() || value.starts_with('#') {
        return value.to_string();
    }
    // It's a variable reference
    if let Some(resolved) = vars.get(value) {
        resolve_var(resolved, vars)
    } else {
        value.to_string()
    }
}

/// The full theme with all named colors.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Theme name (for display / identification).
    pub name: Option<String>,

    // Core UI
    pub accent: ThemeColor,
    pub border: ThemeColor,
    pub border_accent: ThemeColor,
    pub border_muted: ThemeColor,
    pub success: ThemeColor,
    pub error: ThemeColor,
    pub warning: ThemeColor,
    pub muted: ThemeColor,
    pub dim: ThemeColor,
    pub text: ThemeColor,
    pub thinking_text: ThemeColor,

    // Backgrounds
    pub selected_bg: ThemeColor,
    pub user_message_bg: ThemeColor,
    pub user_message_text: ThemeColor,
    pub tool_pending_bg: ThemeColor,
    pub tool_success_bg: ThemeColor,
    pub tool_error_bg: ThemeColor,
    pub tool_title: ThemeColor,
    pub tool_output: ThemeColor,
}

impl Theme {
    /// Foreground style for a named color.
    pub fn fg(&self, color: ThemeColor) -> Style {
        Style::default().fg(color.to_ratatui())
    }

    /// Background style for a named color.
    pub fn bg(&self, color: ThemeColor) -> Style {
        Style::default().bg(color.to_ratatui())
    }

    /// Combined fg + bg style.
    pub fn style(&self, fg: ThemeColor, bg: ThemeColor) -> Style {
        Style::default().fg(fg.to_ratatui()).bg(bg.to_ratatui())
    }

    /// Bold + fg style.
    pub fn bold_fg(&self, color: ThemeColor) -> Style {
        Style::default()
            .fg(color.to_ratatui())
            .add_modifier(Modifier::BOLD)
    }

    /// Italic + fg style.
    pub fn italic_fg(&self, color: ThemeColor) -> Style {
        Style::default()
            .fg(color.to_ratatui())
            .add_modifier(Modifier::ITALIC)
    }

    // --- Convenience accessors for common UI elements ---

    /// Tool success: toolSuccessBg background only (text colors set per-span).
    pub fn tool_success_style(&self) -> Style {
        Style::default().bg(self.tool_success_bg.to_ratatui())
    }

    /// Tool error: toolErrorBg background only (text colors set per-span).
    pub fn tool_error_style(&self) -> Style {
        Style::default().bg(self.tool_error_bg.to_ratatui())
    }

    /// Tool pending: toolPendingBg background only (text colors set per-span).
    pub fn tool_pending_style(&self) -> Style {
        Style::default().bg(self.tool_pending_bg.to_ratatui())
    }

    /// Error message style.
    pub fn error_style(&self) -> Style {
        Style::default()
            .fg(self.error.to_ratatui())
            .bg(self.tool_error_bg.to_ratatui())
    }

    pub fn status_style(&self) -> Style {
        Style::default().fg(self.dim.to_ratatui())
    }

    /// Input border (matches pi: borderMuted always).
    pub fn input_border_style(&self) -> Style {
        Style::default().fg(self.border_muted.to_ratatui())
    }

    /// Spinner character style (accent color, like pi).
    pub fn spinner_style(&self) -> Style {
        Style::default().fg(self.accent.to_ratatui())
    }

    /// Spinner message text style (muted color, like pi).
    pub fn spinner_message_style(&self) -> Style {
        Style::default().fg(self.muted.to_ratatui())
    }

    pub fn scrollbar_style(&self) -> Style {
        Style::default().fg(self.dim.to_ratatui())
    }

    pub fn context_color(&self, pct: f64) -> ThemeColor {
        if pct > 90.0 {
            self.error
        } else if pct > 70.0 {
            self.warning
        } else {
            self.dim
        }
    }
}

/// Built-in dark theme (matching pi's dark.json).
pub fn dark() -> Theme {
    Theme {
        name: Some("dark".into()),
        accent: ThemeColor::Rgb(0x8a, 0xbe, 0xb7),
        border: ThemeColor::Rgb(0x5f, 0x87, 0xff),
        border_accent: ThemeColor::Rgb(0x00, 0xd7, 0xff),
        border_muted: ThemeColor::Rgb(0x50, 0x50, 0x50),
        success: ThemeColor::Rgb(0xb5, 0xbd, 0x68),
        error: ThemeColor::Rgb(0xcc, 0x66, 0x66),
        warning: ThemeColor::Rgb(0xff, 0xff, 0x00),
        muted: ThemeColor::Rgb(0x80, 0x80, 0x80),
        dim: ThemeColor::Rgb(0x66, 0x66, 0x66),
        text: ThemeColor::Default,
        thinking_text: ThemeColor::Rgb(0x80, 0x80, 0x80),

        selected_bg: ThemeColor::Rgb(0x3a, 0x3a, 0x4a),
        user_message_bg: ThemeColor::Rgb(0x34, 0x35, 0x41),
        user_message_text: ThemeColor::Default,
        tool_pending_bg: ThemeColor::Rgb(0x28, 0x28, 0x32),
        tool_success_bg: ThemeColor::Rgb(0x28, 0x32, 0x28),
        tool_error_bg: ThemeColor::Rgb(0x3c, 0x28, 0x28),
        tool_title: ThemeColor::Default,
        tool_output: ThemeColor::Rgb(0x80, 0x80, 0x80),
    }
}

// ---------------------------------------------------------------------------
// Built-in themes (embedded at compile time)
// ---------------------------------------------------------------------------

const BUILTIN_DARK: &str = include_str!("../../../themes/dark.json");
const BUILTIN_LIGHT: &str = include_str!("../../../themes/light.json");

/// Names of built-in themes.
const BUILTIN_NAMES: &[&str] = &["dark", "light"];

/// Custom themes directory: `~/.config/tau/themes/`.
fn custom_themes_dir() -> Option<std::path::PathBuf> {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        Some(std::path::PathBuf::from(config).join("tau").join("themes"))
    } else if let Ok(home) = std::env::var("HOME") {
        Some(
            std::path::PathBuf::from(home)
                .join(".config")
                .join("tau")
                .join("themes"),
        )
    } else {
        None
    }
}

/// List all available theme names (built-in + custom).
pub fn list_themes() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_NAMES.iter().map(|s| (*s).to_string()).collect();

    if let Some(dir) = custom_themes_dir()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                let name = stem.to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }

    names.sort();
    names
}

/// Load a theme by name. Checks built-in first, then custom themes dir.
pub fn load_by_name(name: &str) -> Result<Theme, String> {
    // Built-in
    match name {
        "dark" => return from_json(BUILTIN_DARK),
        "light" => return from_json(BUILTIN_LIGHT),
        _ => {}
    }

    // Custom
    if let Some(dir) = custom_themes_dir() {
        let path = dir.join(format!("{}.json", name));
        if path.exists() {
            let json = std::fs::read_to_string(&path)
                .map_err(|e| format!("read {}: {}", path.display(), e))?;
            return from_json(&json);
        }
    }

    Err(format!("theme '{}' not found", name))
}

/// Load a theme from a file path.
pub fn load_from_path(path: &std::path::Path) -> Result<Theme, String> {
    let json =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    from_json(&json)
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

/// JSON schema for loading themes from file.
#[derive(Debug, Deserialize)]
struct ThemeJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    vars: HashMap<String, String>,
    colors: ThemeColorsJson,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ThemeColorsJson {
    accent: String,
    border: String,
    border_accent: String,
    border_muted: String,
    success: String,
    error: String,
    warning: String,
    muted: String,
    dim: String,
    text: String,
    thinking_text: String,

    selected_bg: String,
    user_message_bg: String,
    user_message_text: String,
    tool_pending_bg: String,
    tool_success_bg: String,
    tool_error_bg: String,
    tool_title: String,
    tool_output: String,
}

/// Load a theme from a JSON string (pi-compatible format).
pub fn from_json(json: &str) -> Result<Theme, String> {
    let theme_json: ThemeJson =
        serde_json::from_str(json).map_err(|e| format!("invalid theme JSON: {}", e))?;
    let vars = &theme_json.vars;
    let c = &theme_json.colors;

    Ok(Theme {
        name: theme_json.name,
        accent: parse_color(&c.accent, vars),
        border: parse_color(&c.border, vars),
        border_accent: parse_color(&c.border_accent, vars),
        border_muted: parse_color(&c.border_muted, vars),
        success: parse_color(&c.success, vars),
        error: parse_color(&c.error, vars),
        warning: parse_color(&c.warning, vars),
        muted: parse_color(&c.muted, vars),
        dim: parse_color(&c.dim, vars),
        text: parse_color(&c.text, vars),
        thinking_text: parse_color(&c.thinking_text, vars),

        selected_bg: parse_color(&c.selected_bg, vars),
        user_message_bg: parse_color(&c.user_message_bg, vars),
        user_message_text: parse_color(&c.user_message_text, vars),
        tool_pending_bg: parse_color(&c.tool_pending_bg, vars),
        tool_success_bg: parse_color(&c.tool_success_bg, vars),
        tool_error_bg: parse_color(&c.tool_error_bg, vars),
        tool_title: parse_color(&c.tool_title, vars),
        tool_output: parse_color(&c.tool_output, vars),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_color() {
        let vars = HashMap::new();
        let c = parse_color("#ff8800", &vars);
        assert!(matches!(c, ThemeColor::Rgb(0xff, 0x88, 0x00)));
    }

    #[test]
    fn parse_empty_is_default() {
        let vars = HashMap::new();
        let c = parse_color("", &vars);
        assert!(matches!(c, ThemeColor::Default));
    }

    #[test]
    fn resolve_variable() {
        let mut vars = HashMap::new();
        vars.insert("accent".into(), "#8abeb7".into());
        let c = parse_color("accent", &vars);
        assert!(matches!(c, ThemeColor::Rgb(0x8a, 0xbe, 0xb7)));
    }

    #[test]
    fn resolve_chained_variable() {
        let mut vars = HashMap::new();
        vars.insert("mycolor".into(), "accent".into());
        vars.insert("accent".into(), "#112233".into());
        let c = parse_color("mycolor", &vars);
        assert!(matches!(c, ThemeColor::Rgb(0x11, 0x22, 0x33)));
    }

    #[test]
    fn dark_theme_has_values() {
        let t = dark();
        assert!(matches!(t.accent, ThemeColor::Rgb(0x8a, 0xbe, 0xb7)));
        assert!(matches!(t.error, ThemeColor::Rgb(0xcc, 0x66, 0x66)));
        assert_eq!(t.name.as_deref(), Some("dark"));
    }

    #[test]
    fn load_builtin_dark() {
        let t = load_by_name("dark").unwrap();
        assert_eq!(t.name.as_deref(), Some("dark"));
        assert!(matches!(t.accent, ThemeColor::Rgb(0x8a, 0xbe, 0xb7)));
    }

    #[test]
    fn load_builtin_light() {
        let t = load_by_name("light").unwrap();
        assert_eq!(t.name.as_deref(), Some("light"));
        // light theme uses teal=#5a8080 for accent
        assert!(matches!(t.accent, ThemeColor::Rgb(0x5a, 0x80, 0x80)));
    }

    #[test]
    fn load_unknown_fails() {
        assert!(load_by_name("nonexistent").is_err());
    }

    #[test]
    fn list_includes_builtins() {
        let themes = list_themes();
        assert!(themes.contains(&"dark".to_string()));
        assert!(themes.contains(&"light".to_string()));
    }
}

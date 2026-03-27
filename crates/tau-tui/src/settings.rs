//! Local TUI settings persisted to `~/.config/tau/settings.toml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub tui: TuiSettings,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TuiSettings {
    /// Active theme name (e.g. "dark", "light").
    #[serde(default)]
    pub theme: Option<String>,
}

fn settings_path() -> PathBuf {
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("tau").join("settings.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("tau")
            .join("settings.toml")
    } else {
        PathBuf::from("/tmp").join("tau-settings.toml")
    }
}

pub fn load() -> Settings {
    let path = settings_path();
    if !path.exists() {
        return Settings::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| toml::from_str(&content).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(content) = toml::to_string_pretty(settings) {
        std::fs::write(&path, content).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings() {
        let s = Settings::default();
        assert!(s.tui.theme.is_none());
    }

    #[test]
    fn roundtrip_toml() {
        let s = Settings {
            tui: TuiSettings {
                theme: Some("light".into()),
            },
        };
        let toml_str = toml::to_string_pretty(&s).unwrap();
        let parsed: Settings = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.tui.theme.as_deref(), Some("light"));
    }
}

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub volume: f32,
    pub profile: String,
    pub up_sound: bool,
    pub exclusive: bool,
    /// Let exclusive mode step aside for a video by itself.
    pub auto_release: bool,
    /// Ask GitHub once at startup whether there is a newer release.
    pub check_updates: bool,
    /// `en` or `fa`; decides the interface, the tray menu and the window title.
    pub lang: String,
}

/// Anything that is not a language this app ships for falls back to the default
/// rather than to whatever was in the file, so a hand-edited config cannot
/// leave the interface with no strings at all.
pub fn lang(value: &str) -> &'static str {
    match value {
        "fa" => "fa",
        _ => "en",
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.7,
            profile: "mx-blue".into(),
            up_sound: true,
            exclusive: false,
            // Both are off until asked for: one gives up the latency the app is
            // built around, the other is the only thing that touches the network.
            auto_release: false,
            check_updates: true,
            lang: "en".into(),
        }
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("MechKeys")
}

pub fn path() -> PathBuf {
    dir().join("config.json")
}

/// Where a downloaded installer goes: the folder the user looks in for files
/// they asked for, wherever that is pointed these days.
pub fn downloads() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::desktop_dir)
        .unwrap_or_else(dir)
}

pub fn load() -> Config {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) {
    let _ = std::fs::create_dir_all(dir());
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path(), json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_starts_in_english() {
        assert_eq!(Config::default().lang, "en");
    }

    #[test]
    fn only_the_languages_that_ship_are_accepted() {
        assert_eq!(lang("fa"), "fa");
        assert_eq!(lang("en"), "en");
        // A config written by hand, or by an older build that never had the key.
        assert_eq!(lang("de"), "en");
        assert_eq!(lang(""), "en");
        assert_eq!(lang("FA"), "en");
    }

    #[test]
    fn a_config_from_before_the_language_key_still_loads() {
        let old = r#"{ "enabled": true, "volume": 0.4, "profile": "topre" }"#;
        let cfg: Config = serde_json::from_str(old).unwrap();
        assert_eq!(cfg.lang, "en");
        assert_eq!(cfg.volume, 0.4);
        assert!(!cfg.exclusive);
    }
}

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub niri: NiriConfig,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub url: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NiriConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[allow(dead_code)]
    #[serde(default = "default_true")]
    pub workspace_notifications: bool,
}

impl Default for NiriConfig {
    fn default() -> Self {
        Self { enabled: true, workspace_notifications: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_cache_path")]
    pub path: PathBuf,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { path: default_cache_path() }
    }
}

fn default_true() -> bool {
    true
}

fn default_cache_path() -> PathBuf {
    dirs_cache_dir().join("riptune").join("library.sqlite")
}

fn dirs_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".cache")
        })
}

impl Config {
    pub fn load(explicit_path: Option<PathBuf>) -> Result<Self> {
        let path = explicit_path.unwrap_or_else(default_config_path);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        Ok(cfg)
    }
}

fn default_config_path() -> PathBuf {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        });
    xdg.join("riptune").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_file_is_valid_toml_shape() {
        // config.example.toml is meant to be copied to
        // ~/.config/riptune/config.toml and edited with real credentials, so
        // this only checks the file parses and required fields are present —
        // it must never assert exact values, since those are expected to be
        // customized locally (and shouldn't leak into this test's output).
        let raw = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(raw).expect("example config should parse");
        assert!(!cfg.server.url.is_empty());
        assert!(!cfg.server.username.is_empty());
    }

    #[test]
    fn parses_a_minimal_known_config() {
        // Unlike the test above, this uses a literal inline string so the
        // expected values can never drift out from under the assertions.
        let raw = r#"
            [server]
            url = "https://music.example.com"
            username = "alice"
            password = "secret"
        "#;
        let cfg: Config = toml::from_str(raw).expect("minimal config should parse");
        assert_eq!(cfg.server.url, "https://music.example.com");
        assert!(cfg.niri.enabled);
    }

    #[test]
    fn niri_and_cache_sections_are_optional() {
        let raw = r#"
            [server]
            url = "https://music.example.com"
            username = "alice"
            password = "secret"
        "#;
        let cfg: Config = toml::from_str(raw).expect("minimal config should parse");
        assert!(cfg.niri.enabled, "niri.enabled should default to true");
    }
}

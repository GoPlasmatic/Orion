use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct OrionConfig {
    pub server_url: Option<String>,
    #[serde(default = "default_output")]
    pub default_output: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_header: Option<String>,
}

fn default_output() -> String {
    "table".to_string()
}

impl OrionConfig {
    pub fn path() -> Result<PathBuf> {
        let config_dir = dirs::home_dir()
            .context("Could not determine home directory")?
            .join(".orion");
        Ok(config_dir.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config from {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse config from {}", path.display()))
    }

    /// Resolve server URL: env var > config file
    pub fn resolve_server_url() -> Result<String> {
        if let Ok(url) = std::env::var("ORION_SERVER_URL") {
            if !url.is_empty() {
                return Ok(url);
            }
        }
        let config = Self::load()?;
        config.server_url.ok_or_else(|| {
            anyhow::anyhow!(
                "No server URL configured. Set ORION_SERVER_URL environment variable or configure server_url in ~/.orion/config.toml"
            )
        })
    }

    /// Resolve API key: env var > config file. Returns (key, optional header).
    pub fn resolve_api_key() -> Option<(String, Option<String>)> {
        if let Ok(key) = std::env::var("ORION_API_KEY") {
            if !key.is_empty() {
                let header = std::env::var("ORION_API_KEY_HEADER").ok();
                return Some((key, header));
            }
        }
        let config = Self::load().ok()?;
        config.api_key.map(|k| (k, config.api_key_header))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory {}", parent.display())
            })?;
        }
        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write config to {}", path.display()))
    }
}

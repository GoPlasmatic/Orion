use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CliConfig {
    pub server_url: Option<String>,
    #[serde(default = "default_output")]
    pub default_output: String,
}

fn default_output() -> String {
    "table".to_string()
}

impl CliConfig {
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

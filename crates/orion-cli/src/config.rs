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

    /// The API key from `~/.orion/config.toml`, with its header.
    ///
    /// Only the config file: `--api-key` and `--api-key-header` carry
    /// `env = "ORION_API_KEY"` / `env = "ORION_API_KEY_HEADER"`, so clap has
    /// already applied those by the time the caller falls through to here —
    /// the same reason the server URL needs no env twin. Reading them again
    /// here was a second, unreachable precedence rule for one credential.
    pub fn resolve_api_key() -> Option<(String, Option<String>)> {
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
        // The file can hold the API key, so create it owner-only (0600) as the
        // signing-key file is.
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let write = |mut file: fs::File| {
            use std::io::Write as _;
            file.write_all(content.as_bytes())
        };
        options
            .open(&path)
            .and_then(write)
            .with_context(|| format!("Failed to write config to {}", path.display()))?;
        #[cfg(unix)]
        {
            // `mode` applies only on creation; tighten a file an older CLI wrote.
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to restrict {}", path.display()))?;
        }
        Ok(())
    }
}

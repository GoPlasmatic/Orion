use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;

use crate::config::OrionConfig;

#[derive(Args)]
#[command(
    long_about = "Manage CLI configuration stored in ~/.orion/config.toml.\n\n\
        Config precedence: CLI flags > environment variables > config file."
)]
pub struct ConfigCmd {
    #[command(subcommand)]
    command: ConfigSubcommand,
}

#[derive(Subcommand)]
enum ConfigSubcommand {
    /// Set the Orion server URL
    SetServer {
        /// Server URL (e.g., http://localhost:8080)
        url: String,
    },
    /// Show current configuration
    Show,
    /// Get a single configuration value (for scripting)
    #[command(
        after_help = "Examples:\n  orion-cli config get server_url\n  SERVER=$(orion-cli config get server_url)"
    )]
    Get {
        /// Configuration key (server_url, default_output, api_key, api_key_header)
        key: String,
    },
    /// Set a configuration value
    #[command(
        after_help = "Examples:\n  orion-cli config set server_url http://prod.example.com:8080\n  orion-cli config set default_output json\n  orion-cli config set api_key my-secret-key\n  orion-cli config set api_key_header X-Custom-Auth"
    )]
    Set {
        /// Configuration key (server_url, default_output, api_key, api_key_header)
        key: String,
        /// Value to set
        value: String,
    },
}

impl ConfigCmd {
    pub async fn run(&self) -> Result<()> {
        match &self.command {
            ConfigSubcommand::SetServer { url } => {
                let mut config = OrionConfig::load()?;
                config.server_url = Some(url.clone());
                config.save()?;
                println!("{} Server URL set to {}", "OK".green().bold(), url.cyan());
            }
            ConfigSubcommand::Show => {
                let config = OrionConfig::load()?;
                let path = OrionConfig::path()?;
                println!("{}", "Configuration".bold());
                println!("  Path:           {}", path.display().to_string().dimmed());
                println!(
                    "  Server URL:     {}",
                    config.server_url.as_deref().unwrap_or("(not set)")
                );
                println!("  Output:         {}", config.default_output);
                println!(
                    "  API Key:        {}",
                    config
                        .api_key
                        .as_deref()
                        .map(mask_secret)
                        .unwrap_or_else(|| "(not set)".to_string())
                );
                if let Some(header) = &config.api_key_header {
                    println!("  API Key Header: {header}");
                }
            }
            ConfigSubcommand::Get { key } => {
                let config = OrionConfig::load()?;
                let value = match key.as_str() {
                    "server_url" => config.server_url.unwrap_or_default(),
                    "default_output" => config.default_output,
                    "api_key" => config.api_key.unwrap_or_default(),
                    "api_key_header" => config.api_key_header.unwrap_or_default(),
                    _ => anyhow::bail!(
                        "Unknown config key: {key}. Valid keys: server_url, default_output, api_key, api_key_header"
                    ),
                };
                println!("{value}");
            }
            ConfigSubcommand::Set { key, value } => {
                let mut config = OrionConfig::load()?;
                match key.as_str() {
                    "server_url" => config.server_url = Some(value.clone()),
                    "default_output" => config.default_output = value.clone(),
                    "api_key" => config.api_key = Some(value.clone()),
                    "api_key_header" => config.api_key_header = Some(value.clone()),
                    _ => anyhow::bail!(
                        "Unknown config key: {key}. Valid keys: server_url, default_output, api_key, api_key_header"
                    ),
                }
                config.save()?;
                println!("{} {} = {}", "OK".green().bold(), key.cyan(), value);
            }
        }
        Ok(())
    }
}

fn mask_secret(s: &str) -> String {
    if s.len() <= 4 {
        "****".to_string()
    } else {
        format!("****{}", &s[s.len() - 4..])
    }
}

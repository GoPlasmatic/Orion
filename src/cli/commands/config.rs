use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;

use crate::config::CliConfig;

#[derive(Args)]
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
    /// Set a configuration value
    Set {
        /// Configuration key (server_url, default_output)
        key: String,
        /// Value to set
        value: String,
    },
}

impl ConfigCmd {
    pub async fn run(&self) -> Result<()> {
        match &self.command {
            ConfigSubcommand::SetServer { url } => {
                let mut config = CliConfig::load()?;
                config.server_url = Some(url.clone());
                config.save()?;
                println!("{} Server URL set to {}", "OK".green().bold(), url.cyan());
            }
            ConfigSubcommand::Show => {
                let config = CliConfig::load()?;
                let path = CliConfig::path()?;
                println!("{}", "Configuration".bold());
                println!("  Path:       {}", path.display().to_string().dimmed());
                println!(
                    "  Server URL: {}",
                    config.server_url.as_deref().unwrap_or("(not set)")
                );
                println!("  Output:     {}", config.default_output);
            }
            ConfigSubcommand::Set { key, value } => {
                let mut config = CliConfig::load()?;
                match key.as_str() {
                    "server_url" => config.server_url = Some(value.clone()),
                    "default_output" => config.default_output = value.clone(),
                    _ => anyhow::bail!(
                        "Unknown config key: {key}. Valid keys: server_url, default_output"
                    ),
                }
                config.save()?;
                println!("{} {} = {}", "OK".green().bold(), key.cyan(), value);
            }
        }
        Ok(())
    }
}

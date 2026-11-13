//! The server binary's command line, for any edition: [`main`] parses the
//! arguments, loads the config, runs an operator subcommand
//! (`validate-config`, `migrate`, `test-connectivity`, `dump-openapi`) or
//! serves the node with the composition the edition builds.
//!
//! `orion-server` is [`Binary::orion_server`] with Community's composition.
//! An edition binary is the same few lines with its own [`Binary`] and
//! composition:
//!
//! ```no_run
//! # use orion::cli::Binary;
//! # use orion::extensions::CompositionRoot;
//! #[tokio::main]
//! async fn main() -> std::process::ExitCode {
//!     let binary = Binary::new(
//!         "orion-enterprise",
//!         "2.0.0",
//!         "2.0.0",
//!         "Orion Enterprise — Declarative Services Runtime",
//!     );
//!     orion::cli::main(binary, |_config| {
//!         // Read the licence and `[extensions.<id>]` from `_config`, then
//!         // register the edition's drivers and providers.
//!         Ok(CompositionRoot::community())
//!     })
//!     .await
//! }
//! ```

mod config_cmd;
mod connectivity;
mod migrate;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

use crate::config::{self, AppConfig};
use crate::extensions::CompositionBuilder;

use config_cmd::{ConfigFormat, handle_validate_config, parse_wait};
use connectivity::run_test_connectivity;
use migrate::{handle_migrate, run_dump_openapi};

/// What a server binary calls itself in `--help` and `--version`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Binary {
    /// The executable's name, used in usage lines and the help's examples.
    pub name: &'static str,
    /// What `--version` prints after the name.
    pub version: &'static str,
    /// What `--version` prints when a build identity is wanted (the long
    /// form); often the version plus a commit and a build time.
    pub long_version: &'static str,
    /// The one-line description at the top of `--help`.
    pub about: &'static str,
}

impl Binary {
    pub const fn new(
        name: &'static str,
        version: &'static str,
        long_version: &'static str,
        about: &'static str,
    ) -> Self {
        Self {
            name,
            version,
            long_version,
            about,
        }
    }

    /// The Community server binary.
    pub const fn orion_server() -> Self {
        Self::new(
            "orion-server",
            env!("CARGO_PKG_VERSION"),
            concat!(
                env!("CARGO_PKG_VERSION"),
                "\ngit hash:  ",
                env!("GIT_HASH"),
                "\nbuilt:     ",
                env!("BUILD_TIMESTAMP"),
            ),
            "Orion — Declarative Services Runtime",
        )
    }
}

#[derive(clap::Parser)]
#[command(name = "orion-server")]
struct Cli {
    /// Path to TOML configuration file
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Validate configuration without starting the server, then print the
    /// full effective config (defaults + file + ORION_* env overrides) with
    /// secrets masked. `--format summary` prints a short human summary
    /// instead.
    ValidateConfig {
        /// Output format for the effective config.
        #[arg(long, value_enum, default_value = "toml")]
        format: ConfigFormat,
    },
    /// Run database migrations without starting the server.
    Migrate {
        /// Preview pending migrations without applying them.
        #[arg(long)]
        dry_run: bool,
        /// Keep retrying until the state database accepts connections, for at
        /// most this long (`60`, `30s`, `5m`). Only connection failures are
        /// retried; a wrong password or a failed migration stops at once.
        /// Overrides `storage.connect_retry_secs` for this run.
        #[arg(long, value_name = "DURATION", value_parser = parse_wait)]
        wait: Option<std::time::Duration>,
    },
    /// Probe the state database for reachability.
    ///
    /// Opens the configured database pool (using the same `storage.url`)
    /// and runs a no-op query. Catches "DB credentials wrong / file
    /// unreadable" before the server tries to start.
    TestConnectivity {
        /// Keep retrying until the state database accepts connections, for
        /// at most this long in total (`60`, `30s`, `5m`). Only connection
        /// failures are retried.
        #[arg(long, value_name = "DURATION", value_parser = parse_wait)]
        wait: Option<std::time::Duration>,
    },
    /// Print the public HTTP API's OpenAPI 3.1 spec as JSON to stdout.
    ///
    /// Needs no config, database, or running server.
    DumpOpenapi,
}

/// The help's closing sections, with `name` in every example.
fn after_help(name: &str) -> String {
    format!(
        "\
EXAMPLES:
    {name}                              Start with default config
    {name} -c config.toml               Start with a config file
    {name} validate-config              Validate + dump effective config (TOML)
    {name} validate-config --format summary  Short human summary instead
    {name} -c config.toml migrate       Run pending database migrations
    {name} migrate --dry-run            Preview pending migrations
    {name} migrate --wait 60s           Wait up to 60s for the database, then migrate
    {name} test-connectivity            Probe the database
    {name} dump-openapi > spec.json     Write the OpenAPI 3.1 spec to a file

AUTHORING:
    The offline tools (lint, compile, dry-run, test, fmt, clippy, sql check,
    plugin digest|sign|verify) are orion-cli subcommands.

ENVIRONMENT VARIABLES:
    All settings can be overridden via ORION_SECTION__KEY env vars:

    ORION_SERVER__PORT=9090            Override server port
    ORION_STORAGE__URL=sqlite:app.db   Override database URL
    ORION_LOGGING__LEVEL=debug         Override log level
    ORION_ENVIRONMENT=production       Set deployment environment

    See config.toml.example for all available settings."
    )
}

/// The command line as `binary` presents it.
fn command(binary: &Binary) -> clap::Command {
    Cli::command()
        .name(binary.name)
        .bin_name(binary.name)
        .version(binary.version)
        .long_version(binary.long_version)
        .about(binary.about)
        .after_help(after_help(binary.name))
}

/// Run the server binary: parse the process arguments, load the config, then
/// either run the operator subcommand named or serve the node `compose`
/// builds. `compose` runs only when the node is about to serve, after the
/// config is loaded and validated, so it can read the edition's own
/// `[extensions.<id>]` tables (and refuse to start by returning an error).
///
/// Returns the process exit code: success, or failure after printing the
/// error and its causes to stderr.
pub async fn main<F>(binary: Binary, compose: F) -> ExitCode
where
    F: FnOnce(&AppConfig) -> Result<CompositionBuilder, Box<dyn std::error::Error>>,
{
    let matches = command(&binary).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    match run(cli, compose).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            let mut source = std::error::Error::source(&*err);
            while let Some(cause) = source {
                eprintln!("  Caused by: {cause}");
                source = std::error::Error::source(cause);
            }
            ExitCode::FAILURE
        }
    }
}

async fn run<F>(cli: Cli, compose: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&AppConfig) -> Result<CompositionBuilder, Box<dyn std::error::Error>>,
{
    let mut config = config::load_config(cli.config.as_deref())?;
    // Resolve the instance identity once, up front, so the tracing resource
    // and the cluster runtime agree on it.
    config.cluster.instance_id = config.cluster.effective_instance_id();
    let config = config;

    if cli.config.is_none() {
        eprintln!(
            "Note: no config file specified (-c <path>). Using defaults + ORION_* env overrides."
        );
    }

    match cli.command {
        Some(Command::ValidateConfig { format }) => {
            return handle_validate_config(&config, format);
        }
        Some(Command::Migrate { dry_run, wait }) => {
            return handle_migrate(&config, dry_run, wait).await;
        }
        Some(Command::TestConnectivity { wait }) => {
            return run_test_connectivity(&config, wait).await;
        }
        Some(Command::DumpOpenapi) => return run_dump_openapi(),
        None => {}
    }

    let composition = compose(&config)?;
    crate::run(config, composition).await.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        command(&Binary::orion_server()).debug_assert();
    }

    #[test]
    fn an_edition_binary_names_itself_throughout_its_help() {
        let binary = Binary::new("orion-enterprise", "9.9.9", "9.9.9 (test)", "An edition");
        let mut cmd = command(&binary);
        let help = cmd.render_long_help().to_string();
        assert!(help.contains("An edition"), "{help}");
        assert!(help.contains("orion-enterprise validate-config"), "{help}");
        assert!(!help.contains("orion-server"), "{help}");
        assert_eq!(cmd.render_version().trim(), "orion-enterprise 9.9.9");
    }

    #[test]
    fn a_subcommand_parses_under_any_binary_name() {
        let binary = Binary::new("orion-enterprise", "1", "1", "An edition");
        let matches = command(&binary)
            .try_get_matches_from(["orion-enterprise", "-c", "x.toml", "migrate", "--dry-run"])
            .expect("parses");
        let cli = Cli::from_arg_matches(&matches).expect("binds");
        assert_eq!(cli.config.as_deref(), Some("x.toml"));
        assert!(matches!(
            cli.command,
            Some(Command::Migrate {
                dry_run: true,
                wait: None
            })
        ));
    }
}

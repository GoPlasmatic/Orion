use clap::Parser;

use orion::config;

mod cli;

#[derive(Parser)]
#[command(
    name = "orion-server",
    version,
    long_version = concat!(
        env!("CARGO_PKG_VERSION"),
        "\ngit hash:  ", env!("GIT_HASH"),
        "\nbuilt:     ", env!("BUILD_TIMESTAMP"),
    ),
    about = "Orion — Declarative Services Runtime",
    long_about = "Orion — Declarative Services Runtime\n\n\
        A workflow engine that processes data through configurable channels \
        and workflows. Supports REST, HTTP, cron, and async processing modes.\n\
        Ships as a single binary with an embedded SQLite database.",
    after_help = "\
EXAMPLES:\n    \
    orion-server                              Start with default config\n    \
    orion-server -c config.toml               Start with a config file\n    \
    orion-server validate-config              Validate + dump effective config (TOML)\n    \
    orion-server validate-config --format summary  Short human summary instead\n    \
    orion-server -c config.toml migrate       Run pending database migrations\n    \
    orion-server migrate --dry-run            Preview pending migrations\n    \
    orion-server migrate --wait 60s           Wait up to 60s for the database, then migrate\n    \
    orion-server test-connectivity            Probe the database\n    \
    orion-server dump-openapi > spec.json     Write the OpenAPI 3.1 spec to a file\n\n\
AUTHORING:\n    \
    The offline tools (lint, compile, dry-run, test, fmt, clippy, sql check,\n    \
    plugin digest|sign|verify) are orion-cli subcommands.\n\n\
ENVIRONMENT VARIABLES:\n    \
    All settings can be overridden via ORION_SECTION__KEY env vars:\n\n    \
    ORION_SERVER__PORT=9090            Override server port\n    \
    ORION_STORAGE__URL=sqlite:app.db   Override database URL\n    \
    ORION_LOGGING__LEVEL=debug         Override log level\n    \
    ORION_ENVIRONMENT=production       Set deployment environment\n\n    \
    See config.toml.example for all available settings."
)]
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
        format: cli::ConfigFormat,
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
        #[arg(long, value_name = "DURATION", value_parser = cli::parse_wait)]
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
        #[arg(long, value_name = "DURATION", value_parser = cli::parse_wait)]
        wait: Option<std::time::Duration>,
    },
    /// Print the public HTTP API's OpenAPI 3.1 spec as JSON to stdout.
    ///
    /// Needs no config, database, or running server. Redirect it to refresh
    /// the checked-in copy: `orion-server dump-openapi > docs/openapi.json`.
    DumpOpenapi,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Error: {err}");
        let mut source = std::error::Error::source(&*err);
        while let Some(cause) = source {
            eprintln!("  Caused by: {cause}");
            source = std::error::Error::source(cause);
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load configuration
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

    // Handle subcommands that exit early (before starting the server)
    match cli.command {
        Some(Command::ValidateConfig { format }) => {
            return cli::handle_validate_config(&config, format);
        }
        Some(Command::Migrate { dry_run, wait }) => {
            return cli::handle_migrate(&config, dry_run, wait).await;
        }
        Some(Command::TestConnectivity { wait }) => {
            return cli::run_test_connectivity(&config, wait).await;
        }
        Some(Command::DumpOpenapi) => return cli::run_dump_openapi(),
        None => {} // Continue to start the server
    }

    // Serve, with Community's composition: the channel drivers, connector
    // providers and package verifier this edition ships.
    orion::run(config, orion::extensions::CompositionRoot::community()).await
}

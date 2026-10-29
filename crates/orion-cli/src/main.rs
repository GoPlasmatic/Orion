mod client;
mod commands;
mod config;
mod help;
mod output;
pub mod utils;

use clap::Parser;
use colored::Colorize;
use std::process;

use client::OrionClient;
use commands::Commands;
use config::OrionConfig;
use output::OutputFormat;

#[derive(Parser)]
#[command(
    name = "orion-cli",
    version,
    long_version = concat!(
        env!("CARGO_PKG_VERSION"),
        "\ngit hash:  ", env!("GIT_HASH"),
        "\nbuilt:     ", env!("BUILD_TIMESTAMP"),
    ),
    about = "CLI tool for the Orion services runtime",
    long_about = "CLI tool for the Orion services runtime.\n\n\
        Deploy definitions to an Orion server, send data through its channels, and \
        inspect traces, the cron ledger and the engine.\n\n\
        Core concepts:\n  \
        Workflows    Processing pipelines with condition + task sequences\n  \
        Channels     Service endpoints that receive data and route it to a workflow\n  \
        Connections  External services (HTTP APIs, databases) a workflow calls\n  \
        Deployments  The committed set of definitions a server serves; \
        `deployments apply -d` sends one\n\n\
        Config precedence: CLI flags > environment variables > ~/.orion/config.toml",
    after_help = help::GETTING_STARTED,
)]
pub struct Cli {
    /// Orion server URL (overrides config)
    #[arg(
        long,
        global = true,
        env = "ORION_SERVER_URL",
        help_heading = "Connection"
    )]
    server: Option<String>,

    /// API key for admin authentication
    #[arg(
        long,
        global = true,
        env = "ORION_API_KEY",
        conflicts_with = "api_key_file",
        help_heading = "Connection"
    )]
    api_key: Option<String>,

    /// Path to a file containing the API key (`-` reads from stdin)
    #[arg(long, global = true, value_name = "PATH", help_heading = "Connection")]
    api_key_file: Option<std::path::PathBuf>,

    /// Header name for API key (default: Authorization with Bearer prefix)
    #[arg(
        long,
        global = true,
        env = "ORION_API_KEY_HEADER",
        help_heading = "Connection"
    )]
    api_key_header: Option<String>,

    /// Output format (default: `default_output` from the config file, else table)
    #[arg(long = "output", global = true, help_heading = "Output")]
    output_flag: Option<OutputFormat>,

    /// The output format in force: the flag, else the config file's
    /// `default_output`, else a table. Resolved once after parsing.
    #[arg(skip = OutputFormat::Table)]
    output: OutputFormat,

    /// Suppress output, print only IDs or minimal info
    #[arg(
        long,
        global = true,
        conflicts_with = "verbose",
        help_heading = "Output"
    )]
    quiet: bool,

    /// Show full response bodies and extra details
    #[arg(long, global = true, help_heading = "Output")]
    verbose: bool,

    /// Disable colored output (also honors nonempty NO_COLOR)
    #[arg(long, global = true, help_heading = "Output")]
    no_color: bool,

    /// Audit label for this change, e.g. "ticket=OPS-4412"
    #[arg(
        long,
        global = true,
        env = "ORION_CHANGE_CONTEXT",
        value_name = "CONTEXT",
        help_heading = "Connection"
    )]
    change_context: Option<String>,

    /// Skip confirmation prompts
    #[arg(long, global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();
    cli.output = resolve_output(cli.output_flag.clone());

    if cli.no_color || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        colored::control::set_override(false);
    }

    let output = cli.output.clone();
    let result = run(cli).await;
    match result {
        Ok(code) => process::exit(code),
        Err(e) if !matches!(output, OutputFormat::Table) => {
            // A script asked for json/yaml: give it the error in that shape on
            // stderr, with the server's code, request id and field errors, so
            // it need not scrape prose.
            let body = error_json(&e);
            let rendered = match output {
                OutputFormat::Yaml => serde_saphyr::to_string(&body).unwrap_or_default(),
                _ => serde_json::to_string_pretty(&body).unwrap_or_default(),
            };
            eprintln!("{}", rendered.trim_end());
            process::exit(1);
        }
        Err(e) => {
            // `{:#}` prints the whole context chain, so a step that names
            // what it was doing ("uploading 'x'") still shows why it failed.
            eprintln!("{} {e:#}", "Error:".red().bold());
            for detail in error_details(&e) {
                eprintln!("  {}: {} {}", detail.path, detail.code, detail.message);
            }
            process::exit(1);
        }
    }
}

/// The format in force: the `--output` flag, else the config file's
/// `default_output`, else a table. An unreadable config or an unknown saved
/// value falls back to a table rather than failing a command that never
/// asked for it.
fn resolve_output(flag: Option<OutputFormat>) -> OutputFormat {
    use clap::ValueEnum as _;
    flag.or_else(|| {
        let saved = config::OrionConfig::load().ok()?.default_output;
        OutputFormat::from_str(&saved, true).ok()
    })
    .unwrap_or(OutputFormat::Table)
}

/// Find the underlying [`orion_client::ClientError`] in an `anyhow::Error` chain,
/// whether attached directly or wrapped in [`client::CliClientError`].
fn find_client_error(e: &anyhow::Error) -> Option<&orion_client::ClientError> {
    e.chain().find_map(|cause| {
        cause
            .downcast_ref::<orion_client::ClientError>()
            .or_else(|| {
                cause
                    .downcast_ref::<client::CliClientError>()
                    .map(client::CliClientError::client_error)
            })
    })
}

/// An error as structured output: `{"error": {"code", "message", "request_id",
/// "details"}}`, the server's envelope when it sent one.
fn error_json(e: &anyhow::Error) -> serde_json::Value {
    let api = match find_client_error(e) {
        Some(orion_client::ClientError::Api { error, .. }) => Some(error.clone()),
        _ => None,
    };
    match api {
        Some(body) => serde_json::json!({ "error": body }),
        None => {
            serde_json::json!({ "error": { "code": "CLI_ERROR", "message": format!("{e:#}") } })
        }
    }
}

/// The per-field refusals an API error carried, when attached as a raw
/// [`orion_client::ClientError`] (rendered [`client::CliClientError`] values
/// already include formatted field details in their display text).
fn error_details(e: &anyhow::Error) -> &[orion_api::FieldError] {
    e.chain()
        .find_map(
            |cause| match cause.downcast_ref::<orion_client::ClientError>() {
                Some(orion_client::ClientError::Api { error, .. }) => {
                    Some(error.details.as_slice())
                }
                _ => None,
            },
        )
        .unwrap_or_default()
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    match &cli.command {
        Commands::Config(cmd) => {
            cmd.run().await?;
            Ok(0)
        }
        Commands::Health => {
            let client = build_client(&cli)?;
            commands::health::run(&client, &cli.output, cli.quiet).await
        }
        Commands::Send(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.verbose).await
        }
        Commands::Traces(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Engine(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Functions(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Metrics(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client).await
        }
        Commands::AuditLogs(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Backups(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Cache(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Dlq(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet, cli.yes).await
        }
        Commands::Cron(cmd) => {
            let client = build_client(&cli)?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Deployments(cmd) => {
            let client = build_client(&cli)?;
            commands::v2::require_api_version(&client).await?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Provider(cmd) => {
            let client = build_client(&cli)?;
            commands::v2::require_api_version(&client).await?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Connections(cmd) => {
            let client = build_client(&cli)?;
            commands::v2::require_api_version(&client).await?;
            cmd.run(&client, &cli.output, cli.quiet).await
        }
        Commands::Completions(cmd) => {
            cmd.run();
            Ok(0)
        }
        Commands::Lint(cmd) => cmd.run(),
        Commands::Compile(cmd) => cmd.run(),
        Commands::DryRun(cmd) => cmd.run().await,
        Commands::Test(cmd) => cmd.run().await,
        Commands::Fmt(cmd) => cmd.run(),
        Commands::Clippy(cmd) => cmd.run(),
        Commands::Sql(cmd) => cmd.run().await,
        Commands::Plugin(cmd) => cmd.run(),
    }
}

fn build_client(cli: &Cli) -> anyhow::Result<OrionClient> {
    let server_url = if let Some(url) = &cli.server {
        url.clone()
    } else {
        let config = OrionConfig::load()?;
        config.server_url.ok_or_else(|| {
            anyhow::anyhow!(
                "No server URL configured. Run {} or use {}",
                "orion-cli config set-server <url>".yellow(),
                "--server <url>".yellow()
            )
        })?
    };

    // Flag first, then `--api-key-file`, then the env var, then
    // `~/.orion/config.toml`. Reading the flag alone would mean
    // `orion-cli config set api_key <key>` stored a key no command ever sent,
    // and every request went out unauthenticated.
    let api_key = if let Some(key) = &cli.api_key {
        Some((key.clone(), cli.api_key_header.clone()))
    } else if let Some(path) = &cli.api_key_file {
        let key = read_api_key_file(path)?;
        Some((key, cli.api_key_header.clone()))
    } else {
        OrionConfig::resolve_api_key()
    };

    let mut client = OrionClient::new(&server_url)?;
    if cli.verbose {
        eprintln!("{} {}", "Server:".dimmed(), client.base_url());
    }
    if let Some((key, header)) = api_key {
        client = client.with_api_key(key, header);
    }
    if let Some(context) = &cli.change_context {
        client = client.with_change_context(context.clone());
    }
    // An admin key over plain http to anything but the local machine crosses
    // the network in the clear; say so once rather than let it pass silently.
    // Printed from the URL we were given, not from the keyed client — the
    // text is the same, and nothing derived from the key reaches stderr.
    if client.sends_credential_in_clear() {
        eprintln!(
            "{} API key will be sent over plain http to {} — use https for any server that is not local",
            "Warning:".yellow().bold(),
            server_url.trim_end_matches('/')
        );
    }
    Ok(client)
}

/// Read an admin API key from `path` (or `stdin` when `path` is `-`),
/// trimming trailing CR/LF so a single-line secret file or piped output works
/// without embedding a newline in the header value.
fn read_api_key_file(path: &std::path::Path) -> anyhow::Result<String> {
    use anyhow::Context as _;
    use std::io::Read as _;
    let raw = if path == std::path::Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("Failed to read API key from stdin")?;
        buf
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read API key from {}", path.display()))?
    };
    let key = raw.trim_end_matches(['\r', '\n']).to_string();
    if key.is_empty() {
        anyhow::bail!("API key file '{}' is empty", path.display());
    }
    Ok(key)
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn read_api_key_file_trims_trailing_newline_and_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("key.txt");
        std::fs::write(&key_path, "secret-token-123\n").unwrap();
        assert_eq!(read_api_key_file(&key_path).unwrap(), "secret-token-123");

        let empty_path = dir.path().join("empty.txt");
        std::fs::write(&empty_path, "\n").unwrap();
        assert!(read_api_key_file(&empty_path).is_err());
    }

    #[test]
    fn api_key_and_api_key_file_conflict() {
        let res = Cli::try_parse_from([
            "orion-cli",
            "--api-key",
            "inline",
            "--api-key-file",
            "/tmp/key",
            "health",
        ]);
        assert!(res.is_err());
    }

    #[test]
    fn an_api_error_keeps_its_code_request_id_and_field_errors() {
        let make_api = || orion_client::ClientError::Api {
            status: orion_client::StatusCode::UNPROCESSABLE_ENTITY,
            error: orion_api::ErrorBody {
                code: "VALIDATION_ERROR".into(),
                message: "bad".into(),
                details: vec![orion_api::FieldError::new("a.b", "REQUIRED", "missing")],
                request_id: Some("req-1".into()),
            },
        };
        for err in [
            anyhow::Error::new(make_api()).context("uploading 'x'"),
            client::render(make_api()).context("calling client"),
        ] {
            let body = error_json(&err);
            assert_eq!(body["error"]["code"], "VALIDATION_ERROR");
            assert_eq!(body["error"]["request_id"], "req-1");
            assert_eq!(body["error"]["details"][0]["path"], "a.b");
        }
    }

    #[test]
    fn any_other_error_is_a_cli_error() {
        let body = error_json(&anyhow::anyhow!("no such file"));
        assert_eq!(body["error"]["code"], "CLI_ERROR");
        assert_eq!(body["error"]["message"], "no such file");
    }

    #[test]
    fn the_flag_wins_over_the_saved_default() {
        assert!(matches!(
            resolve_output(Some(OutputFormat::Json)),
            OutputFormat::Json
        ));
    }
}

use orion::config;

/// Output format for `validate-config`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum ConfigFormat {
    /// The full effective config as TOML, secrets masked (the default).
    Toml,
    /// The full effective config as JSON, secrets masked.
    Json,
    /// A short human-readable summary of the headline settings.
    Summary,
}

/// `validate-config` subcommand: dump the effective configuration and exit.
///
/// `toml`/`json` print the *entire* config — serialized from the structs the
/// server actually runs on, so a new section can never be omitted the way the
/// old hand-maintained summary omitted `[cluster]`, `[queue]` and
/// `[tracing.storage]` (O15). The validity note goes to stderr so stdout
/// stays machine-parseable.
pub(crate) fn handle_validate_config(
    config: &config::AppConfig,
    format: ConfigFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    // The shape of `[packages]` was checked with the rest; its files only
    // here and at startup, where they are about to be used.
    let problems = orion::package::boot::check_files(&config.packages);
    if !problems.is_empty() {
        for (_, problem) in &problems {
            eprintln!("error: {problem}");
        }
        return Err(format!(
            "{} problem(s) with the [packages] artifacts",
            problems.len()
        )
        .into());
    }
    match format {
        ConfigFormat::Summary => print_config_summary(config),
        ConfigFormat::Toml => {
            eprintln!("Configuration is valid.");
            let masked = masked_effective_config(config)?;
            print!(
                "{}",
                toml::to_string_pretty(&toml::Value::try_from(&masked)?)?
            );
        }
        ConfigFormat::Json => {
            eprintln!("Configuration is valid.");
            let masked = masked_effective_config(config)?;
            println!("{}", serde_json::to_string_pretty(&masked)?);
        }
    }
    Ok(())
}

/// The full effective config — defaults + file + env overrides, as merged at
/// startup — with secrets masked, as a JSON tree.
///
/// The tree goes through `toml::Value` first so unset `Option` fields drop
/// out the way an authored config file omits them (TOML has no null).
/// Masking reuses the connector policy (`orion::connector::mask_secrets`):
/// values under secret-looking keys (`kafka.auth.sasl_password`,
/// `admin_auth.api_keys`) are replaced wholesale, and URL userinfo passwords
/// (`storage.url`, `cluster.redis_url`) are redacted in place.
fn masked_effective_config(
    config: &config::AppConfig,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let without_unset = toml::Value::try_from(config)?;
    let mut tree = serde_json::to_value(&without_unset)?;
    orion::connector::mask_secrets(&mut tree);
    Ok(tree)
}

/// URL-shaped values the summary and the connectivity probe print verbatim can
/// embed `user:password@` credentials or secret-named query parameters; show
/// them with both positions struck out.
pub(super) fn redacted(value: &str) -> String {
    orion::connector::redact_url_secrets_or_raw(value)
}

/// The `summary` format: the handful of headline settings an operator scans
/// for on a box. Everything else is in `--format toml`/`json`.
fn print_config_summary(config: &config::AppConfig) {
    println!("Configuration is valid.\n");
    println!("  environment:     {}", config.environment);
    println!(
        "  server:          {}:{}",
        config.server.host, config.server.port
    );
    println!(
        "  tls:             {}",
        if config.server.tls.enabled {
            format!("enabled (cert={})", config.server.tls.cert_path)
        } else {
            "disabled".to_string()
        }
    );
    println!("  storage:         {}", redacted(&config.storage.url));
    println!(
        "  logging:         level={}, format={}",
        config.logging.level,
        match config.logging.format {
            config::LogFormat::Json => "json",
            config::LogFormat::Pretty => "pretty",
        }
    );
    println!(
        "  admin_auth:      {}",
        if config.admin_auth.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  cors:            {}{}",
        config.cors.allowed_origins.join(", "),
        // Credentials change what a listed origin may do on a user's behalf,
        // so a summary that omits it understates the exposure.
        if config.cors.allow_credentials {
            " (credentials allowed)"
        } else {
            ""
        }
    );
    println!(
        "  rate_limiting:   {}",
        if config.rate_limit.enabled {
            format!(
                "enabled (rps={}, burst={})",
                config.rate_limit.default_rps, config.rate_limit.default_burst
            )
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  queue:           workers={}, buffer={}",
        config.trace_queue.workers, config.trace_queue.buffer_size
    );
    println!(
        "  metrics:         {}",
        if config.metrics.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  tracing:         {}",
        if config.tracing.enabled {
            format!(
                "enabled (endpoint={})",
                redacted(&config.tracing.otlp_endpoint)
            )
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  cluster:         {}",
        if config.cluster.enabled {
            format!("enabled (instance_id={})", config.cluster.instance_id)
        } else {
            "disabled".to_string()
        }
    );
    println!(
        "  kafka:           {}",
        if config.kafka.enabled {
            let brokers: Vec<String> = config.kafka.brokers.iter().map(|b| redacted(b)).collect();
            format!("enabled (brokers={})", brokers.join(","))
        } else {
            "disabled".to_string()
        }
    );
}

/// `--wait <DURATION>`: a bare integer is seconds (`60`); otherwise `<n>s`,
/// `<n>m`, `<n>h` or `<n>d`. Zero is one attempt.
pub(crate) fn parse_wait(value: &str) -> Result<std::time::Duration, String> {
    let value = value.trim();
    let secs = match value.parse::<u64>() {
        Ok(secs) => secs,
        Err(_) => orion::engine::functions::connector_helpers::parse_duration_secs(value)?,
    };
    Ok(std::time::Duration::from_secs(secs))
}

/// Open the state database for a CLI subcommand, printing a line on stderr
/// for every retry so a wait is never silent.
///
/// Without `wait` the window and the retry-everything policy are the
/// server's own (`storage.connect_retry_secs`). With it, the window is the
/// flag's, only connection failures are retried, and giving up says how
/// long it waited.
pub(super) async fn open_state_db(
    config: &config::AppConfig,
    wait: Option<std::time::Duration>,
) -> Result<orion::storage::DbPool, Box<dyn std::error::Error>> {
    use orion::storage::{ConnectWait, RetryReport};

    let started = std::time::Instant::now();
    let on_retry = |report: &RetryReport<'_>| {
        eprintln!(
            "waiting for the state database ({}) … {}s",
            report.reason,
            report.elapsed.as_secs()
        );
    };
    let policy = ConnectWait {
        window: wait.unwrap_or(std::time::Duration::from_secs(
            config.storage.connect_retry_secs,
        )),
        transient_only: wait.is_some(),
        on_retry: Some(&on_retry),
    };
    match orion::storage::init_pool_no_migrate_waiting(&config.storage, policy).await {
        Ok(pool) => Ok(pool),
        Err(err) if wait.is_some() && orion::storage::is_transient_connect_error(&err) => {
            Err(Box::new(WaitExhausted {
                what: "state database",
                waited: started.elapsed(),
                source: Box::new(err),
            }))
        }
        Err(err) => Err(Box::new(err)),
    }
}

/// A dependency still unreachable when `--wait` ran out. The cause chain
/// `main` prints carries the last error's own text.
#[derive(Debug)]
struct WaitExhausted {
    what: &'static str,
    waited: std::time::Duration,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for WaitExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} not reachable after {}s",
            self.what,
            self.waited.as_secs()
        )
    }
}

impl std::error::Error for WaitExhausted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

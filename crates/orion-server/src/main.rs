// T42: `main.rs` + `cli.rs` are their own crate root, so `lib.rs`'s panic
// lints did not reach them — a future unjustified unwrap on the CLI path
// would have compiled clean.
#![warn(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use clap::Parser;

use orion::config;

mod cli;
mod package_cli;

use orion::bootstrap;

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
        and workflows. Supports REST, HTTP, Kafka, and async processing modes.\n\
        Ships as a single binary with an embedded SQLite database.",
    after_help = "\
EXAMPLES:\n    \
    orion-server                              Start with default config\n    \
    orion-server -c config.toml               Start with a config file\n    \
    orion-server validate-config              Validate + dump effective config (TOML)\n    \
    orion-server validate-config --format summary  Short human summary instead\n    \
    orion-server -c config.toml migrate       Run pending database migrations\n    \
    orion-server migrate --dry-run            Preview pending migrations\n    \
    orion-server lint workflow.json           Validate a workflow JSON file\n    \
    orion-server dry-run -w wf.json -i x.json Dry-run a workflow against an input\n    \
    orion-server dry-run -w wf.json -i x.json --stubs s.json   ... with canned connector replies\n    \
    orion-server test examples/workflow-tests Run a directory of workflow test cases\n    \
    orion-server test-connectivity            Probe DB (and Kafka if enabled)\n    \
    orion-server preflight                    Scan stored channels/workflows before upgrading\n    \
    orion-server dump-openapi > spec.json     Write the OpenAPI 3.1 spec to a file\n    \
    orion-server package export -s <url> --tag payments --name payments --version 1.0.0 -o pkg.json\n                                              \
Export a promotion package from an instance\n    \
    orion-server package apply -s <url> -f pkg.json  Stage, activate and reload the package on a target\n\n\
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
    },
    /// Statically validate a workflow JSON file (A6).
    ///
    /// Runs the same checks the admin POST /workflows endpoint performs:
    /// name/id/description, task uniqueness, and the A1 function-input
    /// schema registry. Exits non-zero with field-pathed errors on
    /// failure — wire into CI to catch broken workflows before deploy.
    Lint {
        /// Path to a workflow JSON file, or a directory of definitions.
        ///
        /// A directory is linted as a **set**: every channel, workflow and
        /// connector under it, plus the references between them — the checks
        /// a per-file lint cannot make.
        workflow: String,
        /// Channel name that may be referenced without being in the set.
        /// Repeatable. Directory mode only; a package declares this in
        /// `requires`.
        #[arg(long = "requires-channel", value_name = "NAME")]
        requires_channels: Vec<String>,
        /// Connector name that may be referenced without being in the set.
        /// Repeatable.
        #[arg(long = "requires-connector", value_name = "NAME")]
        requires_connectors: Vec<String>,
        /// Exit non-zero on advisory findings too, not just errors.
        ///
        /// Named for `cargo clippy -- -D warnings` rather than `--strict`,
        /// which would read as a no-op on a command whose whole job is strict
        /// validation.
        #[arg(long)]
        deny_warnings: bool,
        /// Directory holding the set's shared definitions — the `constants`,
        /// `errors` and `fragments` documents a `$from` or a `use` resolves
        /// against. Expansion happens before validation, so what is checked
        /// is the expanded form. Implicit when linting a directory.
        #[arg(long, value_name = "DIR")]
        definitions: Option<String>,
    },
    /// Compile a definition set into files the admin API accepts.
    ///
    /// The authoring conveniences a set may use — `$from` for a shared value,
    /// `use` for a task fragment — resolve when the set is loaded, and the
    /// admin API loads no set. This is the step between the two: it runs
    /// every gate `lint <dir>` runs, then writes the compiled entities out.
    ///
    /// Default output is a promotion artifact, so `package plan|apply|diff`
    /// consume it directly.
    Compile {
        /// Directory of definitions to compile.
        dir: String,
        /// Where to write. A file for --format artifact (default: stdout),
        /// a directory for --format dir and --format bulk (required).
        #[arg(short, long)]
        output: Option<String>,
        /// What to write.
        #[arg(long, value_enum, default_value = "artifact")]
        format: cli::CompileFormat,
        /// Package name, e.g. payments. Required for --format artifact.
        #[arg(long)]
        name: Option<String>,
        /// Package version, e.g. 1.4.0. Required for --format artifact.
        /// Applied versions are immutable — any content change needs a bump.
        #[arg(long)]
        version: Option<String>,
        /// Channel name that may be referenced without being in the set —
        /// recorded in the artifact's `requires`. Repeatable.
        #[arg(long = "requires-channel", value_name = "NAME")]
        requires_channels: Vec<String>,
        /// Connector name that may be referenced without being in the set.
        /// Repeatable.
        #[arg(long = "requires-connector", value_name = "NAME")]
        requires_connectors: Vec<String>,
        /// Exit non-zero on advisory findings too, not just errors.
        #[arg(long)]
        deny_warnings: bool,
        /// Do not mark workflows and channels for activation. The artifact
        /// applies as drafts, for a promotion that activates separately.
        #[arg(long)]
        no_activate: bool,
    },
    /// Dry-run a workflow against a JSON input file (A6).
    ///
    /// Boots an in-process engine with just the supplied workflow, then prints
    /// the per-task execution trace from dataflow_rs.
    ///
    /// Connector-backed tasks (`http_call`, `db_read`, `data_query`,
    /// `channel_call`, …) are answered from `--stubs`; nothing reaches a real
    /// backend. Without a stub file such a task fails naming the stub that
    /// would satisfy it, so a workflow is never silently half-run.
    DryRun {
        /// Path to a workflow JSON file.
        #[arg(short, long)]
        workflow: String,
        /// Path to a JSON file used as the message payload.
        #[arg(short, long)]
        input: String,
        /// Path to a JSON file used as the message metadata — `headers`,
        /// `params`, `query`, `cookies`, `auth.claims`, `channel`, as the HTTP
        /// ingress would have built them. Header keys are lowercased and
        /// credential headers masked, so an offline run sees what production
        /// would.
        #[arg(short, long)]
        metadata: Option<String>,
        /// Path to a JSON file of canned connector responses:
        /// `{"http_call": {"crm": {...}}, "db_read": {"*": [...]}}`.
        /// The inner key is the task's `connector` (or `channel` for
        /// `channel_call`); `"*"` matches any.
        #[arg(short, long)]
        stubs: Option<String>,
        /// Path to a JSON file of stand-in values for the
        /// `{"secret": "name"}` references the workflow reads:
        /// `{"partner_hmac": "test-key"}`. An offline run has no `[secrets]`
        /// config to resolve, and an engine with no store refuses a workflow
        /// that names a secret. Values are used verbatim — use throwaway ones.
        #[arg(long)]
        secrets: Option<String>,
        /// Directory holding the set's shared definitions — the `constants`,
        /// `errors` and `fragments` documents a `$from` or a `use` resolves
        /// against. Expansion happens before validation, so what is checked
        /// and run is the expanded form.
        #[arg(long, value_name = "DIR")]
        definitions: Option<String>,
    },
    /// Run a directory of workflow test cases (A6).
    ///
    /// Each `*.case.json` case names a workflow, an input, optional connector stubs
    /// and the values expected in the output:
    ///
    ///     {"name": "flags high-value orders", "workflow": "wf.json",
    ///      "input": {...}, "stubs": {...},
    ///      "expect": {"data.order.flagged": true}}
    ///
    /// Paths inside a case are resolved relative to the case file. Prints a
    /// per-case diff and exits non-zero on any failure, so it gates CI the way
    /// `lint`, `validate-config` and `preflight` already do.
    Test {
        /// Directory of case files, or a single case file.
        path: String,
        /// Directory holding the set's shared definitions — the `constants`,
        /// `errors` and `fragments` documents a `$from` or a `use` resolves
        /// against. Expansion happens before validation, so what is checked
        /// and run is the expanded form.
        #[arg(long, value_name = "DIR")]
        definitions: Option<String>,
    },
    /// Probe configured backends for reachability (A6).
    ///
    /// Opens the configured database pool (using the same `storage.url`)
    /// and runs a no-op query. Catches "DB credentials wrong / file
    /// unreadable" before the server tries to start.
    TestConnectivity,
    /// Format definition files to the house style (like `cargo fmt`).
    ///
    /// Every `.json` under each PATH is rewritten in place — entities, shared
    /// documents, `*.case.json` files and fixtures alike. There is one style
    /// and nothing to configure: known keys of known shapes in canonical
    /// order, unary JSONLogic nodes always on one line, leaf nodes on one
    /// line when they fit in 100 columns, everything deeper broken one
    /// argument per line. Values, number spellings and the order of unknown
    /// keys are never changed, and the output is re-parsed and compared with
    /// the input before anything is written.
    Fmt {
        /// Files or directories. Default: the current directory.
        #[arg(default_value = ".")]
        paths: Vec<String>,
        /// Write nothing; print a diff for each file that is not formatted
        /// and exit 1 if there is one.
        #[arg(long)]
        check: bool,
        /// Format one document from stdin to stdout. PATH is ignored.
        #[arg(long, conflicts_with = "check")]
        stdin: bool,
    },
    /// Advisory checks beyond `lint`, said only when certain (like `cargo clippy`).
    ///
    /// Runs `lint`'s gate over the set, then every rule: a workflow condition
    /// that can never match, steps after an unconditional terminal step, an
    /// unconditional channel_call cycle, a read of `payload`, a mapping
    /// overwritten before it is read, runs of steps an existing fragment
    /// already expresses, objects repeated across the set, and more —
    /// `--list` names them, `--explain <rule>` states each one's proof and
    /// when it stays silent. There is no configuration and no suppression:
    /// a rule fires only when its finding is certain.
    Clippy {
        /// A directory of definitions (set mode: every rule), or one file.
        path: Option<String>,
        /// Exit non-zero on warnings too, not just errors.
        #[arg(long)]
        deny_warnings: bool,
        /// `text` (default) or `json` — one object per diagnostic on stdout.
        #[arg(long, value_enum, default_value = "text")]
        format: cli::ClippyFormat,
        /// Print every rule with its level, scope and summary, and exit.
        #[arg(long, conflicts_with_all = ["explain", "path"])]
        list: bool,
        /// Print one rule's rationale, proof and exclusions, and exit.
        #[arg(long, value_name = "RULE", conflicts_with = "path")]
        explain: Option<String>,
        /// Directory holding the set's shared definitions, for a single-file
        /// run. Implicit in set mode.
        #[arg(long, value_name = "DIR")]
        definitions: Option<String>,
        /// Channel name that may be referenced without being in the set.
        /// Repeatable.
        #[arg(long = "requires-channel", value_name = "NAME")]
        requires_channels: Vec<String>,
        /// Connector name that may be referenced without being in the set.
        /// Repeatable.
        #[arg(long = "requires-connector", value_name = "NAME")]
        requires_connectors: Vec<String>,
    },
    /// Print the public HTTP API's OpenAPI 3.1 spec as JSON to stdout.
    ///
    /// Needs no config, database, or running server. Redirect it to refresh
    /// the checked-in copy: `orion-server dump-openapi > docs/openapi.json`.
    DumpOpenapi,
    /// Package a set of channels + their workflows and connectors, and
    /// promote the artifact through environments (the K-stream design).
    ///
    /// The artifact is one JSON document; git is the registry. `export`
    /// computes the dependency closure from a running instance; `lint` checks
    /// an artifact offline; `plan` pre-flights it against a target with zero
    /// writes; `apply` stages, activates in dependency order, reloads once
    /// and records the package receipt; `diff` reports drift between the
    /// artifact and a running instance. Server calls authenticate with the
    /// ORION_ADMIN_TOKEN environment variable and are stamped with an
    /// `X-Orion-Change-Context: package=<name>@<version>` audit context.
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    /// Scan the stored channels and workflows for anything the 1.0 rules will
    /// refuse, before the upgrade rather than during it.
    ///
    /// Answers the database-backed rows of the 0.3.0 -> 1.0.0 upgrade
    /// checklist: channel configs that no longer parse (the pre-1.0 `cors` and
    /// `backpressure.max_concurrent` spellings, and typos that were always
    /// silently ignored), workflows whose tasks the create validator would
    /// reject, and `data_query`/`data_write` tasks with no `schema` — the one
    /// change that surfaces on live traffic rather than at startup.
    ///
    /// Read-only, and exits non-zero when it finds anything, so it can gate a
    /// deploy. Config-file and ORION_* problems are reported by
    /// `validate-config`; this reads what only the database knows.
    Preflight,
}

#[derive(clap::Subcommand)]
enum PackageCommand {
    /// Export a package artifact from a running instance: the selected
    /// channels, their workflows, and every connector those workflows
    /// reference (closure computed via GET /workflows/{id}/dependencies).
    /// channel_call targets outside the selection land in `requires`.
    Export {
        /// Base URL of the source instance, e.g. https://dev.orion.internal
        #[arg(short, long)]
        server: String,
        /// Select every channel carrying this tag.
        #[arg(long)]
        tag: Option<String>,
        /// Select channels by id (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
        /// Package name, e.g. payments.
        #[arg(long)]
        name: String,
        /// Package version, e.g. 1.4.0. Applied versions are immutable —
        /// any content change needs a bump.
        #[arg(long)]
        version: String,
        /// Write the artifact here instead of stdout.
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Validate an artifact offline: entity shapes (the same validators the
    /// POST endpoints run), closure completeness against `requires`, and the
    /// content hash. Exits non-zero on findings — the CI gate that needs no
    /// server and no secrets.
    Lint {
        /// Path to the artifact file.
        #[arg(short, long)]
        file: String,
    },
    /// Pre-flight an artifact against a target with zero writes: the receipt
    /// immutability check, per-entity would-be import actions, `requires`
    /// verification, and every activation gate.
    Plan {
        /// Base URL of the target instance.
        #[arg(short, long)]
        server: String,
        /// Path to the artifact file.
        #[arg(short, long)]
        file: String,
    },
    /// Apply an artifact: claim the receipt as staged, stage all entities
    /// (connectors → workflows → channels), activate in dependency order
    /// with one engine reload at the end, then flip the receipt to applied.
    /// Idempotent — re-running an identical artifact is a no-op.
    Apply {
        /// Base URL of the target instance.
        #[arg(short, long)]
        server: String,
        /// Path to the artifact file.
        #[arg(short, long)]
        file: String,
    },
    /// Report drift between an artifact and a running instance, comparing
    /// the server's content hashes against the artifact's. Exits non-zero
    /// when anything differs.
    Diff {
        /// Base URL of the instance to compare against.
        #[arg(short, long)]
        server: String,
        /// Path to the artifact file.
        #[arg(short, long)]
        file: String,
    },
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

    // `fmt` reads files, not a server: no config, no "no config file" note.
    if let Some(Command::Fmt {
        paths,
        check,
        stdin,
    }) = &cli.command
    {
        let code = cli::run_fmt(paths, *check, *stdin)?;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }

    // Load configuration
    let mut config = config::load_config(cli.config.as_deref())?;
    // Resolve the instance identity once, up front, so the tracing resource,
    // cluster runtime, and Kafka static membership all agree on it.
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
        Some(Command::Migrate { dry_run }) => return cli::handle_migrate(&config, dry_run).await,
        Some(Command::Lint {
            workflow,
            deny_warnings,
            requires_channels,
            requires_connectors,
            definitions,
        }) => {
            let boundary = orion::definitions::Boundary {
                channels: requires_channels,
                connectors: requires_connectors,
            };
            return cli::run_lint(&workflow, deny_warnings, boundary, definitions.as_deref());
        }
        Some(Command::Compile {
            dir,
            output,
            format,
            name,
            version,
            requires_channels,
            requires_connectors,
            deny_warnings,
            no_activate,
        }) => {
            let boundary = orion::definitions::Boundary {
                channels: requires_channels,
                connectors: requires_connectors,
            };
            return cli::run_compile(cli::CompileRequest {
                dir: &dir,
                output: output.as_deref(),
                format,
                name: name.as_deref(),
                version: version.as_deref(),
                boundary,
                deny_warnings,
                no_activate,
            });
        }
        Some(Command::DryRun {
            workflow,
            input,
            stubs,
            metadata,
            secrets,
            definitions,
        }) => {
            return cli::run_dry_run(
                &workflow,
                &input,
                stubs.as_deref(),
                metadata.as_deref(),
                secrets.as_deref(),
                definitions.as_deref(),
            )
            .await;
        }
        Some(Command::Test { path, definitions }) => {
            return cli::run_test(&path, definitions.as_deref()).await;
        }
        Some(Command::TestConnectivity) => return cli::run_test_connectivity(&config).await,
        Some(Command::Clippy {
            path,
            deny_warnings,
            format,
            list,
            explain,
            definitions,
            requires_channels,
            requires_connectors,
        }) => {
            let code = if list {
                cli::run_clippy_list()?
            } else if let Some(rule) = explain {
                cli::run_clippy_explain(&rule)?
            } else {
                let Some(path) = path else {
                    return Err(
                        "clippy needs a directory or file to check (or --list / --explain)".into(),
                    );
                };
                cli::run_clippy(cli::ClippyRequest {
                    path: &path,
                    deny_warnings,
                    format,
                    definitions: definitions.as_deref(),
                    boundary: orion::definitions::Boundary {
                        channels: requires_channels,
                        connectors: requires_connectors,
                    },
                    // Only a config the operator named counts as "the serving
                    // config": the defaults say nothing about [vars]/[secrets].
                    config: cli.config.is_some().then_some(&config),
                })?
            };
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        Some(Command::DumpOpenapi) => return cli::run_dump_openapi(),
        // Dispatched above, before the config load.
        Some(Command::Fmt { .. }) => unreachable!("fmt returns before config is loaded"),
        Some(Command::Preflight) => return cli::run_preflight(&config).await,
        Some(Command::Package { command }) => {
            return match command {
                PackageCommand::Export {
                    server,
                    tag,
                    channels,
                    name,
                    version,
                    output,
                } => {
                    package_cli::run_export(
                        &server,
                        tag.as_deref(),
                        &channels,
                        &name,
                        &version,
                        output.as_deref(),
                    )
                    .await
                }
                PackageCommand::Lint { file } => package_cli::run_lint(&file),
                PackageCommand::Plan { server, file } => {
                    package_cli::run_plan(&server, &file).await
                }
                PackageCommand::Apply { server, file } => {
                    package_cli::run_apply(&server, &file).await
                }
                PackageCommand::Diff { server, file } => {
                    package_cli::run_diff(&server, &file).await
                }
            };
        }
        None => {} // Continue to start the server
    }

    // Init tracing subscriber with optional OpenTelemetry layer (see
    // `bootstrap::init_observability`). The provider is flushed at shutdown.
    let _otel_provider = bootstrap::init_observability(&config)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        git_hash = env!("GIT_HASH"),
        build_timestamp = env!("BUILD_TIMESTAMP"),
        environment = %config.environment,
        "Starting Orion — Declarative Services Runtime"
    );

    // Init metrics (gated by config)
    let metrics_handle = bootstrap::init_metrics_handle(&config);

    // Init database. With auto_migrate = false (multi-replica deploys) a
    // stale schema is a hard startup error — a replica must never serve
    // against pending migrations; `orion-server migrate` is the deploy step.
    let pool = orion::storage::init_pool_for_startup(&config.storage).await?;
    // S20: the DSN can embed `user:password@` credentials — never log it raw.
    tracing::info!(
        storage = %orion::connector::redact_url_secrets_or_raw(&config.storage.url),
        "Database initialized"
    );
    // C7: in production this pairing is refused by `validate_config` before
    // anything opens a connection, so only a development cluster reaches here
    // — the Helm `devStack` shape, whose database is a release resource and so
    // has no pre-install migrate Job to run instead.
    if config.cluster.enabled && config.storage.auto_migrate {
        tracing::warn!(
            "cluster.enabled with storage.auto_migrate = true: replicas race \
             migrations at boot. Tolerated outside production and refused in it — \
             use auto_migrate = false plus an `orion-server migrate` deploy step"
        );
    }

    // Cluster runtime: instance identity + shared Redis (fails fast when
    // enabled and Redis is unreachable).
    let cluster = orion::cluster::init_cluster_runtime(&config.cluster, &pool).await?;
    tracing::info!(
        instance_id = %cluster.instance_id,
        cluster_enabled = cluster.enabled,
        "Instance identity"
    );

    // Create repositories
    let repos = bootstrap::Repositories::new(&pool, &config.storage)?;

    // Channel registry
    let channel_registry = Arc::new(if config.cluster.enabled {
        orion::channel::ChannelRegistry::with_cluster((&*cluster).into())
    } else {
        orion::channel::ChannelRegistry::new()
    });

    // Connector registry, shared HTTP client, engine lock, cache pools,
    // custom function handlers, and the Kafka producer (see
    // `bootstrap::build_engine_components`).
    let components =
        bootstrap::build_engine_components(&config, &repos, channel_registry.clone()).await?;

    // #268: hand the managed-OAuth2 token manager its runtime — the shared
    // client, the encrypted state store, and (in cluster mode) the refresh
    // lease that keeps N nodes from rotating against each other.
    components
        .serving
        .connector_registry
        .oauth()
        .init(orion::connector::oauth::OAuthRuntimeDeps {
            http_client: components.serving.http_client.clone(),
            repo: repos.connectors.clone(),
            lease: config.cluster.enabled.then(|| {
                std::sync::Arc::new(orion::cluster::JobLeaseGate::new(
                    cluster.repo.clone(),
                    cluster.instance_id.clone(),
                ))
            }),
        });

    // Readiness flag — set after engine is fully initialized
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Load active channels and workflows, build the engine, and populate the
    // pre-created engine lock. Channels that fail to load are quarantined —
    // refused at every ingress until fixed. Consumes `components`: the
    // handler map goes into the engine, and what comes back is the half that
    // backs `AppState` (F55).
    let (components, channels, active_workflow_count) = components
        .load_channels_and_build_engine(&config, &repos, &channel_registry)
        .await?;

    // Mark the service as ready now that the engine and channel registry are loaded
    ready.store(true, std::sync::atomic::Ordering::Release);

    // Start the background tasks: trace persistence queue, trace queue
    // worker pool (with DLQ for failed async traces), trace + audit-log
    // cleanup, and the DLQ retry consumer.
    // One supervisor for every long-lived background task. It goes onto
    // `AppState` so `/health` and `/readyz` can report their liveness, and
    // `main` keeps its own handle so shutdown can stop them.
    let tasks = Arc::new(orion::runtime::TaskRegistry::new());
    let (trace_persistence_queue, trace_queue, audit_queue, task_handles) =
        bootstrap::start_background_tasks(
            &config,
            &tasks,
            components.engine.clone(),
            &repos,
            channel_registry.clone(),
            &cluster,
        );

    // Kafka ingest starts **after** the background tasks, not before.
    //
    // The consumer now writes a `traces` row per message, so it needs the
    // persistence queue that `start_background_tasks` returns. Starting it
    // first would also have meant a window in which records were consumed and
    // dispatched with no trace sink behind them — the same reason the HTTP
    // server is started last.
    let kafka_consumer_handle = bootstrap::start_kafka_ingest(
        &config.kafka,
        &channels,
        bootstrap::IngestDeps {
            engine: components.engine.clone(),
            channel_registry: channel_registry.clone(),
            datalogic: components.datalogic.clone(),
            vars: components.vars.clone(),
            kafka_producer: components.kafka_producer.clone(),
            instance_id: cluster.enabled.then(|| cluster.instance_id.clone()),
            trace_repo: repos.traces.clone(),
            persistence_queue: trace_persistence_queue.clone(),
            max_result_size_bytes: config.trace_queue.max_result_size_bytes,
        },
    )?;

    // Set initial active rules gauge
    orion::metrics::set_active_workflows(active_workflow_count as f64);

    // Build rate limiter (if enabled)
    let rate_limit_state = bootstrap::build_rate_limit_state(&config);

    // Build state and router
    let config = Arc::new(config);

    let state = bootstrap::build_app_state(bootstrap::AppStateParams {
        config: config.clone(),
        pool,
        repos,
        components,
        channel_registry,
        trace_queue,
        trace_persistence_queue,
        audit_queue,
        rate_limit_state,
        metrics_handle,
        ready: ready.clone(),
        kafka_consumer_handle,
        cluster,
        tasks: tasks.clone(),
    });

    // Cluster background tasks (epoch watcher). None when disabled.
    orion::cluster::start_cluster_tasks(&state);

    let router = orion::server::build_router(state.clone());

    // Optional dedicated metrics listener (O12), bound before the main server
    // starts (see `bootstrap::start_metrics_listener`).
    let metrics_server = bootstrap::start_metrics_listener(&config, &state)?;

    if config.server.tls.enabled {
        let handle = axum_server::Handle::new();
        orion::server::serve::serve_tls(
            config.clone(),
            ready.clone(),
            router,
            handle,
            orion::server::shutdown_signal(),
        )
        .await?;
    } else {
        let addr = format!("{}:{}", config.server.host, config.server.port);
        let listener = orion::server::serve::create_tcp_listener(&addr)?;
        orion::server::serve::serve_plain_http(
            listener,
            config.clone(),
            ready.clone(),
            router,
            orion::server::shutdown_signal(),
        )
        .await?;
    }

    bootstrap::join_metrics_listener(metrics_server).await;

    // Graceful shutdown
    if let Some(handle) = state.kafka.consumer_handle.lock().await.take() {
        tracing::info!("Shutting down Kafka consumer...");
        handle.shutdown().await;
    }

    // Stop the supervised tasks first: the retention jobs, the DLQ retry
    // consumer and the epoch watcher all hold an `AppState` clone, and the
    // drain below cannot start until the last trace-queue sender is gone.
    // Cooperative, so a job in the middle of a DELETE finishes it — this
    // used to be `JoinHandle::abort()`.
    tasks
        .shutdown(std::time::Duration::from_secs(
            config.server.shutdown_force_timeout_secs,
        ))
        .await;

    // Release the state's trace-queue sender before draining the workers —
    // they exit when the last sender closes, and holding `state` here would
    // stall the drain until its timeout.
    drop(state);
    task_handles.shutdown().await;

    // Flush pending OTel spans before exit
    if let Some(provider) = _otel_provider {
        tracing::info!("Flushing OpenTelemetry spans...");
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "Error shutting down OTel tracer provider");
        }
    }

    tracing::info!("Orion shut down cleanly");
    Ok(())
}

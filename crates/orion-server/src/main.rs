// T42: `main.rs` + `cli.rs` are their own crate root, so `lib.rs`'s panic
// lints did not reach them — a future unjustified unwrap on the CLI path
// would have compiled clean.
#![warn(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use clap::Parser;

use orion::config;

mod cli;
mod package_cli;
mod signing_cli;

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
    orion-server migrate --wait 60s           Wait up to 60s for the database, then migrate\n    \
    orion-server lint workflow.json           Validate a workflow JSON file\n    \
    orion-server dry-run -w wf.json -i x.json Dry-run a workflow against an input\n    \
    orion-server dry-run -w wf.json -i x.json --stubs s.json   ... with canned connector replies\n    \
    orion-server test examples/workflow-tests Run a directory of workflow test cases\n    \
    orion-server test-connectivity            Probe DB (and Kafka if enabled)\n    \
    orion-server preflight                    Scan stored channels/workflows before upgrading\n    \
    orion-server dump-openapi > spec.json     Write the OpenAPI 3.1 spec to a file\n    \
    orion-server package export -s <url> --tag payments --name payments --version 1.0.0 -o pkg.json\n                                              \
Export a promotion package from an instance\n    \
    orion-server package apply -s <url> -f pkg.json  Stage, activate and reload the package on a target\n    \
    orion-server plugin sign plugins/ --key signer.pem  Sign every plugin component for [plugins.trust]\n\n\
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
        /// Directory of plugin manifests (`plugin.toml`) beyond the set's
        /// own tree, so a workflow naming a plugin function is checked
        /// against the manifest. Repeatable. Without one such a function is
        /// reported as unverifiable, never as an error.
        #[arg(long = "plugin-dir", value_name = "DIR")]
        plugin_dirs: Vec<String>,
        /// Directory of model manifests (`orion:model@` JSON) beyond the
        /// set's own tree, so a `model_infer` task naming a model by literal
        /// id is checked against a manifest. With the artifact beside a
        /// manifest, the graph's stats are reported too. Repeatable.
        #[arg(long = "model-dir", value_name = "DIR")]
        model_dirs: Vec<String>,
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
        /// Package name, e.g. payments. Required for --format artifact
        /// unless the set declares `package.name`; the flag wins when both
        /// are given.
        #[arg(long)]
        name: Option<String>,
        /// Package version, e.g. 1.4.0 — or `content` to derive it from the
        /// artifact's content hash (`content-<12 hex>`), so it moves exactly
        /// when `plan`, `apply` and `diff` would see a change. Required for
        /// --format artifact. Applied versions are immutable — any content
        /// change needs a bump.
        #[arg(long)]
        version: Option<String>,
        /// With `--version content`: `<PREFIX>-<12 hex>` instead of
        /// `content-<12 hex>`.
        #[arg(long, value_name = "PREFIX")]
        version_prefix: Option<String>,
        /// Directory of detached signatures to write into the artifact's
        /// `plugins[]` and `models[]` entries, for a build-time signer:
        /// `<id>.sig` or `<artifact file>.sig`. Signatures are not content,
        /// so the hash and a content version do not move.
        #[arg(long, value_name = "DIR")]
        signatures: Option<String>,
        /// The Orion version range the artifact requires of a target
        /// (`requires.orion`) over the set's own `package.requires.orion`, e.g. ">=1.8.2, <2". `plan` and `apply` refuse
        /// a target outside it.
        #[arg(long, value_name = "RANGE")]
        requires_orion: Option<String>,
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
        /// Directory of plugin manifests (`plugin.toml`) beyond the set's
        /// own tree. A manifest in the set compiles into the artifact with
        /// its component inlined. Repeatable.
        #[arg(long = "plugin-dir", value_name = "DIR")]
        plugin_dirs: Vec<String>,
        /// Directory of model manifests beyond the set's own tree. A
        /// manifest in the set compiles into the artifact as a `models[]`
        /// entry: its `reference` names where the bytes are for the target,
        /// and the file its `artifact` names is hashed. Repeatable.
        #[arg(long = "model-dir", value_name = "DIR")]
        model_dirs: Vec<String>,
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
        /// Directory of plugin manifests and their components. A plugin
        /// function runs for real in the sandbox, never stubbed; a workflow
        /// naming one whose component is absent fails as
        /// PLUGIN_ARTIFACT_UNAVAILABLE. Repeatable.
        #[arg(long = "plugin-dir", value_name = "DIR")]
        plugin_dirs: Vec<String>,
        /// Directory of model manifests and their artifacts. With one,
        /// `model_infer` runs the model for real, never stubbed; a workflow
        /// naming a model the directory does not hold, or holds without its
        /// artifact, fails as MODEL_ARTIFACT_UNAVAILABLE. Without one the
        /// function is answered from --stubs. Repeatable.
        #[arg(long = "model-dir", value_name = "DIR")]
        model_dirs: Vec<String>,
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
        /// Directory of plugin manifests and their components, loaded once
        /// for the whole suite. A plugin function runs for real; a case
        /// naming one whose component is absent fails as
        /// PLUGIN_ARTIFACT_UNAVAILABLE. Repeatable.
        #[arg(long = "plugin-dir", value_name = "DIR")]
        plugin_dirs: Vec<String>,
        /// Directory of model manifests and their artifacts, loaded once for
        /// the whole suite. `model_infer` runs the model for real; a case
        /// naming a model the directory does not hold fails as
        /// MODEL_ARTIFACT_UNAVAILABLE. Repeatable.
        #[arg(long = "model-dir", value_name = "DIR")]
        model_dirs: Vec<String>,
    },
    /// Digest, sign and verify plugin components — what `[plugins.trust]`
    /// checks. PATH is a `plugin.toml`, a directory of them, or any file.
    Plugin {
        #[command(subcommand)]
        command: signing_cli::SigningCommand,
    },
    /// Digest, sign and verify model artifacts — what `[models.trust]`
    /// checks. PATH is a model manifest, a directory of them, or any file.
    Model {
        #[command(subcommand)]
        command: signing_cli::SigningCommand,
    },
    /// Probe configured backends for reachability (A6).
    ///
    /// Opens the configured database pool (using the same `storage.url`)
    /// and runs a no-op query. Catches "DB credentials wrong / file
    /// unreadable" before the server tries to start.
    TestConnectivity {
        /// Keep retrying until the state database (and Kafka, when enabled)
        /// accept connections, for at most this long in total (`60`, `30s`,
        /// `5m`). Only connection failures are retried.
        #[arg(long, value_name = "DURATION", value_parser = cli::parse_wait)]
        wait: Option<std::time::Duration>,
    },
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
        /// Directory of plugin manifests beyond the set's own tree, so a
        /// plugin function's template fields are analysed as the server
        /// evaluates them. Repeatable.
        #[arg(long = "plugin-dir", value_name = "DIR")]
        plugin_dirs: Vec<String>,
        /// Directory of model manifests beyond the set's own tree, for the
        /// `lint` gate that runs first. Repeatable.
        #[arg(long = "model-dir", value_name = "DIR")]
        model_dirs: Vec<String>,
        /// Apply the fixes the rules can prove — today, folding a run of
        /// steps that repeat one condition into a task group — to the source
        /// files, each verified by recompiling the edited file, then report
        /// what remains.
        #[arg(long, conflicts_with_all = ["list", "explain"])]
        fix: bool,
        /// With --fix: print the diff of each file that would change, write
        /// nothing, and exit 1 when anything would.
        #[arg(long, requires = "fix")]
        check: bool,
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
        /// Package version, e.g. 1.4.0 — or `content` to derive it from the
        /// artifact's content hash (`content-<12 hex>`). Applied versions are
        /// immutable — any content change needs a bump.
        #[arg(long)]
        version: String,
        /// With `--version content`: `<PREFIX>-<12 hex>` instead of
        /// `content-<12 hex>`.
        #[arg(long, value_name = "PREFIX")]
        version_prefix: Option<String>,
        /// The Orion version range the artifact requires of a target
        /// (`requires.orion`), e.g. ">=1.8.2, <2". `plan` and `apply` refuse
        /// a target outside it.
        #[arg(long, value_name = "RANGE")]
        requires_orion: Option<String>,
        /// Write the artifact here instead of stdout.
        #[arg(short, long)]
        output: Option<String>,
        /// Inline each plugin's component as base64, so the artifact can
        /// install plugins on a target that has never seen them. Without it
        /// a plugin travels as manifest and digest, and the target must
        /// already hold the component.
        #[arg(long)]
        include_artifacts: bool,
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
        /// Directory of detached signatures to attach before anything is
        /// sent: `<plugin or model id>.sig` or `<artifact file>.sig` per
        /// plugin and model — base64 Ed25519 over the digest string, as
        /// `orion-server plugin sign -o <dir>` writes them. The artifact
        /// file, its version and its hash are untouched.
        #[arg(long, value_name = "DIR")]
        signatures: Option<String>,
        /// Show what `apply --prune` would remove — what the package's
        /// current applied version carried and this artifact does not —
        /// and any removal that would be refused.
        #[arg(
            long,
            value_enum,
            value_name = "MODE",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "archive"
        )]
        prune: Option<package_cli::PruneArg>,
    },
    /// Apply an artifact: claim the receipt as staged, stage all entities
    /// (connectors → workflows → channels), activate in dependency order
    /// with one engine reload at the end, then flip the receipt to applied.
    /// Idempotent — re-running an identical artifact is a no-op. With
    /// `--prune`, also remove what the previous applied version carried and
    /// this one does not, inside the same reload.
    Apply {
        /// Base URL of the target instance.
        #[arg(short, long)]
        server: String,
        /// Path to the artifact file.
        #[arg(short, long)]
        file: String,
        /// Directory of detached signatures to attach before anything is
        /// sent: `<plugin or model id>.sig` or `<artifact file>.sig` per
        /// plugin and model — base64 Ed25519 over the digest string, as
        /// `orion-server plugin sign -o <dir>` writes them. The artifact
        /// file, its version and its hash are untouched.
        #[arg(long, value_name = "DIR")]
        signatures: Option<String>,
        /// Remove what the package's current applied version carried and
        /// this artifact does not. `--prune` archives (reversible, and it
        /// frees the route or schedule; a connector is disabled);
        /// `--prune=delete` deletes. Nothing another package's current
        /// version carries is touched.
        #[arg(
            long,
            value_enum,
            value_name = "MODE",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "archive"
        )]
        prune: Option<package_cli::PruneArg>,
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
    let mut cli = Cli::parse();

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

    // The signing verbs read artifacts and keys, not a server: `-c` is only
    // consulted by `verify`, for the trust keys, and nothing else is loaded.
    let signing = match cli.command.take() {
        Some(Command::Plugin { command }) => Some((orion::signatures::Kind::Plugin, command)),
        Some(Command::Model { command }) => Some((orion::signatures::Kind::Model, command)),
        other => {
            cli.command = other;
            None
        }
    };
    if let Some((kind, command)) = signing {
        let code = signing_cli::run(kind, command, cli.config.as_deref())?;
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
        Some(Command::Migrate { dry_run, wait }) => {
            return cli::handle_migrate(&config, dry_run, wait).await;
        }
        Some(Command::Lint {
            workflow,
            deny_warnings,
            requires_channels,
            requires_connectors,
            definitions,
            plugin_dirs,
            model_dirs,
        }) => {
            let boundary = orion::definitions::Boundary {
                channels: requires_channels,
                connectors: requires_connectors,
                ..orion::definitions::Boundary::default()
            };
            return cli::run_lint(
                &workflow,
                deny_warnings,
                boundary,
                definitions.as_deref(),
                &plugin_dirs,
                &model_dirs,
            );
        }
        Some(Command::Compile {
            dir,
            output,
            format,
            name,
            version,
            version_prefix,
            signatures,
            requires_orion,
            requires_channels,
            requires_connectors,
            deny_warnings,
            no_activate,
            plugin_dirs,
            model_dirs,
        }) => {
            let boundary = orion::definitions::Boundary {
                channels: requires_channels,
                connectors: requires_connectors,
                ..orion::definitions::Boundary::default()
            };
            return cli::run_compile(cli::CompileRequest {
                dir: &dir,
                output: output.as_deref(),
                format,
                name: name.as_deref(),
                version: version.as_deref(),
                version_prefix: version_prefix.as_deref(),
                signatures: signatures.as_deref(),
                requires_orion: requires_orion.as_deref(),
                boundary,
                deny_warnings,
                no_activate,
                plugin_dirs: &plugin_dirs,
                model_dirs: &model_dirs,
            });
        }
        Some(Command::DryRun {
            workflow,
            input,
            stubs,
            metadata,
            secrets,
            definitions,
            plugin_dirs,
            model_dirs,
        }) => {
            return cli::run_dry_run(cli::DryRunRequest {
                workflow: &workflow,
                input: &input,
                stubs: stubs.as_deref(),
                metadata: metadata.as_deref(),
                secrets: secrets.as_deref(),
                definitions: definitions.as_deref(),
                plugin_dirs: &plugin_dirs,
                model_dirs: &model_dirs,
            })
            .await;
        }
        Some(Command::Test {
            path,
            definitions,
            plugin_dirs,
            model_dirs,
        }) => {
            return cli::run_test(&path, definitions.as_deref(), &plugin_dirs, &model_dirs).await;
        }
        Some(Command::TestConnectivity { wait }) => {
            return cli::run_test_connectivity(&config, wait).await;
        }
        Some(Command::Clippy {
            path,
            deny_warnings,
            format,
            list,
            explain,
            definitions,
            requires_channels,
            requires_connectors,
            plugin_dirs,
            model_dirs,
            fix,
            check,
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
                    plugin_dirs: &plugin_dirs,
                    model_dirs: &model_dirs,
                    boundary: orion::definitions::Boundary {
                        channels: requires_channels,
                        connectors: requires_connectors,
                        ..orion::definitions::Boundary::default()
                    },
                    // Only a config the operator named counts as "the serving
                    // config": the defaults say nothing about [vars]/[secrets].
                    config: cli.config.is_some().then_some(&config),
                    fix,
                    fix_check: check,
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
        Some(Command::Plugin { .. } | Command::Model { .. }) => {
            unreachable!("the signing verbs return before config is loaded")
        }
        Some(Command::Preflight) => return cli::run_preflight(&config).await,
        Some(Command::Package { command }) => {
            return match command {
                PackageCommand::Export {
                    server,
                    tag,
                    channels,
                    name,
                    version,
                    version_prefix,
                    requires_orion,
                    output,
                    include_artifacts,
                } => {
                    package_cli::run_export(
                        &server,
                        tag.as_deref(),
                        &channels,
                        &name,
                        &version,
                        version_prefix.as_deref(),
                        requires_orion.as_deref(),
                        output.as_deref(),
                        include_artifacts,
                    )
                    .await
                }
                PackageCommand::Lint { file } => package_cli::run_lint(&file),
                PackageCommand::Plan {
                    server,
                    file,
                    signatures,
                    prune,
                } => {
                    package_cli::run_plan(
                        &server,
                        &file,
                        signatures.as_deref(),
                        prune.map(Into::into),
                    )
                    .await
                }
                PackageCommand::Apply {
                    server,
                    file,
                    signatures,
                    prune,
                } => {
                    package_cli::run_apply(
                        &server,
                        &file,
                        signatures.as_deref(),
                        prune.map(Into::into),
                    )
                    .await
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

    // Builds the channel half of every generation. Cluster mode swaps in the
    // strict backend matrix.
    let channel_loader = Arc::new(if config.cluster.enabled {
        orion::channel::ChannelLoader::with_cluster((&*cluster).into())
    } else {
        orion::channel::ChannelLoader::new()
    });

    // Connector registry, shared HTTP client, runtime handle, cache pools,
    // custom function handlers, and the Kafka producer (see
    // `bootstrap::build_engine_components`).
    let components = bootstrap::build_engine_components(&config, &repos).await?;

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

    // Load active channels and workflows, build both halves of the first
    // generation, and publish it through the pre-created runtime handle.
    // Channels that fail to load are quarantined — refused at every ingress
    // until fixed. Consumes `components`: the handler map goes into the
    // engine, and what comes back is the half that backs `AppState` (F55).
    let (components, channels, active_workflow_count) = components
        .load_channels_and_build_engine(&config, &repos, &channel_loader)
        .await?;

    // Mark the service as ready now that the first generation is published
    ready.store(true, std::sync::atomic::Ordering::Release);

    // Start the background tasks: trace persistence queue, trace queue
    // worker pool (with DLQ for failed async traces), trace + audit-log
    // cleanup, and the DLQ retry consumer.
    // One supervisor for every long-lived background task. It goes onto
    // `AppState` so `/health` and `/readyz` can report their liveness, and
    // `main` keeps its own handle so shutdown can stop them.
    let tasks = Arc::new(orion::runtime::TaskRegistry::new());
    // Shared with `AppState` so `/health` can report what the two scheduler
    // loops are actually achieving, which their liveness does not say.
    let cron_status = Arc::new(orion::cron::CronStatus::new());
    let (trace_persistence_queue, trace_queue, audit_queue, task_handles) =
        bootstrap::start_background_tasks(
            &config,
            &tasks,
            components.runtime.clone(),
            &repos,
            &cluster,
            bootstrap::CronComponents {
                datalogic: components.datalogic.clone(),
                vars: components.vars.clone(),
                status: cron_status.clone(),
            },
        );

    // The plugin epoch ticker: the clock every plugin deadline is measured
    // in, supervised as `Required` so a dead ticker degrades `/readyz`
    // rather than silently disabling every deadline.
    if let Some(sandbox) = &components.plugins {
        orion::plugin::ticker::start(&tasks, sandbox.clone());
    }

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
            runtime: components.runtime.clone(),
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
        channel_loader,
        trace_queue,
        trace_persistence_queue,
        audit_queue,
        rate_limit_state,
        metrics_handle,
        ready: ready.clone(),
        kafka_consumer_handle,
        cluster,
        tasks: tasks.clone(),
        cron_status: cron_status.clone(),
    });

    // The model admission worker: drains the queue the model routes fill,
    // fetching and verifying each artifact and recording the verdict on the
    // row. Only with `models.enabled`; supervised as `Required` because a
    // dead worker leaves every registration pending forever.
    if state.models.is_some() {
        orion::runtime::model_admission::start(&tasks, state.clone());
    }

    // Cluster background tasks (epoch watcher). None when disabled.
    orion::cluster::start_cluster_tasks(&state);

    // `[packages] apply`: applied now that everything an apply needs is
    // running — the audit writer, the model admission worker, the epoch
    // watcher — and concurrently with serving, so `/healthz` answers during
    // a long admission while `/readyz` holds at 503 until they serve.
    let packages = state.packages.clone();
    tokio::spawn(orion::package::boot::run(state.clone()));

    let router = orion::server::build_router(state.clone());

    // Optional dedicated metrics listener (O12), bound before the main server
    // starts (see `bootstrap::start_metrics_listener`).
    let metrics_server = bootstrap::start_metrics_listener(&config, &state)?;

    let serve = async {
        if config.server.tls.enabled {
            let handle = axum_server::Handle::new();
            orion::server::serve::serve_tls(
                config.clone(),
                ready.clone(),
                router,
                handle,
                orion::server::shutdown_signal(),
            )
            .await
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
            .await
        }
    };
    tokio::select! {
        served = serve => served?,
        // A package that failed to apply: the node was never ready, so no
        // load balancer routes here and the drain grace would only delay the
        // restart. Stop serving at once and shut the rest down cleanly.
        () = packages.failed() => {
            ready.store(false, std::sync::atomic::Ordering::Release);
        }
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

    // The exit status an orchestrator restarts on.
    if let Some(failure) = packages.failure() {
        return Err(failure.into());
    }

    tracing::info!("Orion shut down cleanly");
    Ok(())
}

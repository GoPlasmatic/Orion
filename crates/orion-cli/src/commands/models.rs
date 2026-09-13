use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::{STATUS_ACTIVE, STATUS_ARCHIVED};
use serde_json::{Value, json};
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils::{self, colorize_status, truncate};
use orion_client::paths;

/// What a model is, in endpoint terms.
static KIND: utils::EntityKind = utils::EntityKind {
    title: "Model",
    label: "model",
    collection: paths::MODELS,
    export: paths::MODELS_EXPORT,
    validate: paths::MODELS_VALIDATE,
    item: paths::model,
    id_field: "model_id",
};

static VERSIONED: utils::VersionedEntityKind = utils::VersionedEntityKind {
    entity: &KIND,
    status: paths::model_status,
    versions: paths::model_versions,
};

/// The admission states a model version moves through. `pending` is the
/// only one that is not a verdict: `--wait` polls until the state is
/// anything else.
const ADMISSION_PENDING: &str = "pending";
const ADMISSION_PASSED: &str = "passed";
const ADMISSION_FAILED: &str = "failed";

#[derive(Args)]
#[command(
    long_about = "Manage models -- ONNX artifacts in object storage, named by connector, key and digest.\n\n\
        Lifecycle: register -> admission (pending -> passed | failed) -> activate -> live\n\
        A model is registered from its manifest (JSON) and a reference to the artifact; the bytes \
        never travel through the CLI. Registration answers at once and a node admits the version \
        asynchronously -- fetches the object, verifies the digest, parses the graph and runs one \
        probe inference. Activation needs a passed admission; archiving is refused while an active \
        workflow names the model.\n\n\
        With --quiet, list prints one ID per line, get prints the ID, and mutating commands print the resource ID or suppress output."
)]
pub struct ModelsCmd {
    #[command(subcommand)]
    command: ModelsSubcommand,
}

/// Where the artifact is: the three fields a registration names it by.
/// Flattened into `create` and `validate`, where all three are required;
/// `update` declares its own optional trio because `PUT` keeps what is
/// absent.
#[derive(Args, Clone)]
struct ArtifactArgs {
    /// Object-storage connector the artifact is read through
    #[arg(long)]
    connector: String,
    /// Object key within that connector's bucket
    #[arg(long)]
    key: String,
    /// `sha256:<hex>` of the artifact bytes; admission confirms it
    #[arg(long)]
    digest: String,
}

/// Whether to follow the admission a `202` starts, and for how long.
#[derive(Args, Clone, Copy)]
struct WaitArgs {
    /// Poll until admission passes or fails (exit 1 on failed, 2 on timeout)
    #[arg(long)]
    wait: bool,
    /// Seconds to keep polling with --wait; the default is the server's own
    /// `models.admission_timeout_secs` default, after which it records a
    /// verdict itself
    #[arg(long, default_value = "900")]
    timeout: u64,
    /// Seconds between polls while --wait is waiting
    #[arg(long, default_value = "2")]
    interval: u64,
}

#[derive(Subcommand)]
enum ModelsSubcommand {
    /// List all models
    #[command(
        after_help = "The server pages at 50 by default; raise it with --limit (max 1000)\n\
            or walk pages with --offset.\n\n\
            Examples:\n  \
            orion-cli models list\n  \
            orion-cli models list --status active\n  \
            orion-cli models list --admission failed\n  \
            orion-cli models list --tag scoring --limit 200"
    )]
    List {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Filter by admission state (pending, passed, failed)
        #[arg(long)]
        admission: Option<String>,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
        /// Sort by column (model_id, status, created_at, updated_at)
        #[arg(long)]
        sort_by: Option<String>,
        /// Sort direction (asc, desc)
        #[arg(long)]
        sort_order: Option<String>,
    },
    /// Get a model by ID, with its admission verdict and this node's residency (use --verbose for the manifest)
    Get {
        /// Model ID
        id: String,
    },
    /// Register a model from its manifest and the artifact's place in object storage
    #[command(after_help = crate::help::MODEL_CREATE)]
    Create {
        /// Path to the model manifest (JSON)
        #[arg(short, long)]
        file: String,
        #[command(flatten)]
        artifact: ArtifactArgs,
        /// Path to a file holding the base64 Ed25519 signature over the
        /// artifact digest — required by a server with `[models.trust]` keys
        #[arg(long, value_name = "PATH")]
        signature: Option<String>,
        /// Selection labels, repeatable
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Replace a draft model's manifest, artifact reference, signature or tags
    #[command(
        after_help = "Only what is named changes; PUT keeps the rest. The three reference flags\n\
            name one artifact and travel together.\n\n\
            Examples:\n  \
            orion-cli models update acme.fraud -f model.json\n  \
            orion-cli models update acme.fraud --connector models-bucket --key fraud/v4.onnx --digest sha256:...\n  \
            orion-cli models update acme.fraud --tag scoring --tag fraud"
    )]
    Update {
        /// Model ID
        id: String,
        /// Path to the model manifest (JSON)
        #[arg(short, long)]
        file: Option<String>,
        /// Object-storage connector the artifact is read through
        #[arg(long)]
        connector: Option<String>,
        /// Object key within that connector's bucket
        #[arg(long)]
        key: Option<String>,
        /// `sha256:<hex>` of the artifact bytes
        #[arg(long)]
        digest: Option<String>,
        /// Path to a file holding the base64 Ed25519 signature over the new
        /// digest; the stored one is kept when the digest is unchanged
        #[arg(long, value_name = "PATH")]
        signature: Option<String>,
        /// Selection labels, repeatable (replaces the stored tags when given)
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Delete a model (prompts for confirmation)
    Delete {
        /// Model ID
        id: String,
    },
    /// Activate an admitted draft model (the engine reloads automatically)
    Activate {
        /// Model ID
        id: String,
        /// Pre-flight only: report whether activation would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Archive an active model (refused while an active workflow names it)
    Archive {
        /// Model ID
        id: String,
        /// Pre-flight only: report whether archiving would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Run admission again: fetch, verify and probe the artifact once more
    Admit {
        /// Model ID
        id: String,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Show what depends on a model: the active workflows naming it
    #[command(alias = "deps")]
    Dependencies {
        /// Model ID
        id: String,
    },
    /// Validate a manifest and artifact reference without registering them
    Validate {
        /// Path to the model manifest (JSON)
        #[arg(short, long)]
        file: String,
        #[command(flatten)]
        artifact: ArtifactArgs,
        /// Path to a file holding the base64 Ed25519 signature over the
        /// artifact digest, when the server requires one
        #[arg(long, value_name = "PATH")]
        signature: Option<String>,
    },
    /// List version history for a model
    Versions {
        /// Model ID
        id: String,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Create a new draft version of a model
    NewVersion {
        /// Model ID
        id: String,
    },
    /// Export models as JSON (pipe to file for backup or promotion)
    #[command(
        after_help = "The export carries the manifest and the artifact reference, never the bytes:\n\
            the target reads them from its own storage connector of the same name.\n\n\
            Examples:\n  orion-cli models export > models.json\n  orion-cli models export --status active > active-models.json"
    )]
    Export {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Import models from a JSON array file
    #[command(
        after_help = "Examples:\n  orion-cli models import -f models.json --dry-run\n  orion-cli models import -f models.json --on-conflict new_version"
    )]
    Import {
        /// Path to JSON file containing a models array
        #[arg(short, long)]
        file: String,
        /// Preview what would be imported without making changes
        #[arg(long)]
        dry_run: bool,
        /// On ID conflict: fail (default), skip the item, or write a new version
        #[arg(long, value_parser = ["fail", "skip", "new_version"])]
        on_conflict: Option<String>,
    },
}

#[derive(Tabled)]
struct ModelRow {
    #[tabled(rename = "ID")]
    model_id: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Admission")]
    admission: String,
    #[tabled(rename = "Params")]
    parameters: String,
    #[tabled(rename = "Digest")]
    digest: String,
}

impl ModelsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            ModelsSubcommand::List {
                status,
                tag,
                admission,
                limit,
                offset,
                sort_by,
                sort_order,
            } => {
                let qs = utils::build_query_string(&[
                    ("status", status.clone()),
                    ("tag", tag.clone()),
                    ("admission", admission.clone()),
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                    ("sort_by", sort_by.clone()),
                    ("sort_order", sort_order.clone()),
                ]);
                list(client, format, quiet, &qs).await
            }
            ModelsSubcommand::Get { id } => get_model(client, format, quiet, verbose, id).await,
            ModelsSubcommand::Create {
                file,
                artifact,
                signature,
                tags,
                wait,
            } => {
                let body = registration_body(file, artifact, signature.as_deref(), tags)?;
                register(client, format, quiet, &body, *wait).await
            }
            ModelsSubcommand::Update {
                id,
                file,
                connector,
                key,
                digest,
                signature,
                tags,
            } => {
                let body = update_body(UpdateFields {
                    manifest: file.as_deref(),
                    connector: connector.as_deref(),
                    key: key.as_deref(),
                    digest: digest.as_deref(),
                    signature: signature.as_deref(),
                    tags,
                })?;
                utils::update_entity(client, &KIND, format, quiet, id, &body).await
            }
            ModelsSubcommand::Delete { id } => {
                utils::delete_entity(client, &KIND, quiet, yes, id).await
            }
            ModelsSubcommand::Activate {
                id,
                dry_run,
                defer_reload,
            } => {
                utils::change_status(
                    client,
                    &VERSIONED,
                    format,
                    quiet,
                    utils::StatusChange {
                        id,
                        status: STATUS_ACTIVE,
                        dry_run: *dry_run,
                        defer_reload: *defer_reload,
                    },
                )
                .await
            }
            ModelsSubcommand::Archive {
                id,
                dry_run,
                defer_reload,
            } => {
                utils::change_status(
                    client,
                    &VERSIONED,
                    format,
                    quiet,
                    utils::StatusChange {
                        id,
                        status: STATUS_ARCHIVED,
                        dry_run: *dry_run,
                        defer_reload: *defer_reload,
                    },
                )
                .await
            }
            ModelsSubcommand::Admit { id, wait } => admit(client, format, quiet, id, *wait).await,
            ModelsSubcommand::Dependencies { id } => dependencies(client, format, quiet, id).await,
            ModelsSubcommand::Validate {
                file,
                artifact,
                signature,
            } => {
                let body = registration_body(file, artifact, signature.as_deref(), &[])?;
                utils::validate_entity(client, &KIND, format, quiet, &body).await
            }
            ModelsSubcommand::Versions { id, limit, offset } => {
                let qs = utils::build_query_string(&[
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                ]);
                utils::list_versions(client, &VERSIONED, format, quiet, id, &qs).await
            }
            ModelsSubcommand::NewVersion { id } => {
                utils::create_version(client, &VERSIONED, format, quiet, id).await
            }
            ModelsSubcommand::Export { status, tag } => {
                let qs =
                    utils::build_query_string(&[("status", status.clone()), ("tag", tag.clone())]);
                utils::export_entities(client, &KIND, &qs).await
            }
            ModelsSubcommand::Import {
                file,
                dry_run,
                on_conflict,
            } => {
                utils::run_import(
                    client,
                    format,
                    quiet,
                    utils::ImportRequest {
                        base_path: paths::MODELS_IMPORT,
                        label: "model",
                        file,
                        dry_run: *dry_run,
                        on_conflict: on_conflict.as_deref(),
                    },
                )
                .await
            }
        }
    }
}

/// The artifact reference as the API spells it.
fn artifact_ref(connector: &str, key: &str, digest: &str) -> Value {
    json!({ "connector": connector, "key": key, "digest": digest })
}

/// The manifest file: a JSON object. It is sent as parsed, so a file that is
/// not JSON is refused here with the path named, rather than by the server
/// as an opaque body error.
fn read_manifest(path: &str) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading manifest '{path}'"))?;
    let manifest: Value =
        serde_json::from_str(&text).with_context(|| format!("'{path}' is not valid JSON"))?;
    if !manifest.is_object() {
        bail!("'{path}' must hold a JSON object (the model manifest)");
    }
    Ok(manifest)
}

/// The signature file holds the base64 text a signing tool wrote — read as
/// text and trimmed, so a trailing newline is not part of the value.
fn read_signature(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("reading signature '{path}'"))
        .map(|s| s.trim().to_string())
}

/// The request body for a registration — `POST /models` and
/// `POST /models/validate` take the same shape: the manifest as parsed, the
/// artifact reference, the tags, and the signature when one was given.
fn registration_body(
    manifest_path: &str,
    artifact: &ArtifactArgs,
    signature: Option<&str>,
    tags: &[String],
) -> Result<Value> {
    let mut body = json!({
        "manifest": read_manifest(manifest_path)?,
        "artifact": artifact_ref(&artifact.connector, &artifact.key, &artifact.digest),
        "tags": tags,
    });
    if let Some(path) = signature {
        body["signature"] = json!(read_signature(path)?);
    }
    Ok(body)
}

/// What an `update` may change. Every field is optional because `PUT`
/// keeps what is absent — but the three reference fields are one value
/// (a partial reference would name no object), so they are all-or-nothing.
struct UpdateFields<'a> {
    manifest: Option<&'a str>,
    connector: Option<&'a str>,
    key: Option<&'a str>,
    digest: Option<&'a str>,
    signature: Option<&'a str>,
    tags: &'a [String],
}

fn update_body(fields: UpdateFields<'_>) -> Result<Value> {
    let mut body = serde_json::Map::new();
    if let Some(path) = fields.manifest {
        body.insert("manifest".into(), read_manifest(path)?);
    }
    match (fields.connector, fields.key, fields.digest) {
        (Some(connector), Some(key), Some(digest)) => {
            body.insert("artifact".into(), artifact_ref(connector, key, digest));
        }
        (None, None, None) => {}
        _ => bail!("--connector, --key and --digest name one artifact and travel together"),
    }
    if let Some(path) = fields.signature {
        body.insert("signature".into(), json!(read_signature(path)?));
    }
    if !fields.tags.is_empty() {
        body.insert("tags".into(), json!(fields.tags));
    }
    if body.is_empty() {
        bail!(
            "nothing to update: pass -f <manifest>, --connector/--key/--digest, --signature or --tag"
        );
    }
    Ok(Value::Object(body))
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool, qs: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", paths::MODELS)).await?;
    let items = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for m in &items {
            if let Some(id) = m["model_id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    if items.is_empty() {
        println!("{}", "No models found.".dimmed());
        return Ok(0);
    }
    let rows: Vec<ModelRow> = items.iter().map(model_row).collect();
    output::print_table(rows);
    utils::print_list_footer(&resp, items.len(), "model");
    Ok(0)
}

fn model_row(m: &Value) -> ModelRow {
    ModelRow {
        model_id: m["model_id"].as_str().unwrap_or("").to_string(),
        version: m["version"].as_i64().unwrap_or(0),
        status: colorize_status(m["status"].as_str().unwrap_or("")),
        admission: colorize_admission(m["admission"]["state"].as_str().unwrap_or("")),
        // `stats` is `null` until admission passes; the column stays blank
        // rather than reading `0` as "a model with no parameters".
        parameters: m["stats"]["parameters"]
            .as_u64()
            .map(group_digits)
            .unwrap_or_default(),
        digest: truncate(m["digest"].as_str().unwrap_or(""), 19),
    }
}

/// `POST /models` — register a version and, with `--wait`, follow its
/// admission to a verdict.
///
/// Not `utils::create_entity`: a registration is a `202` whose one
/// interesting fact is that admission is now pending, and `--wait` needs the
/// id the response carries. The generic line names a `name` a model does
/// not have.
async fn register(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
    wait: WaitArgs,
) -> Result<i32> {
    let resp: Value = client.post(paths::MODELS, body).await?;
    acknowledge(&resp, format, quiet, wait.wait, "registered")?;
    if !wait.wait {
        return Ok(0);
    }
    let id = resp["data"]["model_id"].as_str().unwrap_or("").to_string();
    wait_for_admission(client, format, quiet, &id, wait).await
}

/// `POST /models/{id}/admit` — run admission again, optionally following it.
async fn admit(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    wait: WaitArgs,
) -> Result<i32> {
    let resp: Value = client.post_empty(&paths::model_admit(id)).await?;
    acknowledge(&resp, format, quiet, wait.wait, "admission restarted")?;
    if !wait.wait {
        return Ok(0);
    }
    wait_for_admission(client, format, quiet, id, wait).await
}

/// The one-line acknowledgement of a `202`: which model, which version, and
/// where its admission stands. With `--wait` the human line still prints
/// (the verdict follows it), but a machine-readable format prints nothing
/// yet — the final document is then the whole of the output, as `send
/// --wait` prints only the trace.
fn acknowledge(
    resp: &Value,
    format: &OutputFormat,
    quiet: bool,
    waiting: bool,
    verb: &str,
) -> Result<()> {
    let m = &resp["data"];
    let id = m["model_id"].as_str().unwrap_or("");
    if quiet {
        println!("{id}");
        return Ok(());
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        if !waiting {
            output::print_value(format, resp)?;
        }
        return Ok(());
    }
    println!(
        "{} Model {id} {verb} (v{}, admission {})",
        "OK".green().bold(),
        m["version"],
        colorize_admission(m["admission"]["state"].as_str().unwrap_or(""))
    );
    Ok(())
}

/// The last document a wait saw, and whether the wait ended on a verdict or
/// on the clock.
struct Polled {
    model: Value,
    timed_out: bool,
}

impl Polled {
    fn state(&self) -> &str {
        self.model["admission"]["state"].as_str().unwrap_or("")
    }

    /// **Exit 0 passed, 1 failed, 2 timed out** — the codes `traces wait`
    /// uses, so a script can tell "the artifact was refused" from "the
    /// server is still fetching it". A state that is neither pending nor
    /// passed is a refusal, whatever it is called.
    fn exit_code(&self) -> i32 {
        if self.timed_out {
            2
        } else if self.state() == ADMISSION_PASSED {
            0
        } else {
            1
        }
    }
}

/// Fetch the model until `admission.state` is anything but `pending`, or
/// `timeout` elapses. `on_state` sees every state read, so a caller can
/// report progress; the fetch is a closure so the loop is testable against
/// a canned sequence without a server.
async fn poll_admission<F, Fut>(
    mut fetch: F,
    interval: Duration,
    timeout: Duration,
    mut on_state: impl FnMut(&str),
) -> Result<Polled>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let start = Instant::now();
    loop {
        let model = fetch().await?;
        let state = model["admission"]["state"]
            .as_str()
            .unwrap_or("")
            .to_string();
        on_state(&state);
        if state != ADMISSION_PENDING {
            return Ok(Polled {
                model,
                timed_out: false,
            });
        }
        if start.elapsed() >= timeout {
            return Ok(Polled {
                model,
                timed_out: true,
            });
        }
        tokio::time::sleep(interval).await;
    }
}

/// Poll `GET /models/{id}` to a verdict and render it.
///
/// Progress goes to stderr, so `--output json` piped to a parser stays clean
/// while a human watching the terminal still sees each state change.
async fn wait_for_admission(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    opts: WaitArgs,
) -> Result<i32> {
    let path = paths::model(id);
    let path = path.as_str();
    let narrate = !quiet && matches!(format, OutputFormat::Table);
    let mut last_seen = String::new();
    if narrate {
        eprint!("Waiting for admission of {id}:");
    }
    let polled = poll_admission(
        || async move {
            let resp: Value = client.get(path).await?;
            Ok::<Value, anyhow::Error>(resp.get("data").cloned().unwrap_or(resp))
        },
        Duration::from_secs(opts.interval),
        Duration::from_secs(opts.timeout),
        |state| {
            if narrate && state != last_seen {
                eprint!(" {state}");
                last_seen = state.to_string();
            }
        },
    )
    .await?;
    if narrate {
        eprintln!();
    }

    let code = polled.exit_code();
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &polled.model)?;
        return Ok(code);
    }
    if quiet {
        return Ok(code);
    }

    let m = &polled.model;
    if polled.timed_out {
        println!(
            "{} Admission still {} after {}s -- 'models get {id}' shows the verdict when it lands",
            "TIMEOUT".yellow().bold(),
            polled.state(),
            opts.timeout
        );
    } else if code == 0 {
        let stats = m
            .get("stats")
            .filter(|s| !s.is_null())
            .map(|s| format!(" ({})", describe_stats(s)))
            .unwrap_or_default();
        println!("{} Admission passed{stats}", "OK".green().bold());
    } else {
        println!(
            "{} Admission {}",
            "ERR".red().bold(),
            describe_admission(&m["admission"])
        );
    }
    Ok(code)
}

async fn get_model(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    verbose: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::model(id)).await?;
    let m = &resp["data"];
    if quiet {
        println!("{}", m["model_id"].as_str().unwrap_or(id));
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    println!("{}: {}", "ID".bold(), m["model_id"].as_str().unwrap_or(""));
    println!(
        "{}: {} (manifest {})",
        "Version".bold(),
        m["version"],
        m["model_version"].as_str().unwrap_or("")
    );
    println!(
        "{}: {}",
        "Status".bold(),
        colorize_status(m["status"].as_str().unwrap_or(""))
    );
    // The verdict is what a reader of a fresh registration came for, so it
    // sits above the identity lines rather than among them.
    println!(
        "{}: {}",
        "Admission".bold(),
        describe_admission(&m["admission"])
    );
    println!(
        "{}: {} ({})",
        "Format".bold(),
        m["format"].as_str().unwrap_or(""),
        m["abi"].as_str().unwrap_or("")
    );
    println!(
        "{}: {}",
        "Digest".bold(),
        m["digest"].as_str().unwrap_or("")
    );
    let artifact = &m["artifact"];
    let size = artifact["size"]
        .as_u64()
        .map(|b| format!(" ({} bytes)", group_digits(b)))
        .unwrap_or_default();
    println!(
        "{}: {}/{}{size}",
        "Artifact".bold(),
        artifact["connector"].as_str().unwrap_or(""),
        artifact["key"].as_str().unwrap_or("")
    );
    println!("{}: {}", "Inputs".bold(), names(&m["inputs"]));
    println!("{}: {}", "Outputs".bold(), names(&m["outputs"]));
    if let Some(stats) = m.get("stats").filter(|s| !s.is_null()) {
        println!("{}: {}", "Stats".bold(), describe_stats(stats));
    }
    if let Some(health) = m.get("health").filter(|h| !h.is_null()) {
        let state = health["state"].as_str().unwrap_or("");
        let detail = match state {
            "loaded" => {
                let mut parts = Vec::new();
                if let Some(runtime) = health["runtime"].as_str() {
                    let device = health["device"]
                        .as_str()
                        .map(|d| format!(" on {d}"))
                        .unwrap_or_default();
                    parts.push(format!("{runtime}{device}"));
                }
                if let Some(bytes) = health["resident_bytes"].as_u64() {
                    parts.push(format!("{} bytes resident", group_digits(bytes)));
                }
                if parts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", parts.join(", "))
                }
            }
            "failed" => health["reason"]
                .as_str()
                .map(|r| format!(": {r}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let coloured = match state {
            "loaded" => state.green().to_string(),
            "failed" => state.red().to_string(),
            other => other.yellow().to_string(),
        };
        println!("{}: {coloured}{detail}", "Health".bold());
    }
    if let Some(tags) = m["tags"].as_array().filter(|t| !t.is_empty()) {
        let tags: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
        println!("{}: {}", "Tags".bold(), tags.join(", "));
    }
    if verbose {
        println!("{}:", "Manifest".bold());
        println!("{}", serde_json::to_string_pretty(&m["manifest"])?);
    }
    Ok(0)
}

async fn dependencies(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::model_dependencies(id)).await?;
    let d = &resp["data"];
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    let workflows = d["workflows"].as_array().cloned().unwrap_or_default();
    if quiet {
        for w in &workflows {
            if let Some(id) = w["workflow_id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }
    println!(
        "{}: {} v{}",
        "Model".bold(),
        d["model_id"].as_str().unwrap_or(id),
        d["version"]
    );
    if workflows.is_empty() {
        println!(
            "{}: {}",
            "Active workflows naming it".bold(),
            "none".dimmed()
        );
    } else {
        println!("{}:", "Active workflows naming it".bold());
        for w in &workflows {
            let tasks = w["task_ids"]
                .as_array()
                .map(|t| {
                    t.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|t| !t.is_empty())
                .map(|t| format!(" (tasks: {t})"))
                .unwrap_or_default();
            println!(
                "  {} v{}{tasks}",
                w["workflow_id"].as_str().unwrap_or(""),
                w["version"]
            );
        }
    }
    if d["dynamic_references_unlisted"].as_bool().unwrap_or(false) {
        println!(
            "{}",
            "A workflow that names its model from an expression is not listed.".dimmed()
        );
    }
    Ok(0)
}

/// The tensor names an `inputs`/`outputs` array carries, comma-joined.
fn names(list: &Value) -> String {
    list.as_array()
        .map(|l| {
            l.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".dimmed().to_string())
}

fn colorize_admission(state: &str) -> String {
    match state {
        ADMISSION_PASSED => state.green().to_string(),
        ADMISSION_FAILED => state.red().to_string(),
        ADMISSION_PENDING => state.yellow().to_string(),
        other => other.to_string(),
    }
}

/// One line for a verdict: `passed (node n1, 2026-…)`, `failed at fetch:
/// <reason>`, or the bare state.
fn describe_admission(admission: &Value) -> String {
    let state = admission["state"].as_str().unwrap_or("unknown");
    let mut line = colorize_admission(state);
    match state {
        ADMISSION_FAILED => {
            if let Some(stage) = admission["stage"].as_str() {
                line.push_str(&format!(" at {stage}"));
            }
            if let Some(reason) = admission["reason"].as_str() {
                line.push_str(&format!(": {reason}"));
            }
        }
        ADMISSION_PASSED => {
            let mut context = Vec::new();
            if let Some(node) = admission["node"].as_str() {
                context.push(format!("node {node}"));
            }
            if let Some(at) = admission["at"].as_str() {
                context.push(at.to_string());
            }
            if !context.is_empty() {
                line.push_str(&format!(" ({})", context.join(", ")));
            }
        }
        _ => {}
    }
    line
}

/// What admission read out of the graph, on one line.
fn describe_stats(stats: &Value) -> String {
    let mut parts = vec![
        format!(
            "{} parameters",
            group_digits(stats["parameters"].as_u64().unwrap_or(0))
        ),
        format!(
            "{} nodes",
            group_digits(stats["nodes"].as_u64().unwrap_or(0))
        ),
    ];
    if let Some(ir) = stats["ir_version"].as_i64() {
        parts.push(format!("IR {ir}"));
    }
    if let Some(opset) = stats["opset"].as_i64() {
        parts.push(format!("opset {opset}"));
    }
    if let Some(ms) = stats["probe_ms"].as_f64() {
        let on = match (stats["runtime"].as_str(), stats["device"].as_str()) {
            (Some(runtime), Some(device)) => format!(" on {runtime}/{device}"),
            (Some(runtime), None) => format!(" on {runtime}"),
            _ => String::new(),
        };
        parts.push(format!("probe {ms:.0} ms{on}"));
    }
    parts.join(", ")
}

/// `1234567` → `1,234,567`: a parameter count is read at a glance by its
/// magnitude, and eight bare digits do not give one.
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use clap::Parser;

    use super::*;

    /// Every `paths::` item fn has the same type, so a static wired to the
    /// wrong entity's family compiles. Asserted against literal URLs.
    #[test]
    fn the_model_kind_points_at_the_model_endpoints() {
        assert_eq!(KIND.collection, "/api/v1/admin/models");
        assert_eq!(KIND.export, "/api/v1/admin/models/export");
        assert_eq!(KIND.validate, "/api/v1/admin/models/validate");
        assert_eq!((KIND.item)("acme.fraud"), "/api/v1/admin/models/acme.fraud");
        assert_eq!(KIND.id_field, "model_id");
        assert_eq!(
            (VERSIONED.status)("acme.fraud"),
            "/api/v1/admin/models/acme.fraud/status"
        );
        assert_eq!(
            (VERSIONED.versions)("acme.fraud"),
            "/api/v1/admin/models/acme.fraud/versions"
        );
    }

    /// A canned `ModelResponse` as the server would answer it before and
    /// after admission. Both deserialize into the wire type, so a drift
    /// between what these tests render and what the server sends is caught
    /// at the contract, not by a reader of the table.
    fn pending_model() -> Value {
        json!({
            "model_id": "acme.fraud",
            "version": 1,
            "status": "draft",
            "digest": "sha256:9f1c0a3b7e5d2c4f6a8b0d1e3f5a7c9b2d4f6e8a0c2b4d6f8a0c2e4f6a8b0d1e3f",
            "abi": "orion:model@1.0.0",
            "model_version": "3.0.0",
            "format": "onnx",
            "manifest": {"abi": "orion:model@1.0.0", "name": "acme.fraud", "version": "3.0.0"},
            "inputs": ["features"],
            "outputs": ["score"],
            "artifact": {"connector": "models-bucket", "key": "fraud/v3.onnx", "digest": "sha256:9f1c"},
            "admission": {"state": "pending"},
            "stats": null,
            "tags": ["scoring"],
            "content_hash": "sha256:abc",
            "created_at": "2026-09-13T10:00:00",
            "updated_at": "2026-09-13T10:00:00"
        })
    }

    fn passed_model() -> Value {
        let mut m = pending_model();
        m["admission"] = json!({"state": "passed", "at": "2026-09-13T10:00:05", "node": "node-1"});
        m["stats"] = json!({
            "parameters": 1234567, "nodes": 240, "artifact_bytes": 4096, "probe_ms": 12.4,
            "ir_version": 9, "opset": 17, "runtime": "onnxruntime", "device": "cpu"
        });
        m
    }

    fn failed_model() -> Value {
        let mut m = pending_model();
        m["admission"] = json!({
            "state": "failed", "at": "2026-09-13T10:00:05", "node": "node-1",
            "stage": "fetch", "reason": "object not found"
        });
        m
    }

    #[test]
    fn the_canned_documents_are_the_wire_type() {
        for doc in [pending_model(), passed_model(), failed_model()] {
            let parsed: orion_api::dto::ModelResponse =
                serde_json::from_value(doc).expect("a ModelResponse");
            assert_eq!(parsed.model_id, "acme.fraud");
        }
    }

    /// The manifest goes as parsed JSON, the reference as one object, the
    /// signature file trimmed — and a manifest that is not an object is
    /// refused with its path named.
    #[test]
    fn a_registration_carries_the_manifest_the_reference_and_the_signature() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("model.json");
        std::fs::write(
            &manifest,
            r#"{"abi": "orion:model@1.0.0", "name": "acme.fraud", "version": "3.0.0"}"#,
        )
        .expect("write");
        let signature = dir.path().join("model.sig");
        std::fs::write(&signature, "c2lnbmF0dXJl\n").expect("write");
        let artifact = ArtifactArgs {
            connector: "models-bucket".into(),
            key: "fraud/v3.onnx".into(),
            digest: "sha256:9f1c".into(),
        };

        let body = registration_body(
            manifest.to_str().expect("utf8"),
            &artifact,
            Some(signature.to_str().expect("utf8")),
            &["scoring".to_string()],
        )
        .expect("body");
        assert_eq!(body["manifest"]["name"], "acme.fraud");
        assert_eq!(
            body["artifact"],
            json!({"connector": "models-bucket", "key": "fraud/v3.onnx", "digest": "sha256:9f1c"})
        );
        assert_eq!(body["signature"], "c2lnbmF0dXJl");
        assert_eq!(body["tags"], json!(["scoring"]));

        let bare = registration_body(manifest.to_str().expect("utf8"), &artifact, None, &[])
            .expect("body");
        assert!(bare.get("signature").is_none());
        assert_eq!(bare["tags"], json!([]));

        std::fs::write(&manifest, "[1, 2]").expect("write");
        let err = registration_body(manifest.to_str().expect("utf8"), &artifact, None, &[])
            .expect_err("not an object");
        assert!(err.to_string().contains("JSON object"), "{err}");
    }

    /// `PUT` keeps what is absent, so the body carries only what was named;
    /// a partial reference names no object and is refused before a request
    /// goes out, as is an update that would change nothing.
    #[test]
    fn an_update_sends_only_what_was_named() {
        let tags = ["scoring".to_string()];
        let only_tags = update_body(UpdateFields {
            manifest: None,
            connector: None,
            key: None,
            digest: None,
            signature: None,
            tags: &tags,
        })
        .expect("body");
        assert_eq!(only_tags, json!({"tags": ["scoring"]}));

        let reference = update_body(UpdateFields {
            manifest: None,
            connector: Some("models-bucket"),
            key: Some("fraud/v4.onnx"),
            digest: Some("sha256:aa"),
            signature: None,
            tags: &[],
        })
        .expect("body");
        assert_eq!(
            reference,
            json!({"artifact": {"connector": "models-bucket", "key": "fraud/v4.onnx", "digest": "sha256:aa"}})
        );

        let partial = update_body(UpdateFields {
            manifest: None,
            connector: None,
            key: Some("fraud/v4.onnx"),
            digest: None,
            signature: None,
            tags: &[],
        })
        .expect_err("partial reference");
        assert!(partial.to_string().contains("travel together"), "{partial}");

        let nothing = update_body(UpdateFields {
            manifest: None,
            connector: None,
            key: None,
            digest: None,
            signature: None,
            tags: &[],
        })
        .expect_err("empty update");
        assert!(
            nothing.to_string().contains("nothing to update"),
            "{nothing}"
        );
    }

    /// `stats` is `null` until admission passes: the column is blank, not
    /// `0`. The digest is shortened the way the plugins table shortens its
    /// own.
    #[test]
    fn the_row_leaves_parameters_blank_until_admission_passes() {
        let pending = model_row(&pending_model());
        assert_eq!(pending.model_id, "acme.fraud");
        assert_eq!(pending.version, 1);
        assert_eq!(pending.parameters, "");
        assert!(pending.admission.contains("pending"));
        assert_eq!(pending.digest, "sha256:9f1c0a3b7...");

        let passed = model_row(&passed_model());
        assert_eq!(passed.parameters, "1,234,567");
        assert!(passed.admission.contains("passed"));
    }

    /// Drive [`poll_admission`] over a canned sequence: every document is
    /// read in order, every state is reported, and the loop stops at the
    /// first state that is not `pending`.
    async fn poll_sequence(docs: Vec<Value>, timeout: Duration) -> (Polled, usize, Vec<String>) {
        let mut queue: VecDeque<Value> = docs.into();
        let mut fetches = 0;
        let mut seen = Vec::new();
        let polled = poll_admission(
            || {
                fetches += 1;
                // A sequence that runs out repeats its last document: the
                // timeout case keeps asking and keeps hearing `pending`.
                let next = if queue.len() > 1 {
                    queue.pop_front()
                } else {
                    queue.front().cloned()
                };
                async move { Ok::<Value, anyhow::Error>(next.expect("a document")) }
            },
            Duration::ZERO,
            timeout,
            |state| seen.push(state.to_string()),
        )
        .await
        .expect("polled");
        (polled, fetches, seen)
    }

    #[tokio::test]
    async fn polling_stops_at_the_first_state_that_is_not_pending() {
        let (polled, fetches, seen) = poll_sequence(
            vec![pending_model(), pending_model(), passed_model()],
            Duration::from_secs(60),
        )
        .await;
        assert_eq!(fetches, 3);
        assert_eq!(seen, ["pending", "pending", "passed"]);
        assert!(!polled.timed_out);
        assert_eq!(polled.state(), "passed");
        assert_eq!(polled.exit_code(), 0);
    }

    #[tokio::test]
    async fn a_failed_verdict_ends_the_wait_with_its_stage_and_reason() {
        let (polled, fetches, _) = poll_sequence(
            vec![pending_model(), failed_model()],
            Duration::from_secs(60),
        )
        .await;
        assert_eq!(fetches, 2);
        assert_eq!(polled.exit_code(), 1);
        let line = describe_admission(&polled.model["admission"]);
        assert!(line.contains("at fetch: object not found"), "{line}");
    }

    /// A wait that outlasts the clock exits 2 — not 1, so a script can tell
    /// "refused" from "still fetching" — and hands back the last document
    /// seen, which is still pending.
    #[tokio::test]
    async fn a_wait_past_the_timeout_exits_2_with_the_last_state_seen() {
        let (polled, fetches, _) = poll_sequence(vec![pending_model()], Duration::ZERO).await;
        assert_eq!(fetches, 1);
        assert!(polled.timed_out);
        assert_eq!(polled.state(), "pending");
        assert_eq!(polled.exit_code(), 2);
    }

    #[test]
    fn a_passed_verdict_is_summarised_with_where_and_when() {
        let line = describe_admission(&passed_model()["admission"]);
        assert!(line.contains("passed"), "{line}");
        assert!(line.contains("node node-1, 2026-09-13T10:00:05"), "{line}");
        let stats = describe_stats(&passed_model()["stats"]);
        assert_eq!(
            stats,
            "1,234,567 parameters, 240 nodes, IR 9, opset 17, probe 12 ms on onnxruntime/cpu"
        );
    }

    #[test]
    fn group_digits_groups_by_thousands() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    /// The subcommands as clap parses them: what `create` insists on, what
    /// `update` leaves optional, and the wait flags both `create` and
    /// `admit` carry.
    #[derive(Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: ModelsSubcommand,
    }

    #[test]
    fn create_requires_the_three_reference_flags() {
        let parsed = Harness::try_parse_from([
            "models",
            "create",
            "-f",
            "model.json",
            "--connector",
            "models-bucket",
            "--key",
            "fraud/v3.onnx",
            "--digest",
            "sha256:9f1c",
            "--tag",
            "scoring",
            "--wait",
            "--timeout",
            "30",
        ])
        .expect("parses");
        match parsed.cmd {
            ModelsSubcommand::Create {
                file,
                artifact,
                tags,
                wait,
                ..
            } => {
                assert_eq!(file, "model.json");
                assert_eq!(artifact.connector, "models-bucket");
                assert_eq!(artifact.key, "fraud/v3.onnx");
                assert_eq!(artifact.digest, "sha256:9f1c");
                assert_eq!(tags, ["scoring"]);
                assert!(wait.wait);
                assert_eq!(wait.timeout, 30);
                assert_eq!(wait.interval, 2);
            }
            _ => unreachable!("parsed as create"),
        }

        let missing = Harness::try_parse_from([
            "models",
            "create",
            "-f",
            "model.json",
            "--connector",
            "models-bucket",
            "--key",
            "fraud/v3.onnx",
        ]);
        assert!(missing.is_err(), "--digest is required");
    }

    #[test]
    fn update_and_admit_take_their_flags_optionally() {
        let parsed = Harness::try_parse_from(["models", "update", "acme.fraud", "--key", "k"])
            .expect("parses; the pairing rule is checked when the body is built");
        match parsed.cmd {
            ModelsSubcommand::Update {
                id,
                file,
                key,
                connector,
                ..
            } => {
                assert_eq!(id, "acme.fraud");
                assert!(file.is_none());
                assert_eq!(key.as_deref(), Some("k"));
                assert!(connector.is_none());
            }
            _ => unreachable!("parsed as update"),
        }

        let parsed = Harness::try_parse_from(["models", "admit", "acme.fraud"]).expect("parses");
        match parsed.cmd {
            ModelsSubcommand::Admit { id, wait } => {
                assert_eq!(id, "acme.fraud");
                assert!(!wait.wait);
                assert_eq!(wait.timeout, 900);
            }
            _ => unreachable!("parsed as admit"),
        }

        let parsed =
            Harness::try_parse_from(["models", "list", "--admission", "failed"]).expect("parses");
        match parsed.cmd {
            ModelsSubcommand::List { admission, .. } => {
                assert_eq!(admission.as_deref(), Some("failed"));
            }
            _ => unreachable!("parsed as list"),
        }
    }
}

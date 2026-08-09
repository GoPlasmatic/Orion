//! `orion-server package` — the packaging half of the promotion design
//! (proposal.md, the K stream §5).
//!
//! Packaging lives here, in the CLI, not in the server: the server provides
//! per-kind primitives (upsert import, activation pre-flight, deferred
//! reload, package receipts) and this module composes them. Everything talks
//! to a running instance's admin API over HTTP (`--server` +
//! `ORION_ADMIN_TOKEN`), except `lint`, which is fully offline.
//!
//! The artifact is one JSON document (proposal §6): a `package` header,
//! `requires` boundaries, and the three entity arrays in the exact shapes
//! the `/import` endpoints accept. `package.content_hash` is computed over
//! the entities' *importable content* — each entry projected through the
//! same `storage::content` canonicalization the server hashes with (K10) —
//! so DB-owned fields (`status`, `version`, timestamps) never make two
//! artifacts differ.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use orion::storage::content;
use orion::storage::repositories::channels::CreateChannelRequest;
use orion::storage::repositories::connectors::CreateConnectorRequest;
use orion::storage::repositories::workflows::CreateWorkflowRequest;

type CliError = Box<dyn std::error::Error>;

// ============================================================
// Artifact shapes
// ============================================================

#[derive(Debug, Serialize, Deserialize)]
struct PackageArtifact {
    package: PackageMeta,
    #[serde(default)]
    requires: Requires,
    #[serde(default)]
    connectors: Vec<Value>,
    #[serde(default)]
    workflows: Vec<Value>,
    #[serde(default)]
    channels: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PackageMeta {
    name: String,
    version: String,
    /// The Orion version that exported this artifact — informational.
    #[serde(default)]
    orion: String,
    content_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    exported_from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    exported_at: String,
}

/// Declared external dependencies (proposal §6): names this package uses but
/// deliberately does not contain, so closures stay small. `plan` verifies
/// they exist and are active in the target.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Requires {
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default)]
    connectors: Vec<String>,
}

/// The package-level hash: each entity array projected entry-by-entry
/// through the shared importable-content canonicalization, then hashed as
/// one document. Fails on an entry that does not parse as its import shape —
/// such an artifact could not apply anyway.
fn artifact_content_hash(artifact: &PackageArtifact) -> Result<String, CliError> {
    let mut connectors = Vec::new();
    for entry in &artifact.connectors {
        let req: CreateConnectorRequest = serde_json::from_value(entry.clone())
            .map_err(|e| format!("connector entry does not parse as an import item: {e}"))?;
        connectors.push(content::connector_request_content(&req));
    }
    let mut workflows = Vec::new();
    for entry in &artifact.workflows {
        let req: CreateWorkflowRequest = serde_json::from_value(entry.clone())
            .map_err(|e| format!("workflow entry does not parse as an import item: {e}"))?;
        workflows.push(content::workflow_request_content(&req));
    }
    let mut channels = Vec::new();
    for entry in &artifact.channels {
        let req: CreateChannelRequest = serde_json::from_value(entry.clone())
            .map_err(|e| format!("channel entry does not parse as an import item: {e}"))?;
        channels.push(content::channel_request_content(&req));
    }
    Ok(content::content_hash(&json!({
        "connectors": connectors,
        "workflows": workflows,
        "channels": channels,
    })))
}

// ============================================================
// Admin-API client
// ============================================================

struct AdminClient {
    base: String,
    token: Option<String>,
    /// Sent as `X-Orion-Change-Context` on every call (K5), so the audit
    /// trail groups the whole operation.
    change_context: String,
    http: reqwest::Client,
}

impl AdminClient {
    fn new(server: &str, change_context: String) -> Self {
        Self {
            base: server.trim_end_matches('/').to_string(),
            token: std::env::var("ORION_ADMIN_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            change_context,
            http: reqwest::Client::new(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("x-orion-change-context", &self.change_context);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        req
    }

    /// Send, insist on 2xx, and unwrap the `{"data": …}` envelope.
    async fn json(&self, req: reqwest::RequestBuilder) -> Result<Value, CliError> {
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body["error"]["message"].as_str().unwrap_or("").to_string();
            return Err(format!("HTTP {status}: {message}").into());
        }
        Ok(body.get("data").cloned().unwrap_or(body))
    }

    /// Like [`Self::json`] but 404 comes back as `Ok(None)`.
    async fn json_opt(&self, req: reqwest::RequestBuilder) -> Result<Option<Value>, CliError> {
        let resp = req.send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body["error"]["message"].as_str().unwrap_or("").to_string();
            return Err(format!("HTTP {status}: {message}").into());
        }
        Ok(Some(body.get("data").cloned().unwrap_or(body)))
    }

    async fn get(&self, path: &str) -> Result<Value, CliError> {
        self.json(self.request(reqwest::Method::GET, path)).await
    }
}

fn read_artifact(path: &str) -> Result<PackageArtifact, CliError> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read '{path}': {e}"))?;
    let artifact: PackageArtifact = serde_json::from_str(&raw)
        .map_err(|e| format!("'{path}' is not a package artifact: {e}"))?;
    Ok(artifact)
}

// ============================================================
// export
// ============================================================

pub(crate) async fn run_export(
    server: &str,
    tag: Option<&str>,
    channel_ids: &[String],
    name: &str,
    version: &str,
    output: Option<&str>,
) -> Result<(), CliError> {
    if tag.is_none() && channel_ids.is_empty() {
        return Err("select the package's channels with --tag or --channels".into());
    }
    let client = AdminClient::new(server, format!("package={name}@{version} export"));

    // 1. The selected channels.
    let mut channels: Vec<Value> = Vec::new();
    if let Some(tag) = tag {
        let listed = client
            .get(&format!(
                "/api/v1/admin/channels/export?tag={}",
                urlencode(tag)
            ))
            .await?;
        channels.extend(listed.as_array().cloned().unwrap_or_default());
    }
    for id in channel_ids {
        channels.push(client.get(&format!("/api/v1/admin/channels/{id}")).await?);
    }
    if channels.is_empty() {
        return Err("the selector matched no channels".into());
    }
    let channel_names: Vec<String> = channels
        .iter()
        .filter_map(|c| c["name"].as_str().map(str::to_string))
        .collect();

    // 2. Their workflows, via each channel's workflow_id.
    let mut workflow_ids: Vec<String> = Vec::new();
    for channel in &channels {
        match channel["workflow_id"].as_str() {
            Some(wf) if !wf.is_empty() => {
                if !workflow_ids.contains(&wf.to_string()) {
                    workflow_ids.push(wf.to_string());
                }
            }
            _ => eprintln!(
                "warning: channel '{}' names no workflow_id and can never activate",
                channel["name"].as_str().unwrap_or("?")
            ),
        }
    }
    let mut workflows = Vec::new();
    let mut connector_names: Vec<String> = Vec::new();
    let mut required_channels: Vec<String> = Vec::new();
    for id in &workflow_ids {
        workflows.push(client.get(&format!("/api/v1/admin/workflows/{id}")).await?);
        // 3. The dependency closure, from the server's own walk (K9).
        let deps = client
            .get(&format!("/api/v1/admin/workflows/{id}/dependencies"))
            .await?;
        for c in deps["connectors"].as_array().into_iter().flatten() {
            if let Some(name) = c["connector"].as_str()
                && !connector_names.iter().any(|n| n == name)
            {
                connector_names.push(name.to_string());
            }
        }
        for target in deps["channels"].as_array().into_iter().flatten() {
            if let Some(target) = target.as_str()
                && !channel_names.iter().any(|n| n == target)
                && !required_channels.iter().any(|n| n == target)
            {
                // A channel_call outside the selection is a boundary, not a
                // member: it goes to `requires` for `plan` to verify.
                required_channels.push(target.to_string());
            }
        }
        if deps["has_dynamic_channel_calls"] == true {
            eprintln!(
                "warning: workflow '{id}' resolves channel_call targets dynamically — \
                 the requires list cannot be complete"
            );
        }
    }

    // 4. The referenced connectors, from one export sweep.
    let all_connectors = client.get("/api/v1/admin/connectors/export").await?;
    let mut connectors = Vec::new();
    let mut required_connectors: Vec<String> = Vec::new();
    for name in &connector_names {
        match all_connectors
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"].as_str() == Some(name))
        {
            Some(connector) => connectors.push(connector.clone()),
            None => {
                eprintln!(
                    "warning: connector '{name}' is referenced but not stored on the \
                     source — recorded under requires.connectors"
                );
                required_connectors.push(name.clone());
            }
        }
    }

    // 5. Carry activation intent: DB-owned `status` does not survive import,
    //    so the artifact says what apply should activate.
    for entity in workflows.iter_mut().chain(channels.iter_mut()) {
        if entity["status"] == "active"
            && let Some(obj) = entity.as_object_mut()
        {
            obj.insert("activate".to_string(), json!(true));
        }
    }

    let mut artifact = PackageArtifact {
        package: PackageMeta {
            name: name.to_string(),
            version: version.to_string(),
            orion: env!("CARGO_PKG_VERSION").to_string(),
            content_hash: String::new(),
            exported_from: server.to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
        requires: Requires {
            channels: required_channels,
            connectors: required_connectors,
        },
        connectors,
        workflows,
        channels,
    };
    artifact.package.content_hash = artifact_content_hash(&artifact)?;

    let rendered = serde_json::to_string_pretty(&artifact)?;
    match output {
        Some(path) => {
            std::fs::write(path, rendered).map_err(|e| format!("write '{path}': {e}"))?;
            println!(
                "wrote {}@{} ({} connectors, {} workflows, {} channels) to {path}",
                artifact.package.name,
                artifact.package.version,
                artifact.connectors.len(),
                artifact.workflows.len(),
                artifact.channels.len(),
            );
        }
        None => println!("{rendered}"),
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                vec![c]
            } else {
                format!("%{:02X}", c as u32).chars().collect()
            }
        })
        .collect()
}

// ============================================================
// lint (offline)
// ============================================================

pub(crate) fn run_lint(file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    let mut errors: Vec<String> = Vec::new();

    if artifact.package.name.trim().is_empty() {
        errors.push("package.name is empty".to_string());
    }
    if artifact.package.version.trim().is_empty() {
        errors.push("package.version is empty".to_string());
    }

    // The hash is part of the contract: an artifact edited without
    // re-hashing would defeat the receipt comparison downstream.
    match artifact_content_hash(&artifact) {
        Ok(actual) if actual != artifact.package.content_hash => errors.push(format!(
            "package.content_hash does not match the entities — expected {actual}"
        )),
        Ok(_) => {}
        Err(e) => errors.push(e.to_string()),
    }

    // Per-entity create-path validation — the same functions the POST
    // endpoints run, so lint-clean means the server will accept the shapes.
    let mut connector_names = Vec::new();
    for (i, entry) in artifact.connectors.iter().enumerate() {
        match serde_json::from_value::<CreateConnectorRequest>(entry.clone()) {
            Ok(req) => {
                if let Err(e) = orion::validation::validate_create_connector(&req) {
                    errors.push(format!("connectors[{i}] '{}': {e}", req.name));
                }
                connector_names.push(req.name);
            }
            Err(e) => errors.push(format!("connectors[{i}]: not an import item: {e}")),
        }
    }
    let mut workflow_ids = Vec::new();
    let mut workflow_tasks: Vec<(String, Value)> = Vec::new();
    for (i, entry) in artifact.workflows.iter().enumerate() {
        match serde_json::from_value::<CreateWorkflowRequest>(entry.clone()) {
            Ok(req) => {
                if let Err(e) = orion::validation::validate_create_workflow(&req) {
                    errors.push(format!("workflows[{i}] '{}': {e}", req.name));
                }
                if let Some(id) = &req.workflow_id {
                    if workflow_ids.contains(id) {
                        errors.push(format!("workflows[{i}]: duplicate workflow_id '{id}'"));
                    }
                    workflow_ids.push(id.clone());
                    workflow_tasks.push((id.clone(), req.tasks.clone()));
                } else {
                    errors.push(format!(
                        "workflows[{i}] '{}': a package workflow must carry an explicit \
                         workflow_id — a generated id cannot be referenced by channels \
                         or re-applied idempotently",
                        req.name
                    ));
                }
            }
            Err(e) => errors.push(format!("workflows[{i}]: not an import item: {e}")),
        }
    }
    let mut channel_ids = Vec::new();
    let mut channel_names = Vec::new();
    for (i, entry) in artifact.channels.iter().enumerate() {
        match serde_json::from_value::<CreateChannelRequest>(entry.clone()) {
            Ok(req) => {
                if let Err(e) = orion::validation::validate_create_channel(&req) {
                    errors.push(format!("channels[{i}] '{}': {e}", req.name));
                }
                match &req.channel_id {
                    Some(id) => {
                        if channel_ids.contains(id) {
                            errors.push(format!("channels[{i}]: duplicate channel_id '{id}'"));
                        }
                        channel_ids.push(id.clone());
                    }
                    None => errors.push(format!(
                        "channels[{i}] '{}': a package channel must carry an explicit \
                         channel_id",
                        req.name
                    )),
                }
                if channel_names.contains(&req.name) {
                    errors.push(format!(
                        "channels[{i}]: duplicate channel name '{}' — channel names are \
                         unique (K7)",
                        req.name
                    ));
                }
                channel_names.push(req.name.clone());
                // Closure: the workflow a channel names must be contained.
                match &req.workflow_id {
                    Some(wf) if !wf.is_empty() => {
                        if !workflow_ids.contains(wf) {
                            errors.push(format!(
                                "channels[{i}] '{}': workflow '{wf}' is not in the package",
                                req.name
                            ));
                        }
                    }
                    _ => errors.push(format!(
                        "channels[{i}] '{}': no workflow_id — the channel can never \
                         activate",
                        req.name
                    )),
                }
            }
            Err(e) => errors.push(format!("channels[{i}]: not an import item: {e}")),
        }
    }

    // Closure: every task reference resolves inside the package or is a
    // declared boundary in `requires`.
    for (workflow_id, tasks) in &workflow_tasks {
        for task in tasks.as_array().into_iter().flatten() {
            let Some(function) = task.get("function") else {
                continue;
            };
            let fn_name = function.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = function.get("input");
            if orion::engine::CONNECTOR_FUNCTIONS.contains(&fn_name)
                && let Some(connector) = input
                    .and_then(|i| i.get("connector"))
                    .and_then(|c| c.as_str())
                && !connector_names.iter().any(|n| n == connector)
                && !artifact.requires.connectors.iter().any(|n| n == connector)
            {
                errors.push(format!(
                    "workflow '{workflow_id}': connector '{connector}' is neither in the \
                     package nor declared in requires.connectors"
                ));
            }
            if fn_name == "channel_call"
                && let Some(target) = input
                    .and_then(|i| i.get("channel"))
                    .and_then(|c| c.as_str())
                && !channel_names.iter().any(|n| n == target)
                && !artifact.requires.channels.iter().any(|n| n == target)
            {
                errors.push(format!(
                    "workflow '{workflow_id}': channel_call target '{target}' is neither \
                     in the package nor declared in requires.channels"
                ));
            }
        }
    }

    if errors.is_empty() {
        println!(
            "'{file}' is a valid package: {}@{} — {} connectors, {} workflows, {} channels",
            artifact.package.name,
            artifact.package.version,
            artifact.connectors.len(),
            artifact.workflows.len(),
            artifact.channels.len(),
        );
        Ok(())
    } else {
        for error in &errors {
            eprintln!("error: {error}");
        }
        Err(format!("{} lint error(s) in '{file}'", errors.len()).into())
    }
}

// ============================================================
// plan
// ============================================================

/// The receipt verdict `plan` and `apply` both start from.
enum ReceiptState {
    Fresh,
    Staged,
    AppliedSame,
    AppliedConflict,
}

async fn check_receipt(
    client: &AdminClient,
    artifact: &PackageArtifact,
) -> Result<ReceiptState, CliError> {
    let receipts = client
        .json_opt(client.request(
            reqwest::Method::GET,
            &format!("/api/v1/admin/packages/{}", artifact.package.name),
        ))
        .await?;
    let Some(receipts) = receipts else {
        return Ok(ReceiptState::Fresh);
    };
    let row = receipts["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["version"] == artifact.package.version.as_str())
        .cloned();
    Ok(match row {
        None => ReceiptState::Fresh,
        Some(row) if row["state"] == "applied" => {
            if row["content_hash"] == artifact.package.content_hash.as_str() {
                ReceiptState::AppliedSame
            } else {
                ReceiptState::AppliedConflict
            }
        }
        Some(_) => ReceiptState::Staged,
    })
}

pub(crate) async fn run_plan(server: &str, file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    verify_hash(&artifact)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = AdminClient::new(server, format!("package={package} plan"));

    // The immutability gate first: a reused applied version is dead on
    // arrival, and nothing below can change that.
    match check_receipt(&client, &artifact).await? {
        ReceiptState::AppliedConflict => {
            return Err(format!(
                "{package} is already applied on {server} with different content — an \
                 applied package version is immutable; bump the package version"
            )
            .into());
        }
        ReceiptState::AppliedSame => {
            println!("{package} is already applied with identical content — apply is a no-op");
        }
        ReceiptState::Staged => {
            println!("{package} is staged here; apply may update it in place");
        }
        ReceiptState::Fresh => {}
    }

    // `requires` boundaries must exist in the target.
    let mut failures = 0usize;
    for name in &artifact.requires.connectors {
        let found = client
            .get("/api/v1/admin/connectors/export")
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["name"].as_str() == Some(name));
        if !found {
            eprintln!("error: required connector '{name}' does not exist in the target");
            failures += 1;
        }
    }
    for name in &artifact.requires.channels {
        let listed = client
            .get("/api/v1/admin/channels?status=active&limit=1000")
            .await?;
        if !listed
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["name"].as_str() == Some(name))
        {
            eprintln!("error: required channel '{name}' is not active in the target");
            failures += 1;
        }
    }

    // Per-entity actions from the servers' own dry-runs (K2).
    let provided = provided_names(&artifact);
    for (kind, items) in [
        ("connectors", &artifact.connectors),
        ("workflows", &artifact.workflows),
        ("channels", &artifact.channels),
    ] {
        if items.is_empty() {
            continue;
        }
        let outcome = client
            .json(
                client
                    .request(
                        reqwest::Method::POST,
                        &format!(
                            "/api/v1/admin/{kind}/import?dry_run=true&on_conflict=new_version"
                        ),
                    )
                    .json(items),
            )
            .await?;
        for result in outcome["results"].as_array().into_iter().flatten() {
            println!(
                "  {kind:<10} {:<28} {}",
                result["id"].as_str().unwrap_or("(generated)"),
                result["action"].as_str().unwrap_or("?"),
            );
        }
        for error in outcome["errors"].as_array().into_iter().flatten() {
            eprintln!(
                "error: {kind}[{}]: {}",
                error["index"],
                error["error"].as_str().unwrap_or("?")
            );
            failures += 1;
        }
    }

    // Activation gates (K3), evaluated against the *current* state. A
    // finding that names something this package itself provides is expected
    // to clear during apply's ordered activation, and is reported as such
    // rather than counted as a failure.
    for (kind, id) in activation_intents(&artifact) {
        let outcome = client
            .json(
                client
                    .request(
                        reqwest::Method::PATCH,
                        &format!("/api/v1/admin/{kind}/{id}/status?dry_run=true"),
                    )
                    .json(&json!({"status": "active"})),
            )
            .await;
        let outcome = match outcome {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {kind} '{id}' activation pre-flight failed: {e}");
                failures += 1;
                continue;
            }
        };
        for finding in outcome["errors"].as_array().into_iter().flatten() {
            let message = finding["message"].as_str().unwrap_or("");
            if provided.iter().any(|name| message.contains(name.as_str()))
                || message.contains("not found")
                || message.contains("No draft version")
            {
                println!("  {kind:<10} {id:<28} gate pending apply order: {message}");
            } else {
                eprintln!("error: {kind} '{id}' would not activate: {message}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        Err(format!("plan found {failures} blocking issue(s)").into())
    } else {
        println!("plan: {package} applies cleanly to {server}");
        Ok(())
    }
}

fn verify_hash(artifact: &PackageArtifact) -> Result<(), CliError> {
    let actual = artifact_content_hash(artifact)?;
    if actual != artifact.package.content_hash {
        return Err(format!(
            "package.content_hash does not match the entities (expected {actual}) — \
             re-run `package lint` after editing an artifact"
        )
        .into());
    }
    Ok(())
}

/// Names this package will create or activate — used to downgrade plan
/// findings that apply's ordering satisfies.
fn provided_names(artifact: &PackageArtifact) -> Vec<String> {
    let mut names = Vec::new();
    for entry in &artifact.connectors {
        if let Some(n) = entry["name"].as_str() {
            names.push(n.to_string());
        }
    }
    for entry in &artifact.workflows {
        if let Some(n) = entry["workflow_id"].as_str() {
            names.push(n.to_string());
        }
    }
    names
}

/// `(kind, id)` of every entity the artifact marks `activate: true`, in
/// dependency order: workflows before the channels that name them.
fn activation_intents(artifact: &PackageArtifact) -> Vec<(&'static str, String)> {
    let mut intents = Vec::new();
    for entry in &artifact.workflows {
        if entry["activate"] == true
            && let Some(id) = entry["workflow_id"].as_str()
        {
            intents.push(("workflows", id.to_string()));
        }
    }
    for entry in &artifact.channels {
        if entry["activate"] == true
            && let Some(id) = entry["channel_id"].as_str()
        {
            intents.push(("channels", id.to_string()));
        }
    }
    intents
}

// ============================================================
// apply
// ============================================================

pub(crate) async fn run_apply(server: &str, file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    verify_hash(&artifact)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = AdminClient::new(server, format!("package={package}"));

    // Phase 1 — claim the receipt as staged. This is the atomic
    // same-version-different-content rejection (K14), and doubles as the
    // guard against two concurrent applies.
    if matches!(
        check_receipt(&client, &artifact).await?,
        ReceiptState::AppliedSame
    ) {
        println!("{package} is already applied with identical content — nothing to do");
        return Ok(());
    }
    client
        .json(
            client
                .request(
                    reqwest::Method::PUT,
                    &format!("/api/v1/admin/packages/{}", artifact.package.name),
                )
                .json(&json!({
                    "version": artifact.package.version,
                    "content_hash": artifact.package.content_hash,
                    "state": "staged",
                })),
        )
        .await
        .map_err(|e| format!("could not claim the receipt: {e}"))?;

    // Phase 2 — stage everything as drafts, in dependency order. Connector
    // import reloads the connector registry server-side, so workflow
    // activation's registry gate sees them.
    for (kind, items) in [
        ("connectors", &artifact.connectors),
        ("workflows", &artifact.workflows),
        ("channels", &artifact.channels),
    ] {
        if items.is_empty() {
            continue;
        }
        let outcome = client
            .json(
                client
                    .request(
                        reqwest::Method::POST,
                        &format!("/api/v1/admin/{kind}/import?on_conflict=new_version"),
                    )
                    .json(items),
            )
            .await?;
        let failed = outcome["failed"].as_u64().unwrap_or(0);
        println!(
            "staged {kind}: {} written, {} unchanged, {failed} failed",
            outcome["imported"], outcome["unchanged"]
        );
        if failed > 0 {
            for error in outcome["errors"].as_array().into_iter().flatten() {
                eprintln!(
                    "error: {kind}[{}]: {}",
                    error["index"],
                    error["error"].as_str().unwrap_or("?")
                );
            }
            return Err(
                "staging failed; nothing was activated and the receipt stays \
                 staged — fix the artifact and re-run (a staged receipt may be re-put)"
                    .into(),
            );
        }
    }

    // Phase 3 — activate in dependency order with the reload deferred (K4):
    // one engine rebuild and one cluster epoch bump at the end, not one per
    // entity.
    for (kind, id) in activation_intents(&artifact) {
        let mut body = json!({"status": "active"});
        if kind == "workflows"
            && let Some(pct) = artifact
                .workflows
                .iter()
                .find(|w| w["workflow_id"].as_str() == Some(id.as_str()))
                .and_then(|w| w["rollout_percentage"].as_i64())
        {
            body["rollout_percentage"] = json!(pct);
        }
        let result = client
            .json(
                client
                    .request(
                        reqwest::Method::PATCH,
                        &format!("/api/v1/admin/{kind}/{id}/status?reload=defer"),
                    )
                    .json(&body),
            )
            .await;
        match result {
            Ok(_) => println!("activated {kind} '{id}'"),
            // An entity that is already active has no draft to promote —
            // `unchanged` staging left it as it was, which is the goal state.
            Err(e) if e.to_string().contains("No draft version") => {
                println!("{kind} '{id}' is already active (unchanged)");
            }
            Err(e) => {
                eprintln!("error: activating {kind} '{id}': {e}");
                return Err(format!(
                    "activation stopped at {kind} '{id}'. Everything before it is \
                     active but the engine has NOT been reloaded; everything after is \
                     staged as drafts. The receipt stays staged — fix the cause and \
                     re-run apply (idempotent), or run POST /engine/reload to serve \
                     what did activate"
                )
                .into());
            }
        }
    }

    // Phase 4 — one reload, one epoch bump.
    client
        .json(client.request(reqwest::Method::POST, "/api/v1/admin/engine/reload"))
        .await
        .map_err(|e| format!("entities are active but the engine reload failed: {e}"))?;

    // Phase 5 — flip the receipt.
    client
        .json(
            client
                .request(
                    reqwest::Method::PUT,
                    &format!("/api/v1/admin/packages/{}", artifact.package.name),
                )
                .json(&json!({
                    "version": artifact.package.version,
                    "content_hash": artifact.package.content_hash,
                    "state": "applied",
                })),
        )
        .await?;

    println!("applied {package} to {server}");
    Ok(())
}

// ============================================================
// diff
// ============================================================

pub(crate) async fn run_diff(server: &str, file: &str) -> Result<(), CliError> {
    let artifact = read_artifact(file)?;
    let package = format!("{}@{}", artifact.package.name, artifact.package.version);
    let client = AdminClient::new(server, format!("package={package} diff"));

    let mut differences = 0usize;
    let report = |entity: &str, state: &str, count: &mut usize| {
        println!("  {state:<10} {entity}");
        if state != "unchanged" {
            *count += 1;
        }
    };

    // Server-side content hashes (K10) against the artifact's per-entity
    // projections — the same canonicalization on both sides.
    let all_connectors = client.get("/api/v1/admin/connectors/export").await?;
    for entry in &artifact.connectors {
        let req: CreateConnectorRequest = serde_json::from_value(entry.clone())?;
        let expected = content::content_hash(&content::connector_request_content(&req));
        let stored = all_connectors
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["name"].as_str() == Some(req.name.as_str()))
            .cloned();
        let label = format!("connector '{}'", req.name);
        match stored {
            None => report(&label, "missing", &mut differences),
            Some(row) if row["content_hash"].as_str() == Some(expected.as_str()) => {
                report(&label, "unchanged", &mut differences)
            }
            Some(_) => report(&label, "changed", &mut differences),
        }
    }
    for (kind, entries, id_field) in [
        ("workflow", &artifact.workflows, "workflow_id"),
        ("channel", &artifact.channels, "channel_id"),
    ] {
        for entry in entries {
            let Some(id) = entry[id_field].as_str() else {
                continue;
            };
            let expected = if kind == "workflow" {
                let req: CreateWorkflowRequest = serde_json::from_value(entry.clone())?;
                content::content_hash(&content::workflow_request_content(&req))
            } else {
                let req: CreateChannelRequest = serde_json::from_value(entry.clone())?;
                content::content_hash(&content::channel_request_content(&req))
            };
            let stored = client
                .json_opt(
                    client.request(reqwest::Method::GET, &format!("/api/v1/admin/{kind}s/{id}")),
                )
                .await?;
            let label = format!("{kind} '{id}'");
            match stored {
                None => report(&label, "missing", &mut differences),
                Some(row) if row["content_hash"].as_str() == Some(expected.as_str()) => {
                    report(&label, "unchanged", &mut differences)
                }
                Some(_) => report(&label, "changed", &mut differences),
            }
        }
    }

    if differences > 0 {
        Err(format!(
            "{differences} entity(ies) differ between '{file}' and {server} — the \
             estate has drifted from the artifact"
        )
        .into())
    } else {
        println!("no drift: {package} matches {server}");
        Ok(())
    }
}

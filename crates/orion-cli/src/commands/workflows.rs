use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils::{self, colorize_status, truncate};

#[derive(Args)]
pub struct WorkflowsCmd {
    #[command(subcommand)]
    command: WorkflowsSubcommand,
}

#[derive(Subcommand)]
enum WorkflowsSubcommand {
    /// List all workflows
    List {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Get a workflow by ID
    Get {
        /// Workflow ID
        id: String,
    },
    /// Create a new workflow
    Create {
        /// Custom workflow ID
        #[arg(long)]
        id: Option<String>,
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Update a workflow
    Update {
        /// Workflow ID
        id: String,
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Delete a workflow
    Delete {
        /// Workflow ID
        id: String,
    },
    /// Activate a workflow
    Activate {
        /// Workflow ID
        id: String,
    },
    /// Archive a workflow
    Archive {
        /// Workflow ID
        id: String,
    },
    /// Validate a workflow definition without creating it
    Validate {
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Update rollout percentage for a workflow
    Rollout {
        /// Workflow ID
        id: String,
        /// Rollout percentage (0-100)
        #[arg(short, long)]
        percentage: i64,
    },
    /// List version history for a workflow
    Versions {
        /// Workflow ID
        id: String,
    },
    /// Create a new draft version of a workflow
    NewVersion {
        /// Workflow ID
        id: String,
    },
    /// Test/dry-run a workflow with data
    Test {
        /// Workflow ID
        id: String,
        /// JSON file with test payload
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON test data
        #[arg(short, long)]
        data: Option<String>,
        /// Read test data from stdin
        #[arg(long)]
        stdin: bool,
        /// Optional metadata JSON
        #[arg(long)]
        metadata: Option<String>,
        /// Show execution trace
        #[arg(long)]
        trace: bool,
    },
    /// Export workflows
    Export {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Import workflows from file
    Import {
        /// JSON file with workflows array
        #[arg(short, long)]
        file: String,
        /// Preview without importing
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare local file against server state
    Diff {
        /// JSON file with workflows
        #[arg(short, long)]
        file: String,
    },
}

#[derive(Tabled)]
struct WorkflowRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Priority")]
    priority: i64,
    #[tabled(rename = "Rollout")]
    rollout: String,
    #[tabled(rename = "Version")]
    version: i64,
}

#[derive(Tabled)]
struct VersionRow {
    #[tabled(rename = "Version")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Priority")]
    priority: i64,
    #[tabled(rename = "Updated")]
    updated: String,
}

impl WorkflowsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            WorkflowsSubcommand::List { status, tag } => {
                list(client, format, quiet, status, tag).await
            }
            WorkflowsSubcommand::Get { id } => {
                get_workflow(client, format, quiet, verbose, id).await
            }
            WorkflowsSubcommand::Create {
                id: custom_id,
                file,
                data,
            } => {
                let mut body = utils::read_json_input(file.as_deref(), data.as_deref(), false)?;
                if let Some(wid) = custom_id {
                    body["workflow_id"] = Value::String(wid.clone());
                }
                create(client, format, quiet, &body).await
            }
            WorkflowsSubcommand::Update { id, file, data } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), false)?;
                update(client, format, quiet, id, &body).await
            }
            WorkflowsSubcommand::Delete { id } => delete(client, quiet, yes, id).await,
            WorkflowsSubcommand::Activate { id } => {
                change_status(client, quiet, id, "active").await
            }
            WorkflowsSubcommand::Archive { id } => {
                change_status(client, quiet, id, "archived").await
            }
            WorkflowsSubcommand::Validate { file, data } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), false)?;
                validate(client, format, quiet, &body).await
            }
            WorkflowsSubcommand::Rollout { id, percentage } => {
                rollout(client, quiet, id, *percentage).await
            }
            WorkflowsSubcommand::Versions { id } => versions(client, format, quiet, id).await,
            WorkflowsSubcommand::NewVersion { id } => new_version(client, format, quiet, id).await,
            WorkflowsSubcommand::Test {
                id,
                file,
                data,
                stdin,
                metadata,
                trace,
            } => {
                let payload = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                let meta = metadata.as_deref().map(serde_json::from_str).transpose()?;
                test_workflow(client, format, quiet, id, &payload, meta.as_ref(), *trace).await
            }
            WorkflowsSubcommand::Export { status, tag } => export(client, status, tag).await,
            WorkflowsSubcommand::Import { file, dry_run } => {
                import(client, quiet, file, *dry_run).await
            }
            WorkflowsSubcommand::Diff { file } => diff(client, file).await,
        }
    }
}

async fn list(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    status: &Option<String>,
    tag: &Option<String>,
) -> Result<i32> {
    let qs = utils::build_query_string(&[("status", status.clone()), ("tag", tag.clone())]);

    let resp: Value = client.get(&format!("/api/v1/admin/workflows{qs}")).await?;
    let workflows = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for wf in &workflows {
            if let Some(id) = wf["workflow_id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if workflows.is_empty() {
        println!("{}", "No workflows found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<WorkflowRow> = workflows
        .iter()
        .map(|r| {
            let rollout = r["rollout_percentage"].as_i64().unwrap_or(0);
            WorkflowRow {
                id: truncate(r["workflow_id"].as_str().unwrap_or(""), 12),
                name: truncate(r["name"].as_str().unwrap_or(""), 30),
                status: colorize_status(r["status"].as_str().unwrap_or("")),
                priority: r["priority"].as_i64().unwrap_or(0),
                rollout: format!("{rollout}%"),
                version: r["version"].as_i64().unwrap_or(0),
            }
        })
        .collect();

    output::print_table(rows);
    let total = resp["total"].as_i64().unwrap_or(workflows.len() as i64);
    println!("{}", format!("{} workflow(s)", total).dimmed());
    Ok(0)
}

async fn get_workflow(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    verbose: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&format!("/api/v1/admin/workflows/{id}")).await?;

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let wf = &resp["data"];

    println!("{}", "Workflow Details".bold());
    println!(
        "  ID:          {}",
        wf["workflow_id"].as_str().unwrap_or("")
    );
    println!("  Name:        {}", wf["name"].as_str().unwrap_or(""));
    println!(
        "  Description: {}",
        wf["description"].as_str().unwrap_or("(none)")
    );
    println!(
        "  Status:      {}",
        colorize_status(wf["status"].as_str().unwrap_or(""))
    );
    println!("  Priority:    {}", wf["priority"].as_i64().unwrap_or(0));
    println!(
        "  Rollout:     {}%",
        wf["rollout_percentage"].as_i64().unwrap_or(0)
    );
    println!("  Version:     {}", wf["version"].as_i64().unwrap_or(0));
    println!("  Tags:        {}", wf["tags"]);
    println!(
        "  Continue on error: {}",
        wf["continue_on_error"].as_bool().unwrap_or(false)
    );
    println!("  Created:     {}", wf["created_at"]);
    println!("  Updated:     {}", wf["updated_at"]);

    if verbose {
        println!("\n{}", "Condition:".bold());
        if let Ok(cond) =
            serde_json::from_str::<Value>(wf["condition_json"].as_str().unwrap_or("true"))
        {
            println!("{}", serde_json::to_string_pretty(&cond)?);
        } else {
            println!("{}", wf["condition_json"]);
        }
        println!("\n{}", "Tasks:".bold());
        if let Ok(tasks) = serde_json::from_str::<Value>(wf["tasks_json"].as_str().unwrap_or("[]"))
        {
            println!("{}", serde_json::to_string_pretty(&tasks)?);
        } else {
            println!("{}", wf["tasks_json"]);
        }
    }

    Ok(0)
}

async fn create(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post("/api/v1/admin/workflows", body).await?;
    let wf = &resp["data"];

    if quiet {
        println!("{}", wf["workflow_id"].as_str().unwrap_or(""));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Workflow created: {} ({})",
        "OK".green().bold(),
        wf["name"].as_str().unwrap_or(""),
        wf["workflow_id"].as_str().unwrap_or("")
    );
    Ok(0)
}

async fn update(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client
        .put(&format!("/api/v1/admin/workflows/{id}"), body)
        .await?;
    let wf = &resp["data"];

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Workflow updated: {} (v{})",
        "OK".green().bold(),
        wf["name"].as_str().unwrap_or(""),
        wf["version"].as_i64().unwrap_or(0)
    );
    Ok(0)
}

async fn delete(client: &OrionClient, quiet: bool, yes: bool, id: &str) -> Result<i32> {
    if !utils::confirm(&format!("Delete workflow {id}?"), yes)? {
        println!("Cancelled.");
        return Ok(0);
    }

    client
        .delete_request(&format!("/api/v1/admin/workflows/{id}"))
        .await?;

    if !quiet {
        println!("{} Workflow {id} deleted", "OK".green().bold());
    }
    Ok(0)
}

async fn change_status(client: &OrionClient, quiet: bool, id: &str, status: &str) -> Result<i32> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/workflows/{id}/status"), &body)
        .await?;

    if !quiet {
        let wf = &resp["data"];
        println!(
            "{} Workflow {} status changed to {}",
            "OK".green().bold(),
            wf["name"].as_str().unwrap_or(id),
            colorize_status(status)
        );
    }
    Ok(0)
}

async fn test_workflow(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    payload: &Value,
    metadata: Option<&Value>,
    trace: bool,
) -> Result<i32> {
    let mut body = serde_json::json!({ "data": payload });
    if let Some(meta) = metadata {
        body["metadata"] = meta.clone();
    }

    let resp: Value = client
        .post(&format!("/api/v1/admin/workflows/{id}/test"), &body)
        .await?;

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        let matched = resp["matched"].as_bool().unwrap_or(false);
        return Ok(if matched { 0 } else { 1 });
    }

    let matched = resp["matched"].as_bool().unwrap_or(false);

    if quiet {
        println!("{}", if matched { "matched" } else { "no_match" });
        return Ok(if matched { 0 } else { 1 });
    }

    let match_display = if matched {
        "MATCHED".green().bold()
    } else {
        "NO MATCH".red().bold()
    };
    println!("{}", "Test Result".bold());
    println!("  Workflow: {id}");
    println!("  Result:   {match_display}");

    if matched {
        if let Some(output) = resp.get("output") {
            println!("\n{}", "Output:".bold());
            println!("{}", serde_json::to_string_pretty(output)?);
        }
    }

    if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            println!("\n{}", "Errors:".yellow().bold());
            for err in errors {
                println!("  - {err}");
            }
        }
    }

    if trace {
        if let Some(trace_data) = resp.get("trace") {
            println!("\n{}", "Execution Trace:".bold());
            print_trace(trace_data, 1);
        }
    }

    Ok(if matched { 0 } else { 1 })
}

async fn validate(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client
        .post("/api/v1/admin/workflows/validate", body)
        .await?;

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        let valid = resp["valid"].as_bool().unwrap_or(false);
        return Ok(if valid { 0 } else { 1 });
    }

    let valid = resp["valid"].as_bool().unwrap_or(false);

    if quiet {
        println!("{}", if valid { "valid" } else { "invalid" });
        return Ok(if valid { 0 } else { 1 });
    }

    if valid {
        println!("{} Workflow definition is valid", "OK".green().bold());
    } else {
        println!("{} Workflow definition has issues", "INVALID".red().bold());
    }

    if let Some(errors) = resp["errors"].as_array() {
        if !errors.is_empty() {
            println!("\n{}", "Errors:".red().bold());
            for err in errors {
                let field = err["field"].as_str().unwrap_or("");
                let msg = err["message"].as_str().unwrap_or("");
                println!("  - {field}: {msg}");
            }
        }
    }

    if let Some(warnings) = resp["warnings"].as_array() {
        if !warnings.is_empty() {
            println!("\n{}", "Warnings:".yellow().bold());
            for warn in warnings {
                let field = warn["field"].as_str().unwrap_or("");
                let msg = warn["message"].as_str().unwrap_or("");
                println!("  - {field}: {msg}");
            }
        }
    }

    Ok(if valid { 0 } else { 1 })
}

async fn rollout(client: &OrionClient, quiet: bool, id: &str, percentage: i64) -> Result<i32> {
    let body = serde_json::json!({ "rollout_percentage": percentage });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/workflows/{id}/rollout"), &body)
        .await?;

    if !quiet {
        let wf = &resp["data"];
        println!(
            "{} Workflow {} rollout set to {}%",
            "OK".green().bold(),
            wf["name"].as_str().unwrap_or(id),
            percentage
        );
    }
    Ok(0)
}

async fn versions(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client
        .get(&format!("/api/v1/admin/workflows/{id}/versions"))
        .await?;
    let vers = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for v in &vers {
            println!("{}", v["version"].as_i64().unwrap_or(0));
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if vers.is_empty() {
        println!("{}", "No versions found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<VersionRow> = vers
        .iter()
        .map(|v| VersionRow {
            version: v["version"].as_i64().unwrap_or(0),
            status: colorize_status(v["status"].as_str().unwrap_or("")),
            priority: v["priority"].as_i64().unwrap_or(0),
            updated: v["updated_at"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    output::print_table(rows);
    let total = resp["total"].as_i64().unwrap_or(vers.len() as i64);
    println!("{}", format!("{} version(s)", total).dimmed());
    Ok(0)
}

async fn new_version(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client
        .post_empty(&format!("/api/v1/admin/workflows/{id}/versions"))
        .await?;
    let wf = &resp["data"];

    if quiet {
        println!("{}", wf["version"].as_i64().unwrap_or(0));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} New draft version {} created for workflow {}",
        "OK".green().bold(),
        wf["version"].as_i64().unwrap_or(0),
        wf["name"].as_str().unwrap_or(id)
    );
    Ok(0)
}

fn print_trace(trace: &Value, indent: usize) {
    let prefix = "  ".repeat(indent);
    if let Some(steps) = trace.get("steps").and_then(|s| s.as_array()) {
        for (i, step) in steps.iter().enumerate() {
            println!("{prefix}Step {}", i + 1);
            if let Some(obj) = step.as_object() {
                for (key, val) in obj {
                    if key == "steps" {
                        print_trace(step, indent + 1);
                    } else {
                        let val_str = if val.is_string() {
                            val.as_str().unwrap_or("").to_string()
                        } else {
                            serde_json::to_string(val).unwrap_or_default()
                        };
                        println!("{prefix}  {key}: {val_str}");
                    }
                }
            }
        }
    } else if let Some(obj) = trace.as_object() {
        for (key, val) in obj {
            let val_str = serde_json::to_string_pretty(val).unwrap_or_default();
            println!("{prefix}{key}: {val_str}");
        }
    }
}

async fn export(
    client: &OrionClient,
    status: &Option<String>,
    tag: &Option<String>,
) -> Result<i32> {
    let qs = utils::build_query_string(&[("status", status.clone()), ("tag", tag.clone())]);

    let resp: Value = client
        .get(&format!("/api/v1/admin/workflows/export{qs}"))
        .await?;
    let workflows = resp.get("data").unwrap_or(&resp);
    println!("{}", serde_json::to_string_pretty(workflows)?);
    Ok(0)
}

async fn import(client: &OrionClient, quiet: bool, file: &str, dry_run: bool) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let workflows: Value = serde_json::from_str(&content)?;

    if !workflows.is_array() {
        bail!("Import file must contain a JSON array of workflows");
    }

    let count = workflows.as_array().map(|a| a.len()).unwrap_or(0);

    if dry_run {
        println!(
            "{} Would import {} workflow(s) from {file}",
            "DRY RUN".yellow().bold(),
            count
        );
        if let Some(arr) = workflows.as_array() {
            for (i, wf) in arr.iter().enumerate() {
                let name = wf["name"].as_str().unwrap_or("(unnamed)");
                println!("  {}. {name}", i + 1);
            }
        }
        return Ok(0);
    }

    let resp: Value = client
        .post("/api/v1/admin/workflows/import", &workflows)
        .await?;

    if quiet {
        let imported = resp["imported"].as_u64().unwrap_or(0);
        println!("{imported}");
        return Ok(0);
    }

    let imported = resp["imported"].as_u64().unwrap_or(0);
    let failed = resp["failed"].as_u64().unwrap_or(0);

    println!(
        "{} Imported: {}, Failed: {}",
        if failed == 0 {
            "OK".green().bold()
        } else {
            "PARTIAL".yellow().bold()
        },
        imported.to_string().green(),
        if failed > 0 {
            failed.to_string().red().to_string()
        } else {
            "0".to_string()
        }
    );

    if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
        for err in errors {
            let idx = err["index"].as_u64().unwrap_or(0);
            let msg = err["error"].as_str().unwrap_or("unknown");
            println!("  {} Workflow #{idx}: {msg}", "ERR".red());
        }
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

async fn diff(client: &OrionClient, file: &str) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let local_workflows: Vec<Value> = serde_json::from_str(&content)?;

    let resp: Value = client.get("/api/v1/admin/workflows/export").await?;
    let server_workflows = resp["data"].as_array().cloned().unwrap_or_default();

    let mut new_count = 0;
    let mut modified_count = 0;
    let mut unchanged_count = 0;

    // Index server workflows by name
    let server_by_name: std::collections::HashMap<&str, &Value> = server_workflows
        .iter()
        .filter_map(|r| r["name"].as_str().map(|n| (n, r)))
        .collect();

    let local_names: std::collections::HashSet<&str> = local_workflows
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();

    println!("{}", "Workflow Diff".bold());
    println!();

    for local in &local_workflows {
        let name = local["name"].as_str().unwrap_or("(unnamed)");
        if let Some(server) = server_by_name.get(name) {
            let local_cond = local.get("condition").map(|v| v.to_string());
            let server_cond = server
                .get("condition_json")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let local_tasks = local.get("tasks").map(|v| v.to_string());
            let server_tasks = server
                .get("tasks_json")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if local_cond != server_cond || local_tasks != server_tasks {
                println!("  {} {name}", "~".yellow().bold());
                modified_count += 1;
            } else {
                println!("  {} {name}", "=".dimmed());
                unchanged_count += 1;
            }
        } else {
            println!("  {} {name}", "+".green().bold());
            new_count += 1;
        }
    }

    let mut deleted_count = 0;
    for server in &server_workflows {
        let name = server["name"].as_str().unwrap_or("");
        if !local_names.contains(name) {
            println!("  {} {name}", "-".red().bold());
            deleted_count += 1;
        }
    }

    println!();
    println!(
        "  {} new, {} modified, {} deleted, {} unchanged",
        new_count.to_string().green(),
        modified_count.to_string().yellow(),
        deleted_count.to_string().red(),
        unchanged_count
    );

    Ok(0)
}

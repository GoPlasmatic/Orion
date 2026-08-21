use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::{STATUS_ACTIVE, STATUS_ARCHIVED};
use serde_json::Value;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils::{self, colorize_status, truncate};
use orion_client::paths;

#[derive(Args)]
#[command(
    long_about = "Manage workflows -- processing pipelines that transform and route data.\n\n\
        Lifecycle: draft -> activate -> live\n\
        Workflows are created in draft status. Activating one reloads the engine automatically; pass --defer to batch several changes behind one reload.\n\n\
        With --quiet, list prints one ID per line, get prints the ID, and mutating commands print the resource ID or suppress output."
)]
pub struct WorkflowsCmd {
    #[command(subcommand)]
    command: WorkflowsSubcommand,
}

#[derive(Subcommand)]
enum WorkflowsSubcommand {
    /// List all workflows
    #[command(
        after_help = "The server pages at 50 by default; raise it with --limit (max 1000)\n\
            or walk pages with --offset.\n\n\
            Examples:\n  \
            orion-cli workflows list\n  \
            orion-cli workflows list --status active\n  \
            orion-cli workflows list --status draft --tag orders\n  \
            orion-cli workflows list --limit 200 --sort-by name --sort-order asc"
    )]
    List {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
        /// Sort by column (priority, name, status, created_at, updated_at)
        #[arg(long)]
        sort_by: Option<String>,
        /// Sort direction (asc, desc)
        #[arg(long)]
        sort_order: Option<String>,
    },
    /// Get a workflow by ID (use --verbose to see condition and tasks)
    Get {
        /// Workflow ID
        id: String,
    },
    /// Create a new workflow from JSON
    #[command(after_help = crate::help::WORKFLOW_CREATE)]
    Create {
        /// Custom workflow ID (auto-generated if omitted)
        #[arg(long)]
        id: Option<String>,
        /// Path to JSON file containing the workflow definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the workflow definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read workflow definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Update a workflow with new JSON definition
    Update {
        /// Workflow ID
        id: String,
        /// Path to JSON file containing the workflow definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the workflow definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read workflow definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Delete a workflow (prompts for confirmation)
    Delete {
        /// Workflow ID
        id: String,
    },
    /// Activate a draft workflow (the engine reloads automatically)
    Activate {
        /// Workflow ID
        id: String,
        /// Pre-flight only: report whether activation would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Archive an active workflow (the engine reloads automatically)
    Archive {
        /// Workflow ID
        id: String,
        /// Pre-flight only: report whether archiving would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Show what a workflow depends on: connectors and channel_call targets
    #[command(alias = "deps")]
    Dependencies {
        /// Workflow ID
        id: String,
    },
    /// Validate a workflow definition without creating it
    #[command(
        after_help = "Examples:\n  orion-cli workflows validate -f workflow.json\n  orion-cli workflows validate -d '{\"name\":\"test\",\"tasks\":[]}'"
    )]
    Validate {
        /// Path to JSON file containing the workflow definition
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON string with the workflow definition
        #[arg(short, long)]
        data: Option<String>,
        /// Read workflow definition from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Update rollout percentage for a workflow
    Rollout {
        /// Workflow ID
        id: String,
        /// Rollout percentage (0-100)
        #[arg(short, long)]
        percentage: i64,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// List version history for a workflow
    Versions {
        /// Workflow ID
        id: String,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Create a new draft version of a workflow
    NewVersion {
        /// Workflow ID
        id: String,
    },
    /// Test/dry-run a workflow with sample data
    #[command(
        after_help = "Examples:\n  orion-cli workflows test <id> -d '{\"key\": \"value\"}'\n  orion-cli workflows test <id> -f payload.json --trace\n  cat data.json | orion-cli workflows test <id> --stdin\n  orion-cli workflows test <id> -d '...' --metadata '{\"source\": \"test\"}'"
    )]
    Test {
        /// Workflow ID
        id: String,
        /// Path to JSON file with test payload
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON test data
        #[arg(short, long)]
        data: Option<String>,
        /// Read test data from stdin
        #[arg(long)]
        stdin: bool,
        /// Optional metadata JSON string
        #[arg(long)]
        metadata: Option<String>,
        /// Show execution trace of the dry-run
        #[arg(long)]
        trace: bool,
    },
    /// Export workflows as JSON (pipe to file for backup)
    #[command(
        after_help = "Examples:\n  orion-cli workflows export > workflows.json\n  orion-cli workflows export --status active > active-workflows.json"
    )]
    Export {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Import workflows from a JSON array file
    #[command(
        after_help = "Examples:\n  orion-cli workflows import -f workflows.json --dry-run\n  orion-cli workflows import -f workflows.json"
    )]
    Import {
        /// Path to JSON file containing a workflows array
        #[arg(short, long)]
        file: String,
        /// Preview what would be imported without making changes
        #[arg(long)]
        dry_run: bool,
        /// On ID conflict: fail (default), skip the item, or write a new version
        #[arg(long, value_parser = ["fail", "skip", "new_version"])]
        on_conflict: Option<String>,
    },
    /// Compare local file against server state
    #[command(after_help = "Examples:\n  orion-cli workflows diff -f workflows.json")]
    Diff {
        /// Path to JSON file with workflows to compare
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
            WorkflowsSubcommand::List {
                status,
                tag,
                limit,
                offset,
                sort_by,
                sort_order,
            } => {
                let qs = utils::build_query_string(&[
                    ("status", status.clone()),
                    ("tag", tag.clone()),
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                    ("sort_by", sort_by.clone()),
                    ("sort_order", sort_order.clone()),
                ]);
                list(client, format, quiet, &qs).await
            }
            WorkflowsSubcommand::Get { id } => {
                get_workflow(client, format, quiet, verbose, id).await
            }
            WorkflowsSubcommand::Create {
                id: custom_id,
                file,
                data,
                stdin,
            } => {
                let mut body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                if let Some(wid) = custom_id {
                    body["workflow_id"] = Value::String(wid.clone());
                }
                create(client, format, quiet, &body).await
            }
            WorkflowsSubcommand::Update {
                id,
                file,
                data,
                stdin,
            } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                update(client, format, quiet, id, &body).await
            }
            WorkflowsSubcommand::Delete { id } => delete(client, quiet, yes, id).await,
            WorkflowsSubcommand::Activate {
                id,
                dry_run,
                defer_reload,
            } => {
                change_status(
                    client,
                    format,
                    quiet,
                    id,
                    STATUS_ACTIVE,
                    *dry_run,
                    *defer_reload,
                )
                .await
            }
            WorkflowsSubcommand::Archive {
                id,
                dry_run,
                defer_reload,
            } => {
                change_status(
                    client,
                    format,
                    quiet,
                    id,
                    STATUS_ARCHIVED,
                    *dry_run,
                    *defer_reload,
                )
                .await
            }
            WorkflowsSubcommand::Dependencies { id } => {
                dependencies(client, format, quiet, id).await
            }
            WorkflowsSubcommand::Validate { file, data, stdin } => {
                let body = utils::read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                validate(client, format, quiet, &body).await
            }
            WorkflowsSubcommand::Rollout {
                id,
                percentage,
                defer_reload,
            } => rollout(client, quiet, id, *percentage, *defer_reload).await,
            WorkflowsSubcommand::Versions { id, limit, offset } => {
                let qs = utils::build_query_string(&[
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                ]);
                versions(client, format, quiet, id, &qs).await
            }
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
            WorkflowsSubcommand::Import {
                file,
                dry_run,
                on_conflict,
            } => {
                utils::run_import(
                    client,
                    format,
                    quiet,
                    paths::WORKFLOWS_IMPORT,
                    "workflow",
                    file,
                    *dry_run,
                    on_conflict.as_deref(),
                )
                .await
            }
            WorkflowsSubcommand::Diff { file } => diff(client, file).await,
        }
    }
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool, qs: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", paths::WORKFLOWS)).await?;
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
    utils::print_list_footer(&resp, workflows.len(), "workflow(s)");
    Ok(0)
}

async fn get_workflow(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    verbose: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::workflow(id)).await?;

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
    let resp: Value = client.post(paths::WORKFLOWS, body).await?;
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
    let resp: Value = client.put(&paths::workflow(id), body).await?;
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

    client.delete_request(&paths::workflow(id)).await?;

    if !quiet {
        println!("{} Workflow {id} deleted", "OK".green().bold());
    }
    Ok(0)
}

async fn change_status(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    status: &str,
    dry_run: bool,
    defer_reload: bool,
) -> Result<i32> {
    let qs = utils::build_query_string(&[
        ("dry_run", dry_run.then(|| "true".to_string())),
        ("reload", defer_reload.then(|| "defer".to_string())),
    ]);
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("{}{qs}", paths::workflow_status(id)), &body)
        .await?;

    // A dry run answers with the `/validate` envelope, not the entity — and a
    // transition that would be refused is reported as `valid: false` inside a
    // 200. Render the findings and exit non-zero, so a pre-flight that fails
    // cannot read as one that passed.
    if dry_run {
        return utils::print_validation_envelope(
            &resp,
            format,
            quiet,
            "DRY RUN",
            &format!("Workflow {id} can change to {status} (nothing written)"),
            &format!("Workflow {id} cannot change to {status}"),
        );
    }

    if !quiet {
        let wf = &resp["data"];
        println!(
            "{} Workflow {} status changed to {}",
            "OK".green().bold(),
            wf["name"].as_str().unwrap_or(id),
            colorize_status(status)
        );
        if defer_reload {
            println!("  Reload deferred -- run 'orion-cli engine reload' to apply.");
        }
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

    let resp: Value = client.post(&paths::workflow_test(id), &body).await?;
    // v1.0 wraps the WorkflowTestResult in the {"data": …} admin envelope.
    let resp = resp.get("data").cloned().unwrap_or(resp);

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

    if matched && let Some(output) = resp.get("output") {
        println!("\n{}", "Output:".bold());
        println!("{}", serde_json::to_string_pretty(output)?);
    }

    if let Some(errors) = resp.get("errors").and_then(|e| e.as_array())
        && !errors.is_empty()
    {
        println!("\n{}", "Errors:".yellow().bold());
        for err in errors {
            println!("  - {err}");
        }
    }

    if trace && let Some(trace_data) = resp.get("trace") {
        println!("\n{}", "Execution Trace:".bold());
        print_trace(trace_data, 1);
    }

    Ok(if matched { 0 } else { 1 })
}

async fn validate(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post(paths::WORKFLOWS_VALIDATE, body).await?;
    utils::print_validation_envelope(
        &resp,
        format,
        quiet,
        "OK",
        "Workflow definition is valid",
        "Workflow definition has issues",
    )
}

async fn rollout(
    client: &OrionClient,
    quiet: bool,
    id: &str,
    percentage: i64,
    defer_reload: bool,
) -> Result<i32> {
    let qs = utils::build_query_string(&[("reload", defer_reload.then(|| "defer".to_string()))]);
    let body = serde_json::json!({ "rollout_percentage": percentage });
    let resp: Value = client
        .patch(&format!("{}{qs}", paths::workflow_rollout(id)), &body)
        .await?;

    if !quiet {
        let wf = &resp["data"];
        println!(
            "{} Workflow {} rollout set to {}%",
            "OK".green().bold(),
            wf["name"].as_str().unwrap_or(id),
            percentage
        );
        if defer_reload {
            println!("  Reload deferred -- run 'orion-cli engine reload' to apply.");
        }
    }
    Ok(0)
}

async fn versions(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    qs: &str,
) -> Result<i32> {
    let resp: Value = client
        .get(&format!("{}{qs}", paths::workflow_versions(id)))
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
    utils::print_list_footer(&resp, vers.len(), "version(s)");
    Ok(0)
}

async fn new_version(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.post_empty(&paths::workflow_versions(id)).await?;
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
        .get(&format!("{}{qs}", paths::WORKFLOWS_EXPORT))
        .await?;
    let workflows = resp.get("data").unwrap_or(&resp);
    println!("{}", serde_json::to_string_pretty(workflows)?);
    Ok(0)
}

/// The key a bulk import collides on: `workflow_id`. An artifact without one
/// cannot conflict with anything — the store generates an id — so it falls
/// back to the name purely so the report can say which item it is.
fn diff_key(wf: &Value) -> Option<&str> {
    wf["workflow_id"].as_str().or_else(|| wf["name"].as_str())
}

/// The fields a re-import would actually write. Comparing anything else
/// (`version`, `status`, `created_at`, `content_hash`) reports drift for rows
/// that are byte-identical where it counts.
fn importable_content(wf: &Value) -> Value {
    serde_json::json!({
        "name": wf.get("name").cloned().unwrap_or(Value::Null),
        "description": wf.get("description").cloned().unwrap_or(Value::Null),
        "priority": wf.get("priority").cloned().unwrap_or(Value::from(0)),
        "condition": wf.get("condition").cloned().unwrap_or(Value::Bool(true)),
        "tasks": wf.get("tasks").cloned().unwrap_or(Value::Null),
        "tags": wf.get("tags").cloned().unwrap_or_else(|| Value::Array(vec![])),
        "loop": wf.get("loop").cloned().unwrap_or(Value::Null),
        "continue_on_error": wf.get("continue_on_error").cloned().unwrap_or(Value::Bool(false)),
    })
}

/// Whether the local artifact differs from the stored workflow.
///
/// `content_hash` is the server's own `sha256:` over the canonical importable
/// projection — "equal hashes mean importing one over the other is a no-op" —
/// so when both sides carry one, that is the exact answer. A hand-authored
/// file has no hash; those fall back to comparing the importable fields.
fn workflow_differs(local: &Value, server: &Value) -> bool {
    match (
        local["content_hash"].as_str(),
        server["content_hash"].as_str(),
    ) {
        (Some(l), Some(s)) => l != s,
        _ => importable_content(local) != importable_content(server),
    }
}

async fn diff(client: &OrionClient, file: &str) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let local_workflows: Vec<Value> = serde_json::from_str(&content)?;

    let resp: Value = client.get(paths::WORKFLOWS_EXPORT).await?;
    let server_workflows = resp["data"].as_array().cloned().unwrap_or_default();

    let mut new_count = 0;
    let mut modified_count = 0;
    let mut unchanged_count = 0;

    let server_by_key: std::collections::HashMap<&str, &Value> = server_workflows
        .iter()
        .filter_map(|r| diff_key(r).map(|k| (k, r)))
        .collect();

    let local_keys: std::collections::HashSet<&str> =
        local_workflows.iter().filter_map(diff_key).collect();

    println!("{}", "Workflow Diff".bold());
    println!();

    for local in &local_workflows {
        let label = local["name"].as_str().unwrap_or("(unnamed)");
        match diff_key(local).and_then(|k| server_by_key.get(k)) {
            Some(server) => {
                if workflow_differs(local, server) {
                    println!("  {} {label}", "~".yellow().bold());
                    modified_count += 1;
                } else {
                    println!("  {} {label}", "=".dimmed());
                    unchanged_count += 1;
                }
            }
            None => {
                println!("  {} {label}", "+".green().bold());
                new_count += 1;
            }
        }
    }

    let mut deleted_count = 0;
    for server in &server_workflows {
        let Some(key) = diff_key(server) else {
            continue;
        };
        if !local_keys.contains(key) {
            println!(
                "  {} {}",
                "-".red().bold(),
                server["name"].as_str().unwrap_or(key)
            );
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

    // A diff that finds drift is worth branching on: exit 1 when the file and
    // the server disagree, matching `orion-server package diff`.
    Ok(i32::from(
        new_count > 0 || modified_count > 0 || deleted_count > 0,
    ))
}

async fn dependencies(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::workflow_dependencies(id)).await?;
    let deps = &resp["data"];

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let connectors = deps["connectors"].as_array().cloned().unwrap_or_default();
    let channels = deps["channels"].as_array().cloned().unwrap_or_default();

    if quiet {
        for c in &connectors {
            if let Some(name) = c["connector"].as_str() {
                println!("{name}");
            }
        }
        return Ok(0);
    }

    println!("{}", "Workflow Dependencies".bold());
    println!(
        "  Workflow: {} (v{})",
        deps["workflow_id"].as_str().unwrap_or(id),
        deps["version"].as_i64().unwrap_or(0)
    );

    if connectors.is_empty() {
        println!("  Connectors: {}", "(none)".dimmed());
    } else {
        println!("  Connectors:");
        for c in &connectors {
            println!(
                "    - {} (via {})",
                c["connector"].as_str().unwrap_or(""),
                c["function"].as_str().unwrap_or("")
            );
        }
    }

    if channels.is_empty() {
        println!("  Channel calls: {}", "(none)".dimmed());
    } else {
        println!("  Channel calls:");
        for ch in &channels {
            println!("    - {}", ch.as_str().unwrap_or(""));
        }
    }

    if deps["has_dynamic_channel_calls"].as_bool().unwrap_or(false) {
        println!(
            "  {} channel_call targets are resolved at runtime -- the list above is incomplete by construction",
            "NOTE".yellow()
        );
    }

    Ok(0)
}

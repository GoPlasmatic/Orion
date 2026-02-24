use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;
use std::io::Read;
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

#[derive(Args)]
pub struct RulesCmd {
    #[command(subcommand)]
    command: RulesSubcommand,
}

#[derive(Subcommand)]
enum RulesSubcommand {
    /// List all rules
    List {
        /// Filter by status (active, paused, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by channel
        #[arg(long)]
        channel: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Get a rule by ID
    Get {
        /// Rule ID
        id: String,
    },
    /// Create a new rule
    Create {
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Update a rule
    Update {
        /// Rule ID
        id: String,
        /// JSON file path
        #[arg(short, long)]
        file: Option<String>,
        /// Inline JSON data
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Delete a rule
    Delete {
        /// Rule ID
        id: String,
    },
    /// Activate a rule
    Activate {
        /// Rule ID
        id: String,
    },
    /// Pause a rule
    Pause {
        /// Rule ID
        id: String,
    },
    /// Archive a rule
    Archive {
        /// Rule ID
        id: String,
    },
    /// Test/dry-run a rule with data
    Test {
        /// Rule ID
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
    /// Export rules
    Export {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Filter by channel
        #[arg(long)]
        channel: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Import rules from file
    Import {
        /// JSON file with rules array
        #[arg(short, long)]
        file: String,
        /// Preview without importing
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare local file against server state
    Diff {
        /// JSON file with rules
        #[arg(short, long)]
        file: String,
    },
}

#[derive(Tabled)]
struct RuleRow {
    #[tabled(rename = "ID")]
    id: String,
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Channel")]
    channel: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Priority")]
    priority: i64,
    #[tabled(rename = "Version")]
    version: i64,
}

impl RulesCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            RulesSubcommand::List {
                status,
                channel,
                tag,
            } => list(client, format, quiet, status, channel, tag).await,
            RulesSubcommand::Get { id } => get_rule(client, format, quiet, verbose, id).await,
            RulesSubcommand::Create { file, data } => {
                let body = read_json_input(file.as_deref(), data.as_deref(), false)?;
                create(client, format, quiet, &body).await
            }
            RulesSubcommand::Update { id, file, data } => {
                let body = read_json_input(file.as_deref(), data.as_deref(), false)?;
                update(client, format, quiet, id, &body).await
            }
            RulesSubcommand::Delete { id } => delete(client, quiet, yes, id).await,
            RulesSubcommand::Activate { id } => change_status(client, quiet, id, "active").await,
            RulesSubcommand::Pause { id } => change_status(client, quiet, id, "paused").await,
            RulesSubcommand::Archive { id } => change_status(client, quiet, id, "archived").await,
            RulesSubcommand::Test {
                id,
                file,
                data,
                stdin,
                metadata,
                trace,
            } => {
                let payload = read_json_input(file.as_deref(), data.as_deref(), *stdin)?;
                let meta = metadata.as_deref().map(serde_json::from_str).transpose()?;
                test_rule(client, format, quiet, id, &payload, meta.as_ref(), *trace).await
            }
            RulesSubcommand::Export {
                status,
                channel,
                tag,
            } => export(client, status, channel, tag).await,
            RulesSubcommand::Import { file, dry_run } => {
                import(client, quiet, file, *dry_run).await
            }
            RulesSubcommand::Diff { file } => diff(client, file).await,
        }
    }
}

async fn list(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    status: &Option<String>,
    channel: &Option<String>,
    tag: &Option<String>,
) -> Result<i32> {
    let mut query = Vec::new();
    if let Some(s) = status {
        query.push(format!("status={s}"));
    }
    if let Some(c) = channel {
        query.push(format!("channel={c}"));
    }
    if let Some(t) = tag {
        query.push(format!("tag={t}"));
    }
    let qs = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };

    let resp: Value = client.get(&format!("/api/v1/admin/rules{qs}")).await?;
    let rules = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for rule in &rules {
            if let Some(id) = rule["id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    if rules.is_empty() {
        println!("{}", "No rules found.".dimmed());
        return Ok(0);
    }

    let rows: Vec<RuleRow> = rules
        .iter()
        .map(|r| RuleRow {
            id: truncate(r["id"].as_str().unwrap_or(""), 12),
            name: truncate(r["name"].as_str().unwrap_or(""), 30),
            channel: r["channel"].as_str().unwrap_or("").to_string(),
            status: colorize_status(r["status"].as_str().unwrap_or("")),
            priority: r["priority"].as_i64().unwrap_or(0),
            version: r["version"].as_i64().unwrap_or(0),
        })
        .collect();

    output::print_table(rows);
    let total = resp["total"].as_i64().unwrap_or(rules.len() as i64);
    println!("{}", format!("{} rule(s)", total).dimmed());
    Ok(0)
}

async fn get_rule(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    verbose: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&format!("/api/v1/admin/rules/{id}")).await?;

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    let rule = &resp["data"];
    let version_count = resp["version_count"].as_i64().unwrap_or(0);

    println!("{}", "Rule Details".bold());
    println!("  ID:          {}", rule["id"].as_str().unwrap_or(""));
    println!("  Name:        {}", rule["name"].as_str().unwrap_or(""));
    println!(
        "  Description: {}",
        rule["description"].as_str().unwrap_or("(none)")
    );
    println!("  Channel:     {}", rule["channel"].as_str().unwrap_or(""));
    println!(
        "  Status:      {}",
        colorize_status(rule["status"].as_str().unwrap_or(""))
    );
    println!("  Priority:    {}", rule["priority"].as_i64().unwrap_or(0));
    println!(
        "  Version:     {} ({version_count} total)",
        rule["version"].as_i64().unwrap_or(0)
    );
    println!("  Tags:        {}", rule["tags"]);
    println!(
        "  Continue on error: {}",
        rule["continue_on_error"].as_bool().unwrap_or(false)
    );
    println!("  Created:     {}", rule["created_at"]);
    println!("  Updated:     {}", rule["updated_at"]);

    if verbose {
        println!("\n{}", "Condition:".bold());
        if let Ok(cond) =
            serde_json::from_str::<Value>(rule["condition_json"].as_str().unwrap_or("true"))
        {
            println!("{}", serde_json::to_string_pretty(&cond)?);
        } else {
            println!("{}", rule["condition_json"]);
        }
        println!("\n{}", "Tasks:".bold());
        if let Ok(tasks) =
            serde_json::from_str::<Value>(rule["tasks_json"].as_str().unwrap_or("[]"))
        {
            println!("{}", serde_json::to_string_pretty(&tasks)?);
        } else {
            println!("{}", rule["tasks_json"]);
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
    let resp: Value = client.post("/api/v1/admin/rules", body).await?;
    let rule = &resp["data"];

    if quiet {
        println!("{}", rule["id"].as_str().unwrap_or(""));
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Rule created: {} ({})",
        "OK".green().bold(),
        rule["name"].as_str().unwrap_or(""),
        rule["id"].as_str().unwrap_or("")
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
        .put(&format!("/api/v1/admin/rules/{id}"), body)
        .await?;
    let rule = &resp["data"];

    if quiet {
        println!("{id}");
        return Ok(0);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }

    println!(
        "{} Rule updated: {} (v{})",
        "OK".green().bold(),
        rule["name"].as_str().unwrap_or(""),
        rule["version"].as_i64().unwrap_or(0)
    );
    Ok(0)
}

async fn delete(client: &OrionClient, quiet: bool, yes: bool, id: &str) -> Result<i32> {
    if !yes {
        eprint!("Delete rule {id}? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(0);
        }
    }

    client
        .delete_request(&format!("/api/v1/admin/rules/{id}"))
        .await?;

    if !quiet {
        println!("{} Rule {id} deleted", "OK".green().bold());
    }
    Ok(0)
}

async fn change_status(client: &OrionClient, quiet: bool, id: &str, status: &str) -> Result<i32> {
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("/api/v1/admin/rules/{id}/status"), &body)
        .await?;

    if !quiet {
        let rule = &resp["data"];
        println!(
            "{} Rule {} status changed to {}",
            "OK".green().bold(),
            rule["name"].as_str().unwrap_or(id),
            colorize_status(status)
        );
    }
    Ok(0)
}

async fn test_rule(
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
        .post(&format!("/api/v1/admin/rules/{id}/test"), &body)
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
    println!("  Rule:    {id}");
    println!("  Result:  {match_display}");

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
    channel: &Option<String>,
    tag: &Option<String>,
) -> Result<i32> {
    let mut query = Vec::new();
    if let Some(s) = status {
        query.push(format!("status={s}"));
    }
    if let Some(c) = channel {
        query.push(format!("channel={c}"));
    }
    if let Some(t) = tag {
        query.push(format!("tag={t}"));
    }
    let qs = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };

    let resp: Value = client
        .get(&format!("/api/v1/admin/rules/export{qs}"))
        .await?;
    let rules = resp.get("data").unwrap_or(&resp);
    println!("{}", serde_json::to_string_pretty(rules)?);
    Ok(0)
}

async fn import(client: &OrionClient, quiet: bool, file: &str, dry_run: bool) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let rules: Value = serde_json::from_str(&content)?;

    if !rules.is_array() {
        bail!("Import file must contain a JSON array of rules");
    }

    let count = rules.as_array().map(|a| a.len()).unwrap_or(0);

    if dry_run {
        println!(
            "{} Would import {} rule(s) from {file}",
            "DRY RUN".yellow().bold(),
            count
        );
        if let Some(arr) = rules.as_array() {
            for (i, rule) in arr.iter().enumerate() {
                let name = rule["name"].as_str().unwrap_or("(unnamed)");
                let channel = rule["channel"].as_str().unwrap_or("default");
                println!("  {}. {name} (channel: {channel})", i + 1);
            }
        }
        return Ok(0);
    }

    let resp: Value = client.post("/api/v1/admin/rules/import", &rules).await?;

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
            println!("  {} Rule #{idx}: {msg}", "ERR".red());
        }
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

async fn diff(client: &OrionClient, file: &str) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let local_rules: Vec<Value> = serde_json::from_str(&content)?;

    let resp: Value = client.get("/api/v1/admin/rules/export").await?;
    let server_rules = resp["data"].as_array().cloned().unwrap_or_default();

    let mut new_count = 0;
    let mut modified_count = 0;
    let mut unchanged_count = 0;

    // Index server rules by name
    let server_by_name: std::collections::HashMap<&str, &Value> = server_rules
        .iter()
        .filter_map(|r| r["name"].as_str().map(|n| (n, r)))
        .collect();

    let local_names: std::collections::HashSet<&str> = local_rules
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();

    println!("{}", "Rule Diff".bold());
    println!();

    for local in &local_rules {
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
    for server in &server_rules {
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

fn read_json_input(file: Option<&str>, data: Option<&str>, stdin: bool) -> Result<Value> {
    if let Some(path) = file {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    } else if let Some(json) = data {
        Ok(serde_json::from_str(json)?)
    } else if stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(serde_json::from_str(&buf)?)
    } else {
        bail!("Provide input with -f <file>, -d '<json>', or --stdin")
    }
}

fn colorize_status(status: &str) -> String {
    match status {
        "active" => "active".green().to_string(),
        "paused" => "paused".yellow().to_string(),
        "archived" => "archived".dimmed().to_string(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max - 3])
    } else {
        s.to_string()
    }
}

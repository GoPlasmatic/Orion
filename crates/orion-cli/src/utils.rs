use anyhow::{Result, bail};
use colored::Colorize;
use serde_json::Value;
use std::io::Read;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};

/// Truncate a string to `max` characters, appending "..." if truncated.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max - 3])
    } else {
        s.to_string()
    }
}

/// Colorize a lifecycle status (draft/active/archived).
pub fn colorize_status(status: &str) -> String {
    match status {
        "active" | "completed" | "ok" => status.green().to_string(),
        "draft" | "running" | "pending" => {
            if status == "pending" {
                status.yellow().to_string()
            } else {
                status.blue().to_string()
            }
        }
        "failed" => status.red().to_string(),
        "archived" => status.dimmed().to_string(),
        other => other.to_string(),
    }
}

/// Format seconds into a human-readable duration string.
pub fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else if seconds < 86400 {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}d {}h", seconds / 86400, (seconds % 86400) / 3600)
    }
}

/// Prompt for confirmation. Returns `true` if the user confirms or `yes` is set.
pub fn confirm(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    eprint!("{prompt} [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

/// Read JSON input from a file path, an inline string, or stdin.
pub fn read_json_input(file: Option<&str>, data: Option<&str>, stdin: bool) -> Result<Value> {
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

/// Read a JSON-array file and POST it to a bulk-import endpoint, printing a
/// summary of the outcome. Shared by `workflows`/`channels`/`connectors import`.
///
/// `base_path` is the import endpoint (e.g. `/api/v1/admin/channels/import`),
/// `label` the singular resource noun (e.g. `channel`). With `dry_run`, appends
/// `?dry_run=true` so the server validates without writing and returns
/// `would_create`/`would_fail`; otherwise it reports `imported`/`failed`.
/// Returns exit code 1 when any item failed (or would fail).
pub async fn run_import(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    base_path: &str,
    label: &str,
    file: &str,
    dry_run: bool,
) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let items: Value = serde_json::from_str(&content)?;
    if !items.is_array() {
        bail!("Import file must contain a JSON array of {label}s");
    }

    let path = if dry_run {
        format!("{base_path}?dry_run=true")
    } else {
        base_path.to_string()
    };
    let resp: Value = client.post(&path, &items).await?;

    let is_dry = resp["dry_run"].as_bool().unwrap_or(dry_run);
    let (success, fail) = if is_dry {
        (
            resp["would_create"].as_u64().unwrap_or(0),
            resp["would_fail"].as_u64().unwrap_or(0),
        )
    } else {
        (
            resp["imported"].as_u64().unwrap_or(0),
            resp["failed"].as_u64().unwrap_or(0),
        )
    };
    let exit = if fail > 0 { 1 } else { 0 };

    if quiet {
        println!("{success}");
        return Ok(exit);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(exit);
    }

    let tag = if is_dry {
        "DRY RUN".yellow().bold()
    } else if fail == 0 {
        "OK".green().bold()
    } else {
        "PARTIAL".yellow().bold()
    };
    let verb = if is_dry { "Would import" } else { "Imported" };
    println!(
        "{tag} {verb}: {}, Failed: {}",
        success.to_string().green(),
        if fail > 0 {
            fail.to_string().red().to_string()
        } else {
            "0".to_string()
        }
    );

    if let Some(errors) = resp.get("errors").and_then(|e| e.as_array()) {
        for err in errors {
            let idx = err["index"].as_u64().unwrap_or(0);
            let msg = err["error"].as_str().unwrap_or("unknown");
            println!("  {} #{idx}: {msg}", "ERR".red());
        }
    }

    Ok(exit)
}

/// Build a URL query string from key-value pairs, skipping `None` values.
pub fn build_query_string(params: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = params
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|val| format!("{k}={val}")))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

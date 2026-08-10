use anyhow::{Result, bail};
use colored::Colorize;
use orion_api::{
    ImportResult, STATUS_ACTIVE, STATUS_ARCHIVED, STATUS_DRAFT, TRACE_STATUS_COMPLETED,
    TRACE_STATUS_FAILED, TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING,
};
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

/// Colorize an entity lifecycle status (draft/active/archived) or a trace
/// status (pending/running/completed/failed), matched against the shared
/// `orion-api` vocabulary. `"ok"` is CLI-local: the health command's summary.
pub fn colorize_status(status: &str) -> String {
    match status {
        STATUS_ACTIVE | TRACE_STATUS_COMPLETED | "ok" => status.green().to_string(),
        TRACE_STATUS_PENDING => status.yellow().to_string(),
        STATUS_DRAFT | TRACE_STATUS_RUNNING => status.blue().to_string(),
        TRACE_STATUS_FAILED => status.red().to_string(),
        STATUS_ARCHIVED => status.dimmed().to_string(),
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
/// `?dry_run=true` so the server validates without writing. `on_conflict`
/// (`fail`/`skip`/`new_version`) rides along as `?on_conflict=`. The v1.0
/// server wraps the result in the `{"data": …}` admin envelope and reports
/// `imported`/`unchanged`/`skipped`/`failed` in both real and dry-run modes.
/// Returns exit code 1 when any item failed (or would fail).
#[allow(clippy::too_many_arguments)]
pub async fn run_import(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    base_path: &str,
    label: &str,
    file: &str,
    dry_run: bool,
    on_conflict: Option<&str>,
) -> Result<i32> {
    let content = std::fs::read_to_string(file)?;
    let items: Value = serde_json::from_str(&content)?;
    if !items.is_array() {
        bail!("Import file must contain a JSON array of {label}s");
    }

    let qs = build_query_string(&[
        ("dry_run", dry_run.then(|| "true".to_string())),
        ("on_conflict", on_conflict.map(str::to_string)),
    ]);
    let resp: Value = client.post(&format!("{base_path}{qs}"), &items).await?;
    // v1.0 wraps admin responses in {"data": …}; tolerate the bare pre-1.0
    // shape so the CLI still reads older servers.
    let result = resp.get("data").cloned().unwrap_or(resp);

    // The typed report from orion-api — the same type the server serializes.
    // Every field defaults, so a pre-1.0 report still parses; a response that
    // isn't even object-shaped falls back to all-zero counts, exactly as the
    // per-field `unwrap_or(0)` reads did before.
    let report: ImportResult = serde_json::from_value(result.clone()).unwrap_or_default();
    // A pre-1.0 report without the field echoes what was requested.
    let is_dry = report.dry_run || dry_run;
    let success = report.imported;
    let fail = report.failed;
    let unchanged = report.unchanged;
    let skipped = report.skipped;
    let exit = if fail > 0 { 1 } else { 0 };

    if quiet {
        println!("{success}");
        return Ok(exit);
    }

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &result)?;
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
    let mut extras = Vec::new();
    if unchanged > 0 {
        extras.push(format!("{unchanged} unchanged"));
    }
    if skipped > 0 {
        extras.push(format!("{skipped} skipped"));
    }
    let extras = if extras.is_empty() {
        String::new()
    } else {
        format!(" ({})", extras.join(", "))
    };
    println!(
        "{tag} {verb}: {}{extras}, Failed: {}",
        success.to_string().green(),
        if fail > 0 {
            fail.to_string().red().to_string()
        } else {
            "0".to_string()
        }
    );

    for err in &report.errors {
        println!("  {} #{}: {}", "ERR".red(), err.index, err.error);
    }

    Ok(exit)
}

/// Build a URL query string from key-value pairs, skipping `None` values.
/// The one implementation lives in `orion-client`; this re-export keeps the
/// CLI's historical call sites working.
pub use orion_client::query_string as build_query_string;

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

/// Truncate a string to at most `max` bytes, appending "..." if truncated.
/// The cut lands on a char boundary — names, descriptions, and upstream
/// error messages are user-authored, and a raw byte slice panics when the
/// index falls inside a multi-byte UTF-8 character.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max.saturating_sub(3);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
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

/// Render the `{"valid", "errors", "warnings"}` validation envelope and return
/// the process exit code: 0 when valid, 1 when not.
///
/// Two endpoints answer in this shape and both matter: `POST /{kind}/validate`,
/// and `PATCH /{kind}/{id}/status?dry_run=true` — the activation pre-flight.
/// The pre-flight deliberately reports a refused transition as `valid: false`
/// inside a **200** (so a promotion can pre-flight a whole package without
/// stopping at the first missing entity), which means a caller that reads only
/// the HTTP status sees a failing pre-flight as a passing one.
pub fn print_validation_envelope(
    resp: &Value,
    format: &OutputFormat,
    quiet: bool,
    label: &str,
    ok_msg: &str,
    bad_msg: &str,
) -> Result<i32> {
    let resp = resp.get("data").unwrap_or(resp);
    let valid = resp["valid"].as_bool().unwrap_or(false);
    let code = if valid { 0 } else { 1 };

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, resp)?;
        return Ok(code);
    }

    if quiet {
        println!("{}", if valid { "valid" } else { "invalid" });
        return Ok(code);
    }

    if valid {
        println!("{} {ok_msg}", label.green().bold());
    } else {
        println!("{} {bad_msg}", "INVALID".red().bold());
    }

    if let Some(errors) = resp["errors"].as_array()
        && !errors.is_empty()
    {
        println!("\n{}", "Errors:".red().bold());
        for err in errors {
            let field = err["field"].as_str().unwrap_or("");
            let msg = err["message"].as_str().unwrap_or("");
            println!("  - {field}: {msg}");
        }
    }

    if let Some(warnings) = resp["warnings"].as_array()
        && !warnings.is_empty()
    {
        println!("\n{}", "Warnings:".yellow().bold());
        for warn in warnings {
            let field = warn["field"].as_str().unwrap_or("");
            let msg = warn["message"].as_str().unwrap_or("");
            println!("  - {field}: {msg}");
        }
    }

    Ok(code)
}

/// Print the count line under a table, from an admin list envelope. `noun` is
/// the already-pluralised label, e.g. `"workflow(s)"`.
///
/// Every list endpoint pages — 50 rows by default, 1000 at most — and reports
/// the unpaged `total` alongside the page. Printing that total alone under a
/// short page reads as "these are all of them", so when the page does not
/// reach the total this says which is which and names the flags that fetch
/// the rest.
pub fn print_list_footer(resp: &Value, shown: usize, noun: &str) {
    let total = resp["total"].as_i64().unwrap_or(shown as i64);
    if total > shown as i64 {
        println!(
            "{}",
            format!("Showing {shown} of {total} {noun} -- page with --limit / --offset").dimmed()
        );
    } else {
        println!("{}", format!("{total} {noun}").dimmed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate("short", 25), "short");
        assert_eq!(truncate("abcdefghij", 8), "abcde...");
    }

    #[test]
    fn truncate_backs_off_to_a_char_boundary() {
        // 21 ASCII bytes, then 'é' (2 bytes) spanning indices 21–22: a raw
        // byte slice at 22 panics with "not a char boundary".
        let s = "aaaaaaaaaaaaaaaaaaaaaéxxxx";
        assert_eq!(truncate(s, 25), "aaaaaaaaaaaaaaaaaaaaa...");
        // Cut inside a 4-byte emoji.
        assert_eq!(truncate("ab🦀cdefgh", 6), "ab...");
        // Degenerate budgets must not underflow or panic.
        assert_eq!(truncate("🦀🦀", 2), "...");
    }
}

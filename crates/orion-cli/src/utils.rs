use anyhow::{Result, bail};
use colored::Colorize;
use serde_json::Value;
use std::io::Read;

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

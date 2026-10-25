use anyhow::{Result, bail};
use colored::Colorize;
use orion_api::{
    STATUS_ACTIVE, STATUS_ARCHIVED, STATUS_DRAFT, TRACE_STATUS_COMPLETED, TRACE_STATUS_FAILED,
    TRACE_STATUS_PENDING, TRACE_STATUS_RUNNING,
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
/// Build a URL query string from key-value pairs, skipping `None` values.
/// The one implementation lives in `orion-client`; this re-export keeps the
/// CLI's historical call sites working.
pub use orion_client::query_string as build_query_string;

/// Which trace to wait on, and how long to keep asking.
///
/// Grouped so `interval` and `timeout` — both bare `u64` seconds — cannot be
/// swapped at the call site without the compiler noticing.
pub struct WaitOptions<'a> {
    pub id: &'a str,
    pub token: Option<&'a str>,
    pub interval: u64,
    pub timeout: u64,
}

/// Poll a trace to a terminal state and render it. **Exit 0 completed,
/// 1 failed, 2 timed out.**
///
/// One implementation for `traces wait` and `data send --wait`, which polled
/// the same endpoint through the same states and rendered the same three
/// outcomes — and had drifted on all three of the things that distinguish
/// them:
///
/// - **the timeout code.** `traces wait` documented and returned 2; `send`
///   returned 2 from its human branch and 1 from its JSON branch, where the
///   early return ran before the status was examined. Under `--output json` a
///   timeout was indistinguishable from a failed trace.
/// - **what `--output json` prints.** `traces wait` gated its terminal branch
///   on the format but not its timeout branch, so a timeout wrote a human
///   `TIMEOUT` line to *stdout* and no JSON at all — a caller piping to `jq`
///   got a parse error rather than a document.
/// - **the token.** `send` interpolated it into the query string raw where
///   `traces wait` percent-encoded it.
///
/// So: the machine-readable formats always emit the trace and never the
/// human line, on every exit including the timeout; the token always goes
/// through `query_string`; and the timeout is always 2.
///
/// Progress goes to stderr, so `--output json` piped to a parser stays clean
/// while a human watching the terminal still sees it.
pub async fn wait_for_trace(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    opts: WaitOptions<'_>,
) -> Result<i32> {
    let WaitOptions {
        id,
        token,
        interval,
        timeout,
    } = opts;
    let qs = build_query_string(&[("token", token.map(str::to_string))]);
    let path = format!("{}{qs}", orion_client::paths::trace(id));
    let start = std::time::Instant::now();
    let timeout_dur = std::time::Duration::from_secs(timeout);
    let interval_dur = std::time::Duration::from_secs(interval);

    if !quiet {
        eprint!("Waiting for trace {id}...");
    }

    let (resp, timed_out) = loop {
        let resp: Value = client.get(&path).await?;
        // v1.0 wraps the trace in the `{"data": …}` admin envelope; tolerate
        // the bare pre-1.0 shape.
        let resp = resp.get("data").cloned().unwrap_or(resp);
        let status = resp["status"].as_str().unwrap_or("unknown");

        if status == "completed" || status == "failed" {
            break (resp, false);
        }
        if start.elapsed() >= timeout_dur {
            break (resp, true);
        }
        tokio::time::sleep(interval_dur).await;
    };

    if !quiet {
        eprintln!();
    }

    let status = resp["status"].as_str().unwrap_or("unknown");
    let code = if timed_out {
        2
    } else if status == "failed" {
        1
    } else {
        0
    };

    // Every exit, including the timeout: a machine-readable format emits the
    // trace and nothing else.
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(code);
    }

    if quiet {
        return Ok(code);
    }

    if timed_out {
        println!(
            "{} Timed out after {timeout}s (status: {status})",
            "TIMEOUT".yellow().bold()
        );
    } else if status == "failed" {
        let err = resp["error_message"]
            .as_str()
            .or(resp["error"].as_str())
            .unwrap_or("Unknown error");
        println!("{} Trace failed: {err}", "ERR".red().bold());
    } else {
        println!("{} Trace completed", "OK".green().bold());
        if let Some(msg) = resp.get("message") {
            println!("{}", serde_json::to_string_pretty(msg)?);
        } else if let Some(result) = resp.get("result_json").and_then(|r| r.as_str())
            && let Ok(parsed) = serde_json::from_str::<Value>(result)
        {
            println!("{}", serde_json::to_string_pretty(&parsed)?);
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
#[allow(clippy::unwrap_used, clippy::panic)]
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

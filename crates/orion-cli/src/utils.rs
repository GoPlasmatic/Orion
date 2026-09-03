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
/// One import's worth of "what is being imported, from where, and how".
///
/// Grouped because `base_path`, `label` and `file` are three consecutive
/// `&str`: transposing any two of them compiles and imports the wrong thing
/// under the wrong name.
pub struct ImportRequest<'a> {
    /// The admin endpoint the array is POSTed to.
    pub base_path: &'a str,
    /// Singular noun for the entity, used in messages ("connector", "channel").
    pub label: &'a str,
    /// Path to the JSON file holding the array.
    pub file: &'a str,
    pub dry_run: bool,
    pub on_conflict: Option<&'a str>,
}

pub async fn run_import(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    req: ImportRequest<'_>,
) -> Result<i32> {
    let ImportRequest {
        base_path,
        label,
        file,
        dry_run,
        on_conflict,
    } = req;
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

/// Everything that distinguishes one admin entity from another, in one value.
///
/// The CRUD commands for workflows, channels and connectors differ only in a
/// noun and a set of endpoints — `create`/`update`/`delete`/`export` were
/// token-identical across the three modules, and had drifted where nothing
/// forced them to agree. Grouped for the reason [`ImportRequest`] is grouped,
/// one step further out: `collection`, `export` and `validate` are three
/// `&'static str` and `item` is one of a family of interchangeable
/// `fn(&str) -> String`, so any two of them transpose without a compile error.
/// Written once, in the entity's own command module, they cannot.
pub struct EntityKind {
    /// Capitalised singular, for human output: `"Channel"`.
    pub title: &'static str,
    /// Lowercase singular, for prompts: `"channel"`.
    pub label: &'static str,
    pub collection: &'static str,
    pub export: &'static str,
    pub validate: &'static str,
    pub item: fn(&str) -> String,
    /// The response field carrying the id. Not uniform across the API:
    /// workflows and channels answer with `workflow_id`/`channel_id`,
    /// connectors with a bare `id`.
    pub id_field: &'static str,
}

/// The two endpoints only a *versioned* entity has.
///
/// Separate from [`EntityKind`] rather than `Option` fields inside it:
/// connectors have no status transition and no version history, and there is
/// no `paths::connector_status` to point at. Taking this type in the versioned
/// commands means a connector cannot reach them — the mistake is a type error
/// rather than a runtime `None` branch nobody would write a test for.
pub struct VersionedEntityKind {
    pub entity: &'static EntityKind,
    pub status: fn(&str) -> String,
    pub versions: fn(&str) -> String,
}

/// One status transition's worth of "which entity, to what, and for real?".
///
/// Grouped because the two trailing flags are both `bool` and one of them
/// means "do not actually write": transposing them turns a pre-flight into a
/// live transition and still compiles.
pub struct StatusChange<'a> {
    pub id: &'a str,
    pub status: &'a str,
    pub dry_run: bool,
    pub defer_reload: bool,
}

/// `POST /{collection}` — create an entity from a request body.
pub async fn create_entity(
    client: &OrionClient,
    kind: &EntityKind,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post(kind.collection, body).await?;
    let entity = &resp["data"];
    let id = entity[kind.id_field].as_str().unwrap_or("");

    if quiet {
        println!("{id}");
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    println!(
        "{} {} created: {} ({id})",
        "OK".green().bold(),
        kind.title,
        entity["name"].as_str().unwrap_or(""),
    );
    Ok(0)
}

/// `PUT /{collection}/{id}` — replace an entity's content.
pub async fn update_entity(
    client: &OrionClient,
    kind: &EntityKind,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.put(&(kind.item)(id), body).await?;
    let entity = &resp["data"];

    if quiet {
        println!("{id}");
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    // A versioned entity's update may or may not have cut a new version, and
    // which it did is the first thing a caller wants to know. A connector has
    // no version at all, so the suffix is driven by the response rather than
    // by a flag — `unwrap_or(0)` here would have invented "(v0)" for one.
    let version = entity["version"]
        .as_i64()
        .map(|v| format!(" (v{v})"))
        .unwrap_or_default();
    println!(
        "{} {} updated: {}{version}",
        "OK".green().bold(),
        kind.title,
        entity["name"].as_str().unwrap_or(id),
    );
    Ok(0)
}

/// `DELETE /{collection}/{id}`, behind a confirmation.
pub async fn delete_entity(
    client: &OrionClient,
    kind: &EntityKind,
    quiet: bool,
    yes: bool,
    id: &str,
) -> Result<i32> {
    if !confirm(&format!("Delete {} {id}?", kind.label), yes)? {
        println!("Cancelled.");
        return Ok(0);
    }
    client.delete_request(&(kind.item)(id)).await?;
    if !quiet {
        println!("{} {} {id} deleted", "OK".green().bold(), kind.title);
    }
    Ok(0)
}

/// `POST /{collection}/validate` — check a definition without storing it.
pub async fn validate_entity(
    client: &OrionClient,
    kind: &EntityKind,
    format: &OutputFormat,
    quiet: bool,
    body: &Value,
) -> Result<i32> {
    let resp: Value = client.post(kind.validate, body).await?;
    print_validation_envelope(
        &resp,
        format,
        quiet,
        "OK",
        &format!("{} definition is valid", kind.title),
        &format!("{} definition has issues", kind.title),
    )
}

/// `GET /{collection}/export{qs}` — the artifact array, pretty-printed.
///
/// The filters stay at the call site: each entity exposes a different set
/// (channels filter on protocol and type, workflows on status, connectors on
/// tag alone), and they arrive here already built by [`build_query_string`].
pub async fn export_entities(client: &OrionClient, kind: &EntityKind, qs: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", kind.export)).await?;
    let items = resp.get("data").unwrap_or(&resp);
    println!("{}", serde_json::to_string_pretty(items)?);
    Ok(0)
}

/// `PATCH /{collection}/{id}/status` — activate or archive.
pub async fn change_status(
    client: &OrionClient,
    kind: &VersionedEntityKind,
    format: &OutputFormat,
    quiet: bool,
    req: StatusChange<'_>,
) -> Result<i32> {
    let StatusChange {
        id,
        status,
        dry_run,
        defer_reload,
    } = req;
    let qs = build_query_string(&[
        ("dry_run", dry_run.then(|| "true".to_string())),
        ("reload", defer_reload.then(|| "defer".to_string())),
    ]);
    let body = serde_json::json!({ "status": status });
    let resp: Value = client
        .patch(&format!("{}{qs}", (kind.status)(id)), &body)
        .await?;
    let title = kind.entity.title;

    // A dry run answers with the `/validate` envelope, not the entity — and a
    // transition that would be refused is reported as `valid: false` inside a
    // 200. Render the findings and exit non-zero, so a pre-flight that fails
    // cannot read as one that passed. It earns its keep because what the
    // server checks is not reproducible client-side: activating a channel
    // needs an active workflow, a route that collides with nothing, and a
    // config that still builds.
    if dry_run {
        return print_validation_envelope(
            &resp,
            format,
            quiet,
            "DRY RUN",
            &format!("{title} {id} can change to {status} (nothing written)"),
            &format!("{title} {id} cannot change to {status}"),
        );
    }

    if !quiet {
        let entity = &resp["data"];
        println!(
            "{} {title} {} status changed to {}",
            "OK".green().bold(),
            entity["name"].as_str().unwrap_or(id),
            colorize_status(status)
        );
        if defer_reload {
            println!("  Reload deferred -- run 'orion-cli engine reload' to apply.");
        }
    }
    Ok(0)
}

/// One row of `{entity} versions`.
///
/// `priority` is real on both versioned entities: it is a required field on
/// the channel and workflow responses alike, and it is how competing matches
/// are ordered. The channels table lost the column in a copy-edit when that
/// module was split out of the old `rules.rs`, which had it.
#[derive(tabled::Tabled)]
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

/// `GET /{collection}/{id}/versions{qs}` — the version history.
pub async fn list_versions(
    client: &OrionClient,
    kind: &VersionedEntityKind,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
    qs: &str,
) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", (kind.versions)(id))).await?;
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
    print_list_footer(&resp, vers.len(), "version(s)");
    Ok(0)
}

/// `POST /{collection}/{id}/versions` — cut a new draft from the latest.
pub async fn create_version(
    client: &OrionClient,
    kind: &VersionedEntityKind,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.post_empty(&(kind.versions)(id)).await?;
    let entity = &resp["data"];

    if quiet {
        println!("{}", entity["version"].as_i64().unwrap_or(0));
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    println!(
        "{} New draft version {} created for {} {}",
        "OK".green().bold(),
        entity["version"].as_i64().unwrap_or(0),
        kind.entity.label,
        entity["name"].as_str().unwrap_or(id)
    );
    Ok(0)
}

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

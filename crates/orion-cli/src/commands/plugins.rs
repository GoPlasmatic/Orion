use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use clap::{Args, Subcommand};
use colored::Colorize;
use orion_api::{STATUS_ACTIVE, STATUS_ARCHIVED};
use serde_json::{Value, json};
use tabled::Tabled;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use crate::utils::{self, colorize_status, truncate};
use orion_client::paths;

/// What a plugin is, in endpoint terms.
static KIND: utils::EntityKind = utils::EntityKind {
    title: "Plugin",
    label: "plugin",
    collection: paths::PLUGINS,
    export: paths::PLUGINS_EXPORT,
    validate: paths::PLUGINS_VALIDATE,
    item: paths::plugin,
    id_field: "plugin_id",
};

static VERSIONED: utils::VersionedEntityKind = utils::VersionedEntityKind {
    entity: &KIND,
    status: paths::plugin_status,
    versions: paths::plugin_versions,
};

#[derive(Args)]
#[command(
    long_about = "Manage plugins -- sandboxed WebAssembly components that add task functions.\n\n\
        Lifecycle: draft -> activate -> live\n\
        A plugin is created from its manifest (plugin.toml) and the component the manifest names. \
        Activating one reloads the engine and its functions join `functions list`; archiving is \
        refused while an active workflow calls one of them.\n\n\
        With --quiet, list prints one ID per line, get prints the ID, and mutating commands print the resource ID or suppress output."
)]
pub struct PluginsCmd {
    #[command(subcommand)]
    command: PluginsSubcommand,
}

#[derive(Subcommand)]
enum PluginsSubcommand {
    /// List all plugins
    #[command(
        after_help = "The server pages at 50 by default; raise it with --limit (max 1000)\n\
            or walk pages with --offset.\n\n\
            Examples:\n  \
            orion-cli plugins list\n  \
            orion-cli plugins list --status active\n  \
            orion-cli plugins list --tag codecs --limit 200"
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
        /// Sort by column (plugin_id, status, created_at, updated_at)
        #[arg(long)]
        sort_by: Option<String>,
        /// Sort direction (asc, desc)
        #[arg(long)]
        sort_order: Option<String>,
    },
    /// Get a plugin by ID, with this node's load state (use --verbose for the manifest)
    Get {
        /// Plugin ID
        id: String,
    },
    /// Upload a plugin from its manifest and component
    #[command(after_help = crate::help::PLUGIN_CREATE)]
    Create {
        /// Path to the plugin manifest (TOML)
        #[arg(short, long)]
        file: String,
        /// Path to the component; overrides the manifest's `component`
        #[arg(long)]
        component: Option<String>,
        /// Path to a file holding the base64 Ed25519 signature over the
        /// component digest — required by a server with `[plugins.trust]` keys
        #[arg(long, value_name = "PATH")]
        signature: Option<String>,
        /// Selection labels, repeatable
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Replace a draft plugin's manifest and component
    Update {
        /// Plugin ID
        id: String,
        /// Path to the plugin manifest (TOML)
        #[arg(short, long)]
        file: String,
        /// Path to the component; overrides the manifest's `component`
        #[arg(long)]
        component: Option<String>,
        /// Path to a file holding the base64 Ed25519 signature over the new
        /// component's digest; the stored one is kept when the digest is unchanged
        #[arg(long, value_name = "PATH")]
        signature: Option<String>,
        /// Selection labels, repeatable (replaces the stored tags when given)
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Delete a plugin (prompts for confirmation)
    Delete {
        /// Plugin ID
        id: String,
    },
    /// Activate a draft plugin (the engine reloads automatically)
    Activate {
        /// Plugin ID
        id: String,
        /// Pre-flight only: report whether activation would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Archive an active plugin (refused while an active workflow calls its functions)
    Archive {
        /// Plugin ID
        id: String,
        /// Pre-flight only: report whether archiving would succeed, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Defer the engine reload (batch several changes, then 'engine reload' once)
        #[arg(long)]
        defer_reload: bool,
    },
    /// Show what depends on a plugin: its functions and the active workflows calling them
    #[command(alias = "deps")]
    Dependencies {
        /// Plugin ID
        id: String,
    },
    /// Validate a manifest and component without uploading them
    Validate {
        /// Path to the plugin manifest (TOML)
        #[arg(short, long)]
        file: String,
        /// Path to the component; overrides the manifest's `component`
        #[arg(long)]
        component: Option<String>,
        /// Path to a file holding the base64 Ed25519 signature over the
        /// component digest, when the server requires one
        #[arg(long, value_name = "PATH")]
        signature: Option<String>,
    },
    /// List version history for a plugin
    Versions {
        /// Plugin ID
        id: String,
        /// Page size (default: 50, max: 1000)
        #[arg(long)]
        limit: Option<i64>,
        /// Page offset
        #[arg(long)]
        offset: Option<i64>,
    },
    /// Create a new draft version of a plugin
    NewVersion {
        /// Plugin ID
        id: String,
    },
    /// Export plugins as JSON (pipe to file for backup or promotion)
    #[command(
        after_help = "Examples:\n  orion-cli plugins export --include-artifacts > plugins.json\n  orion-cli plugins export --status active > active-plugins.json"
    )]
    Export {
        /// Filter by status (draft, active, archived)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Inline each component as base64, so the file imports anywhere
        #[arg(long)]
        include_artifacts: bool,
    },
    /// Import plugins from a JSON array file
    #[command(
        after_help = "Examples:\n  orion-cli plugins import -f plugins.json --dry-run\n  orion-cli plugins import -f plugins.json --on-conflict new_version"
    )]
    Import {
        /// Path to JSON file containing a plugins array
        #[arg(short, long)]
        file: String,
        /// Preview what would be imported without making changes
        #[arg(long)]
        dry_run: bool,
        /// On ID conflict: fail (default), skip the item, or write a new version
        #[arg(long, value_parser = ["fail", "skip", "new_version"])]
        on_conflict: Option<String>,
    },
}

#[derive(Tabled)]
struct PluginRow {
    #[tabled(rename = "ID")]
    plugin_id: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Functions")]
    functions: String,
    #[tabled(rename = "Digest")]
    digest: String,
}

impl PluginsCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
        verbose: bool,
        yes: bool,
    ) -> Result<i32> {
        match &self.command {
            PluginsSubcommand::List {
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
            PluginsSubcommand::Get { id } => get_plugin(client, format, quiet, verbose, id).await,
            PluginsSubcommand::Create {
                file,
                component,
                signature,
                tags,
            } => {
                let body = upload_body(file, component.as_deref(), signature.as_deref(), tags)?;
                utils::create_entity(client, &KIND, format, quiet, &body).await
            }
            PluginsSubcommand::Update {
                id,
                file,
                component,
                signature,
                tags,
            } => {
                let mut body = upload_body(file, component.as_deref(), signature.as_deref(), tags)?;
                // `PUT` keeps an absent field; the tags are replaced only when
                // the caller named some.
                if tags.is_empty() {
                    body.as_object_mut().map(|o| o.remove("tags"));
                }
                utils::update_entity(client, &KIND, format, quiet, id, &body).await
            }
            PluginsSubcommand::Delete { id } => {
                utils::delete_entity(client, &KIND, quiet, yes, id).await
            }
            PluginsSubcommand::Activate {
                id,
                dry_run,
                defer_reload,
            } => {
                utils::change_status(
                    client,
                    &VERSIONED,
                    format,
                    quiet,
                    utils::StatusChange {
                        id,
                        status: STATUS_ACTIVE,
                        dry_run: *dry_run,
                        defer_reload: *defer_reload,
                    },
                )
                .await
            }
            PluginsSubcommand::Archive {
                id,
                dry_run,
                defer_reload,
            } => {
                utils::change_status(
                    client,
                    &VERSIONED,
                    format,
                    quiet,
                    utils::StatusChange {
                        id,
                        status: STATUS_ARCHIVED,
                        dry_run: *dry_run,
                        defer_reload: *defer_reload,
                    },
                )
                .await
            }
            PluginsSubcommand::Dependencies { id } => dependencies(client, format, quiet, id).await,
            PluginsSubcommand::Validate {
                file,
                component,
                signature,
            } => {
                let body = upload_body(file, component.as_deref(), signature.as_deref(), &[])?;
                utils::validate_entity(client, &KIND, format, quiet, &body).await
            }
            PluginsSubcommand::Versions { id, limit, offset } => {
                let qs = utils::build_query_string(&[
                    ("limit", limit.map(|l| l.to_string())),
                    ("offset", offset.map(|o| o.to_string())),
                ]);
                utils::list_versions(client, &VERSIONED, format, quiet, id, &qs).await
            }
            PluginsSubcommand::NewVersion { id } => {
                utils::create_version(client, &VERSIONED, format, quiet, id).await
            }
            PluginsSubcommand::Export {
                status,
                tag,
                include_artifacts,
            } => {
                let qs = utils::build_query_string(&[
                    ("status", status.clone()),
                    ("tag", tag.clone()),
                    (
                        "include_artifacts",
                        include_artifacts.then(|| "true".to_string()),
                    ),
                ]);
                utils::export_entities(client, &KIND, &qs).await
            }
            PluginsSubcommand::Import {
                file,
                dry_run,
                on_conflict,
            } => {
                utils::run_import(
                    client,
                    format,
                    quiet,
                    utils::ImportRequest {
                        base_path: paths::PLUGINS_IMPORT,
                        label: "plugin",
                        file,
                        dry_run: *dry_run,
                        on_conflict: on_conflict.as_deref(),
                    },
                )
                .await
            }
        }
    }
}

/// The request body for an upload: the manifest text as read, the component
/// the manifest (or `--component`) names, base64-encoded, and the tags.
///
/// The component path is resolved relative to the manifest — the same rule
/// the server's offline tooling applies — and refused if it would leave the
/// manifest's directory, so a manifest cannot make the CLI read an arbitrary
/// file.
fn upload_body(
    manifest_path: &str,
    component: Option<&str>,
    signature: Option<&str>,
    tags: &[String],
) -> Result<Value> {
    let manifest_text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest '{manifest_path}'"))?;
    // The signature file holds the base64 text a signing tool wrote — read
    // as text and trimmed, so a trailing newline is not part of the value.
    let signature = signature
        .map(|path| {
            std::fs::read_to_string(path)
                .with_context(|| format!("reading signature '{path}'"))
                .map(|s| s.trim().to_string())
        })
        .transpose()?;
    let manifest: toml::Value = toml::from_str(&manifest_text)
        .with_context(|| format!("'{manifest_path}' is not valid TOML"))?;
    let manifest_dir = Path::new(manifest_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let component_path = match component {
        Some(explicit) => Path::new(explicit).to_path_buf(),
        None => {
            let named = manifest
                .get("component")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "'{manifest_path}' names no `component`; pass --component <path>"
                    )
                })?;
            let rel = Path::new(named);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!(
                    "'{manifest_path}': component path '{named}' must stay beneath the manifest's \
                     directory"
                );
            }
            manifest_dir.join(rel)
        }
    };
    let bytes = std::fs::read(&component_path)
        .with_context(|| format!("reading component '{}'", component_path.display()))?;
    let mut body = json!({
        "manifest": manifest_text,
        "component": base64::engine::general_purpose::STANDARD.encode(bytes),
        "tags": tags,
    });
    if let Some(signature) = signature {
        body["signature"] = json!(signature);
    }
    Ok(body)
}

async fn list(client: &OrionClient, format: &OutputFormat, quiet: bool, qs: &str) -> Result<i32> {
    let resp: Value = client.get(&format!("{}{qs}", paths::PLUGINS)).await?;
    let items = resp["data"].as_array().cloned().unwrap_or_default();

    if quiet {
        for p in &items {
            if let Some(id) = p["plugin_id"].as_str() {
                println!("{id}");
            }
        }
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    if items.is_empty() {
        println!("{}", "No plugins found.".dimmed());
        return Ok(0);
    }
    let rows: Vec<PluginRow> = items.iter().map(plugin_row).collect();
    output::print_table(rows);
    utils::print_list_footer(&resp, items.len(), "plugin");
    Ok(0)
}

fn plugin_row(p: &Value) -> PluginRow {
    let functions = p["functions"]
        .as_array()
        .map(|fs| {
            fs.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    PluginRow {
        plugin_id: p["plugin_id"].as_str().unwrap_or("").to_string(),
        version: p["version"].as_i64().unwrap_or(0),
        status: colorize_status(p["status"].as_str().unwrap_or("")),
        functions: truncate(&functions, 48),
        digest: truncate(p["digest"].as_str().unwrap_or(""), 19),
    }
}

async fn get_plugin(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    verbose: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::plugin(id)).await?;
    let p = &resp["data"];
    if quiet {
        println!("{}", p["plugin_id"].as_str().unwrap_or(id));
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    println!("{}: {}", "ID".bold(), p["plugin_id"].as_str().unwrap_or(""));
    println!(
        "{}: {} (manifest {})",
        "Version".bold(),
        p["version"],
        p["plugin_version"].as_str().unwrap_or("")
    );
    println!(
        "{}: {}",
        "Status".bold(),
        colorize_status(p["status"].as_str().unwrap_or(""))
    );
    println!("{}: {}", "ABI".bold(), p["abi"].as_str().unwrap_or(""));
    println!(
        "{}: {}",
        "Digest".bold(),
        p["digest"].as_str().unwrap_or("")
    );
    if let Some(functions) = p["functions"].as_array() {
        println!("{}:", "Functions".bold());
        for f in functions {
            println!("  {}", f.as_str().unwrap_or(""));
        }
    }
    if let Some(health) = p.get("health").filter(|h| !h.is_null()) {
        let state = health["state"].as_str().unwrap_or("");
        let detail = match state {
            "loaded" => health["compile_ms"]
                .as_u64()
                .map(|ms| format!(" (compiled in {ms} ms)"))
                .unwrap_or_default(),
            "failed" => health["reason"]
                .as_str()
                .map(|r| format!(": {r}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let coloured = match state {
            "loaded" => state.green().to_string(),
            "failed" => state.red().to_string(),
            other => other.yellow().to_string(),
        };
        println!("{}: {coloured}{detail}", "Health".bold());
    }
    if let Some(tags) = p["tags"].as_array().filter(|t| !t.is_empty()) {
        let tags: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
        println!("{}: {}", "Tags".bold(), tags.join(", "));
    }
    if verbose {
        println!("{}:", "Manifest".bold());
        println!("{}", serde_json::to_string_pretty(&p["manifest"])?);
    }
    Ok(0)
}

async fn dependencies(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    id: &str,
) -> Result<i32> {
    let resp: Value = client.get(&paths::plugin_dependencies(id)).await?;
    let d = &resp["data"];
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    let workflows: Vec<&str> = d["workflows"]
        .as_array()
        .map(|w| w.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if quiet {
        for w in &workflows {
            println!("{w}");
        }
        return Ok(0);
    }
    println!(
        "{}: {} v{}",
        "Plugin".bold(),
        d["plugin_id"].as_str().unwrap_or(id),
        d["version"]
    );
    if let Some(functions) = d["functions"].as_array() {
        println!("{}:", "Functions".bold());
        for f in functions {
            println!("  {}", f.as_str().unwrap_or(""));
        }
    }
    if workflows.is_empty() {
        println!(
            "{}: {}",
            "Active workflows calling them".bold(),
            "none".dimmed()
        );
    } else {
        println!("{}:", "Active workflows calling them".bold());
        for w in workflows {
            println!("  {w}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `paths::` item fn has the same type, so a static wired to the
    /// wrong entity's family compiles. Asserted against literal URLs.
    #[test]
    fn the_plugin_kind_points_at_the_plugin_endpoints() {
        assert_eq!(KIND.collection, "/api/v1/admin/plugins");
        assert_eq!(KIND.export, "/api/v1/admin/plugins/export");
        assert_eq!(KIND.validate, "/api/v1/admin/plugins/validate");
        assert_eq!((KIND.item)("acme.x"), "/api/v1/admin/plugins/acme.x");
        assert_eq!(KIND.id_field, "plugin_id");
        assert_eq!(
            (VERSIONED.status)("acme.x"),
            "/api/v1/admin/plugins/acme.x/status"
        );
        assert_eq!(
            (VERSIONED.versions)("acme.x"),
            "/api/v1/admin/plugins/acme.x/versions"
        );
    }

    /// The component named by a manifest is read beside it, and never from
    /// outside its directory.
    #[test]
    fn the_component_is_resolved_beside_the_manifest_and_kept_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("plugin.toml");
        std::fs::write(
            &manifest,
            "abi = \"orion:plugin@1.0.0\"\nname = \"acme.x\"\nversion = \"1\"\ncomponent = \"build/x.wasm\"\n",
        )
        .expect("write");
        std::fs::create_dir(dir.path().join("build")).expect("mkdir");
        std::fs::write(dir.path().join("build/x.wasm"), b"\0asm").expect("write");

        let body = upload_body(
            manifest.to_str().expect("utf8"),
            None,
            None,
            &["a".to_string()],
        )
        .expect("body");
        assert_eq!(
            body["component"],
            base64::engine::general_purpose::STANDARD.encode(b"\0asm")
        );
        assert!(body["manifest"].as_str().expect("text").contains("acme.x"));
        assert_eq!(body["tags"], json!(["a"]));

        std::fs::write(
            &manifest,
            "abi = \"orion:plugin@1.0.0\"\nname = \"acme.x\"\nversion = \"1\"\ncomponent = \"../x.wasm\"\n",
        )
        .expect("write");
        let err =
            upload_body(manifest.to_str().expect("utf8"), None, None, &[]).expect_err("escape");
        assert!(err.to_string().contains("beneath the manifest"), "{err}");
    }
}

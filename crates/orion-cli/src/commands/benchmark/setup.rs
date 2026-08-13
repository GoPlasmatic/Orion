use anyhow::{Context, Result, bail};
use colored::Colorize;
use orion_api::{ImportResult, STATUS_ACTIVE};
use serde_json::Value;

use crate::client::OrionClient;
use orion_client::paths;

use super::fixtures::{ScenarioConfig, parse_payload};

pub struct BenchmarkResources {
    pub scenario_name: String,
    pub workflow_ids: Vec<String>,
    /// Channels this scenario created, deleted again on cleanup. The load
    /// target is `channel_name`; the rest populate the estate.
    pub channel_names: Vec<String>,
    pub channel_name: String,
    pub payload: Value,
}

pub async fn create_resources(
    client: &OrionClient,
    scenarios: &[&ScenarioConfig],
    quiet: bool,
) -> Result<Vec<BenchmarkResources>> {
    let mut all_resources = Vec::new();

    for config in scenarios {
        if !quiet {
            eprint!("  Setting up {}... ", config.description);
        }

        // The v1.0 data plane routes a channel to exactly one workflow, so
        // every workflow that should serve traffic needs its own channel —
        // the same shape as the server's bench.sh scenarios.
        let (workflow_ids, channel_names) = if config.is_import {
            let ids = import_workflows(client, config.workflow_json).await?;
            let mut channels = Vec::new();
            for (n, id) in ids.iter().enumerate() {
                // `config.channel` first, so the URL under load is the same
                // one the single-workflow scenarios use and they differ only
                // in how much else is registered.
                let name = if n == 0 {
                    config.channel.to_string()
                } else {
                    format!("{}-{n}", config.channel)
                };
                create_and_activate_channel(client, &name, id).await?;
                channels.push(name);
            }
            (ids, channels)
        } else {
            let id = create_and_activate_workflow(client, config.workflow_json).await?;
            create_and_activate_channel(client, config.channel, &id).await?;
            (vec![id], vec![config.channel.to_string()])
        };

        if !quiet {
            eprintln!("{}", "done".green());
        }

        all_resources.push(BenchmarkResources {
            scenario_name: config.name.to_string(),
            workflow_ids,
            channel_names,
            channel_name: config.channel.to_string(),
            payload: parse_payload(config.payload_json),
        });
    }

    // Single engine reload after all workflows are set up
    if !quiet {
        eprint!("  Reloading engine... ");
    }
    let _: Value = client
        .post_empty(paths::ENGINE_RELOAD)
        .await
        .context("Failed to reload engine")?;

    // Wait for engine to be ready
    wait_for_ready(client).await?;

    if !quiet {
        eprintln!("{}", "ready".green());
    }

    Ok(all_resources)
}

async fn create_and_activate_workflow(client: &OrionClient, workflow_json: &str) -> Result<String> {
    let body: Value =
        serde_json::from_str(workflow_json).context("Failed to parse workflow fixture")?;

    let resp: Value = client
        .post(paths::WORKFLOWS, &body)
        .await
        .context("Failed to create workflow")?;

    let workflow_id = resp["data"]["workflow_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No workflow_id in response"))?
        .to_string();

    let status_body = serde_json::json!({"status": STATUS_ACTIVE});
    let _: Value = client
        .patch(&paths::workflow_status(&workflow_id), &status_body)
        .await
        .context("Failed to activate workflow")?;

    Ok(workflow_id)
}

/// `protocol: "http"` requires both `methods` and `route_pattern`; a channel
/// missing either is refused at create. Activation requires the workflow to
/// already be active.
async fn create_and_activate_channel(
    client: &OrionClient,
    name: &str,
    workflow_id: &str,
) -> Result<()> {
    let body = serde_json::json!({
        "channel_id": name,
        "name": name,
        "channel_type": "sync",
        "protocol": "http",
        "methods": ["POST"],
        "route_pattern": format!("/{name}"),
        "workflow_id": workflow_id,
    });
    let _: Value = client
        .post(paths::CHANNELS, &body)
        .await
        .with_context(|| format!("Failed to create channel {name}"))?;

    let status_body = serde_json::json!({"status": STATUS_ACTIVE});
    let _: Value = client
        .patch(&paths::channel_status(name), &status_body)
        .await
        .with_context(|| format!("Failed to activate channel {name}"))?;

    Ok(())
}

async fn import_workflows(client: &OrionClient, workflows_json: &str) -> Result<Vec<String>> {
    let body: Value =
        serde_json::from_str(workflows_json).context("Failed to parse multi-workflow fixture")?;

    let resp: Value = client
        .post(paths::WORKFLOWS_IMPORT, &body)
        .await
        .context("Failed to import workflows")?;

    // Activate exactly the workflows this import wrote — the report names
    // them. Listing drafts instead would pick up (activate, and later
    // delete) drafts that belong to someone else on a shared server.
    let report: ImportResult =
        serde_json::from_value(resp.get("data").cloned().unwrap_or(resp)).unwrap_or_default();
    if report.failed > 0 {
        bail!(
            "{} benchmark workflow(s) failed to import: {}",
            report.failed,
            report
                .errors
                .first()
                .map(|e| e.error.as_str())
                .unwrap_or("see server logs")
        );
    }

    let mut ids = Vec::new();
    let status_body = serde_json::json!({"status": STATUS_ACTIVE});

    for item in &report.results {
        // Only written items have a draft to activate; `unchanged`/`skipped`
        // entities already exist outside this run and are not ours to touch.
        if !matches!(
            item.action.as_str(),
            "created" | "updated_draft" | "new_version"
        ) {
            continue;
        }
        let Some(id) = item.id.as_deref() else {
            continue;
        };
        let _: Value = client
            .patch(&paths::workflow_status(id), &status_body)
            .await
            .with_context(|| format!("Failed to activate workflow {id}"))?;
        ids.push(id.to_string());
    }

    if ids.is_empty() {
        bail!(
            "Import wrote no workflows — a previous benchmark run may still be deployed. \
             Run with --cleanup-only first."
        );
    }

    Ok(ids)
}

async fn wait_for_ready(client: &OrionClient) -> Result<()> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(15);

    loop {
        match client.get::<Value>(paths::HEALTH).await {
            Ok(resp) => {
                if resp["status"].as_str() == Some("ok") {
                    return Ok(());
                }
            }
            Err(_) if start.elapsed() < timeout => {}
            Err(e) => return Err(e).context("Server health check failed"),
        }

        if start.elapsed() >= timeout {
            bail!("Server did not become ready within 15s after reload");
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

pub async fn verify_workflow(client: &OrionClient, workflow_id: &str) -> Result<()> {
    let resp: Value = client
        .get(&paths::workflow(workflow_id))
        .await
        .with_context(|| format!("Workflow '{workflow_id}' not found"))?;

    let status = resp["data"]["status"].as_str().unwrap_or("unknown");
    if status != STATUS_ACTIVE {
        bail!("Workflow '{workflow_id}' is not active (status: {status}). Activate it first.");
    }

    Ok(())
}

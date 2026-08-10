use anyhow::{Context, Result, bail};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;

use super::fixtures::{ScenarioConfig, parse_payload};

pub struct BenchmarkResources {
    pub scenario_name: String,
    pub workflow_ids: Vec<String>,
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

        let workflow_ids = if config.is_import {
            import_workflows(client, config.workflow_json).await?
        } else {
            let id = create_and_activate_workflow(client, config.workflow_json).await?;
            vec![id]
        };

        if !quiet {
            eprintln!("{}", "done".green());
        }

        all_resources.push(BenchmarkResources {
            scenario_name: config.name.to_string(),
            workflow_ids,
            channel_name: config.channel.to_string(),
            payload: parse_payload(config.payload_json),
        });
    }

    // Single engine reload after all workflows are set up
    if !quiet {
        eprint!("  Reloading engine... ");
    }
    let _: Value = client
        .post_empty("/api/v1/admin/engine/reload")
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
        .post("/api/v1/admin/workflows", &body)
        .await
        .context("Failed to create workflow")?;

    let workflow_id = resp["data"]["workflow_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No workflow_id in response"))?
        .to_string();

    let status_body = serde_json::json!({"status": "active"});
    let _: Value = client
        .patch(
            &format!("/api/v1/admin/workflows/{workflow_id}/status"),
            &status_body,
        )
        .await
        .context("Failed to activate workflow")?;

    Ok(workflow_id)
}

async fn import_workflows(client: &OrionClient, workflows_json: &str) -> Result<Vec<String>> {
    let body: Value =
        serde_json::from_str(workflows_json).context("Failed to parse multi-workflow fixture")?;

    let _: Value = client
        .post("/api/v1/admin/workflows/import", &body)
        .await
        .context("Failed to import workflows")?;

    // List draft workflows and activate each
    let list: Value = client
        .get("/api/v1/admin/workflows?status=draft")
        .await
        .context("Failed to list draft workflows")?;

    let drafts = list["data"].as_array().cloned().unwrap_or_default();

    let mut ids = Vec::new();
    let status_body = serde_json::json!({"status": "active"});

    for wf in &drafts {
        if let Some(id) = wf["workflow_id"].as_str() {
            let _: Value = client
                .patch(
                    &format!("/api/v1/admin/workflows/{id}/status"),
                    &status_body,
                )
                .await
                .with_context(|| format!("Failed to activate workflow {id}"))?;
            ids.push(id.to_string());
        }
    }

    Ok(ids)
}

async fn wait_for_ready(client: &OrionClient) -> Result<()> {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(15);

    loop {
        match client.get::<Value>("/health").await {
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
        .get(&format!("/api/v1/admin/workflows/{workflow_id}"))
        .await
        .with_context(|| format!("Workflow '{workflow_id}' not found"))?;

    let status = resp["data"]["status"].as_str().unwrap_or("unknown");
    if status != "active" {
        bail!("Workflow '{workflow_id}' is not active (status: {status}). Activate it first.");
    }

    Ok(())
}

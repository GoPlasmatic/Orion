use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;

use super::setup::BenchmarkResources;

/// Delete known benchmark resources by their IDs.
pub async fn remove_resources(
    client: &OrionClient,
    resources: &[BenchmarkResources],
    quiet: bool,
) -> Result<()> {
    if !quiet {
        eprint!("  Cleaning up benchmark resources... ");
    }

    for res in resources {
        for id in &res.workflow_ids {
            if let Err(e) = client
                .delete_request(&format!("/api/v1/admin/workflows/{id}"))
                .await
            {
                if !quiet {
                    eprintln!("\n  Warning: failed to delete workflow {id}: {e}");
                }
            }
        }
    }

    // Reload engine after cleanup
    let _ = client
        .post_empty::<Value>("/api/v1/admin/engine/reload")
        .await;

    if !quiet {
        eprintln!("{}", "done".green());
    }

    Ok(())
}

/// Delete all benchmark resources by scanning for known benchmark workflow names.
/// Used by --cleanup-only when workflow IDs are unknown (e.g., after a crash).
pub async fn cleanup_all_bench_resources(client: &OrionClient, quiet: bool) -> Result<()> {
    if !quiet {
        eprintln!(
            "{}",
            "Scanning for leftover benchmark resources...".dimmed()
        );
    }

    let resp: Value = client
        .get("/api/v1/admin/workflows")
        .await
        .context("Failed to list workflows")?;

    let workflows = resp["data"].as_array().cloned().unwrap_or_default();
    let mut deleted = 0;

    for wf in &workflows {
        let name = wf["name"].as_str().unwrap_or("");
        let is_bench = name.starts_with("Bench ") || name.starts_with("Multi Rule ");

        if is_bench {
            if let Some(id) = wf["workflow_id"].as_str() {
                if let Err(e) = client
                    .delete_request(&format!("/api/v1/admin/workflows/{id}"))
                    .await
                {
                    if !quiet {
                        eprintln!("  Warning: failed to delete {name} ({id}): {e}");
                    }
                } else {
                    deleted += 1;
                    if !quiet {
                        eprintln!("  Deleted: {name} ({id})");
                    }
                }
            }
        }
    }

    // Reload engine
    let _ = client
        .post_empty::<Value>("/api/v1/admin/engine/reload")
        .await;

    if !quiet {
        if deleted > 0 {
            eprintln!(
                "{} Cleaned up {deleted} benchmark workflow(s)",
                "OK".green().bold()
            );
        } else {
            eprintln!("No benchmark resources found");
        }
    }

    Ok(())
}

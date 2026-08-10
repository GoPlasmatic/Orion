use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use orion_client::paths;

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
        // Channels first — they reference the workflows.
        for name in &res.channel_names {
            if let Err(e) = client.delete_request(&paths::channel(name)).await
                && !quiet
            {
                eprintln!("\n  Warning: failed to delete channel {name}: {e}");
            }
        }
        for id in &res.workflow_ids {
            if let Err(e) = client.delete_request(&paths::workflow(id)).await
                && !quiet
            {
                eprintln!("\n  Warning: failed to delete workflow {id}: {e}");
            }
        }
    }

    // Reload engine after cleanup
    let _ = client.post_empty::<Value>(paths::ENGINE_RELOAD).await;

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

    // Benchmark channels first — they reference the workflows. Channel ids
    // are the fixed names setup creates: `bench`, `orders`, `bench-N`.
    let mut deleted = 0;
    if let Ok(resp) = client.get::<Value>(paths::CHANNELS).await {
        for ch in resp["data"].as_array().cloned().unwrap_or_default() {
            let id = ch["channel_id"].as_str().unwrap_or("");
            let is_bench = id == "bench" || id == "orders" || id.starts_with("bench-");
            if is_bench {
                if let Err(e) = client.delete_request(&paths::channel(id)).await {
                    if !quiet {
                        eprintln!("  Warning: failed to delete channel {id}: {e}");
                    }
                } else {
                    deleted += 1;
                    if !quiet {
                        eprintln!("  Deleted channel: {id}");
                    }
                }
            }
        }
    }

    let resp: Value = client
        .get(paths::WORKFLOWS)
        .await
        .context("Failed to list workflows")?;

    let workflows = resp["data"].as_array().cloned().unwrap_or_default();

    for wf in &workflows {
        let name = wf["name"].as_str().unwrap_or("");
        let is_bench = name.starts_with("Bench ") || name.starts_with("Multi Rule ");

        if is_bench && let Some(id) = wf["workflow_id"].as_str() {
            if let Err(e) = client.delete_request(&paths::workflow(id)).await {
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

    // Reload engine
    let _ = client.post_empty::<Value>(paths::ENGINE_RELOAD).await;

    if !quiet {
        if deleted > 0 {
            eprintln!(
                "{} Cleaned up {deleted} benchmark resource(s)",
                "OK".green().bold()
            );
        } else {
            eprintln!("No benchmark resources found");
        }
    }

    Ok(())
}

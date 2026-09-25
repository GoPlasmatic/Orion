use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use serde_json::Value;

use crate::client::OrionClient;
use crate::output::{self, OutputFormat};
use orion_client::paths;

#[derive(Args)]
pub struct CacheCmd {
    #[command(subcommand)]
    command: CacheSubcommand,
}

#[derive(Subcommand)]
enum CacheSubcommand {
    /// Invalidate a channel response-cache namespace
    #[command(long_about = "Invalidate a channel response-cache namespace.\n\n\
            Every channel whose cache.namespaces lists it misses on its next request, on \
            every node sharing the store. For a change no workflow made; a workflow uses \
            the cache_invalidate function instead.")]
    Invalidate {
        /// The namespace, as channels declare it in cache.namespaces
        namespace: String,
    },
}

impl CacheCmd {
    pub async fn run(
        &self,
        client: &OrionClient,
        format: &OutputFormat,
        quiet: bool,
    ) -> Result<i32> {
        match &self.command {
            CacheSubcommand::Invalidate { namespace } => {
                invalidate(client, format, quiet, namespace).await
            }
        }
    }
}

async fn invalidate(
    client: &OrionClient,
    format: &OutputFormat,
    quiet: bool,
    namespace: &str,
) -> Result<i32> {
    let resp: Value = client
        .post_empty(&paths::cache_namespace_invalidate(namespace))
        .await?;
    let resp = resp.get("data").cloned().unwrap_or(resp);
    if quiet {
        return Ok(0);
    }
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        output::print_value(format, &resp)?;
        return Ok(0);
    }
    let stores = resp["stores"].as_u64().unwrap_or(0);
    println!(
        "{} Invalidated namespace '{namespace}' in {stores} store(s)",
        "OK".green().bold()
    );
    Ok(0)
}

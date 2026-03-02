pub mod tools;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::client::OrionClient;

#[derive(Debug, Clone)]
pub struct OrionService {
    client: OrionClient,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl OrionService {
    pub fn new(client: OrionClient) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }

    // ── Health ──────────────────────────────────────────────────────────

    #[tool(
        description = "Check the health status of the Orion server, including component status and uptime"
    )]
    async fn health_check(&self) -> Result<String, String> {
        tools::health::check(&self.client).await
    }

    // ── Metrics ────────────────────────────────────────────────────────

    #[tool(description = "Retrieve Prometheus metrics from the Orion server in raw text format")]
    async fn get_metrics(&self) -> Result<String, String> {
        tools::metrics::get(&self.client).await
    }

    // ── Engine ─────────────────────────────────────────────────────────

    #[tool(
        description = "Get the current engine status including version, uptime, active rules count, and available channels"
    )]
    async fn engine_status(&self) -> Result<String, String> {
        tools::engine::status(&self.client).await
    }

    #[tool(
        description = "Reload the engine to pick up rule changes. This hot-reloads rules without server restart."
    )]
    async fn engine_reload(&self) -> Result<String, String> {
        tools::engine::reload(&self.client).await
    }

    // ── Rules ──────────────────────────────────────────────────────────

    #[tool(
        description = "List all rules in the Orion engine. Optionally filter by status (active/paused/archived), channel, or tag. Supports pagination with limit and offset."
    )]
    async fn rules_list(
        &self,
        Parameters(params): Parameters<tools::rules::RulesListParams>,
    ) -> Result<String, String> {
        tools::rules::list(&self.client, params).await
    }

    #[tool(
        description = "Get a rule by its ID, including full details like condition, tasks, tags, and version history count"
    )]
    async fn rules_get(
        &self,
        Parameters(params): Parameters<tools::rules::RulesGetParams>,
    ) -> Result<String, String> {
        tools::rules::get(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/rules_create.md")]
    #[tool]
    async fn rules_create(
        &self,
        Parameters(params): Parameters<tools::rules::RulesCreateParams>,
    ) -> Result<String, String> {
        tools::rules::create(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/rules_update.md")]
    #[tool]
    async fn rules_update(
        &self,
        Parameters(params): Parameters<tools::rules::RulesUpdateParams>,
    ) -> Result<String, String> {
        tools::rules::update(&self.client, params).await
    }

    #[tool(description = "Delete a rule by its ID. This is irreversible.")]
    async fn rules_delete(
        &self,
        Parameters(params): Parameters<tools::rules::RulesDeleteParams>,
    ) -> Result<String, String> {
        tools::rules::delete(&self.client, params).await
    }

    #[tool(
        description = "Activate a rule by setting its status to 'active'. Active rules are evaluated during data processing."
    )]
    async fn rules_activate(
        &self,
        Parameters(params): Parameters<tools::rules::RulesStatusParams>,
    ) -> Result<String, String> {
        tools::rules::activate(&self.client, params).await
    }

    #[tool(
        description = "Pause a rule by setting its status to 'paused'. Paused rules are skipped during data processing."
    )]
    async fn rules_pause(
        &self,
        Parameters(params): Parameters<tools::rules::RulesStatusParams>,
    ) -> Result<String, String> {
        tools::rules::pause(&self.client, params).await
    }

    #[tool(
        description = "Archive a rule by setting its status to 'archived'. Archived rules are not evaluated and hidden from default listings."
    )]
    async fn rules_archive(
        &self,
        Parameters(params): Parameters<tools::rules::RulesStatusParams>,
    ) -> Result<String, String> {
        tools::rules::archive(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/rules_test.md")]
    #[tool]
    async fn rules_test(
        &self,
        Parameters(params): Parameters<tools::rules::RulesTestParams>,
    ) -> Result<String, String> {
        tools::rules::test(&self.client, params).await
    }

    #[tool(
        description = "Validate a rule definition without creating it. Returns validation errors and warnings. Useful for checking rule syntax before creating or updating."
    )]
    async fn rules_validate(
        &self,
        Parameters(params): Parameters<tools::rules::RulesValidateParams>,
    ) -> Result<String, String> {
        tools::rules::validate(&self.client, params).await
    }

    #[tool(
        description = "Update the rollout percentage for a rule (0-100). Controls what percentage of matching data is processed by this rule. Useful for gradual rollouts."
    )]
    async fn rules_rollout(
        &self,
        Parameters(params): Parameters<tools::rules::RulesRolloutParams>,
    ) -> Result<String, String> {
        tools::rules::rollout(&self.client, params).await
    }

    #[tool(
        description = "List all versions of a rule by its ID. Shows the version history including changes over time."
    )]
    async fn rules_versions(
        &self,
        Parameters(params): Parameters<tools::rules::RulesVersionsParams>,
    ) -> Result<String, String> {
        tools::rules::versions(&self.client, params).await
    }

    #[tool(
        description = "Create a new version of an existing rule. Snapshots the current state as a new version for version tracking."
    )]
    async fn rules_create_version(
        &self,
        Parameters(params): Parameters<tools::rules::RulesVersionsParams>,
    ) -> Result<String, String> {
        tools::rules::create_version(&self.client, params).await
    }

    #[tool(
        description = "Export rules from the server as a JSON array. Optionally filter by status, channel, or tag. Supports pagination. Useful for backup or GitOps workflows."
    )]
    async fn rules_export(
        &self,
        Parameters(params): Parameters<tools::rules::RulesExportParams>,
    ) -> Result<String, String> {
        tools::rules::export(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/rules_import.md")]
    #[tool]
    async fn rules_import(
        &self,
        Parameters(params): Parameters<tools::rules::RulesImportParams>,
    ) -> Result<String, String> {
        tools::rules::import(&self.client, params).await
    }

    // ── Connectors ─────────────────────────────────────────────────────

    #[tool(
        description = "List all connectors configured in the Orion server (HTTP and Kafka connectors for external service integration). Supports pagination with limit and offset."
    )]
    async fn connectors_list(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsListParams>,
    ) -> Result<String, String> {
        tools::connectors::list(&self.client, params).await
    }

    #[tool(
        description = "Get a connector by its ID, including configuration details (secrets are masked)"
    )]
    async fn connectors_get(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsGetParams>,
    ) -> Result<String, String> {
        tools::connectors::get(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/connectors_create.md")]
    #[tool]
    async fn connectors_create(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsCreateParams>,
    ) -> Result<String, String> {
        tools::connectors::create(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/connectors_update.md")]
    #[tool]
    async fn connectors_update(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsUpdateParams>,
    ) -> Result<String, String> {
        tools::connectors::update(&self.client, params).await
    }

    #[tool(description = "Delete a connector by its ID. This is irreversible.")]
    async fn connectors_delete(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsDeleteParams>,
    ) -> Result<String, String> {
        tools::connectors::delete(&self.client, params).await
    }

    #[tool(
        description = "Enable a connector so it can be used by rules for external service calls"
    )]
    async fn connectors_enable(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsToggleParams>,
    ) -> Result<String, String> {
        tools::connectors::enable(&self.client, params).await
    }

    #[tool(description = "Disable a connector to prevent it from being used by rules")]
    async fn connectors_disable(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsToggleParams>,
    ) -> Result<String, String> {
        tools::connectors::disable(&self.client, params).await
    }

    // ── Data ───────────────────────────────────────────────────────────

    #[doc = include_str!("tools/descriptions/data_send_sync.md")]
    #[tool]
    async fn data_send_sync(
        &self,
        Parameters(params): Parameters<tools::data::DataSendSyncParams>,
    ) -> Result<String, String> {
        tools::data::send_sync(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/data_send_async.md")]
    #[tool]
    async fn data_send_async(
        &self,
        Parameters(params): Parameters<tools::data::DataSendAsyncParams>,
    ) -> Result<String, String> {
        tools::data::send_async(&self.client, params).await
    }

    // ── Traces ──────────────────────────────────────────────────────────

    #[tool(
        description = "List execution traces. Traces record data processing results including status, duration, input/output, and errors. Supports filtering by status, channel, mode and pagination."
    )]
    async fn traces_list(
        &self,
        Parameters(params): Parameters<tools::traces::TracesListParams>,
    ) -> Result<String, String> {
        tools::traces::list(&self.client, params).await
    }

    #[tool(
        description = "Get a specific execution trace by its ID. Returns full trace details including input data, result, duration, and any error messages."
    )]
    async fn traces_get(
        &self,
        Parameters(params): Parameters<tools::traces::TracesGetParams>,
    ) -> Result<String, String> {
        tools::traces::get(&self.client, params).await
    }
}

#[tool_handler]
impl ServerHandler for OrionService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(include_str!("tools/descriptions/instructions.md").to_string()),
        }
    }
}

pub async fn serve(server_url: String, http: bool, bind: String) -> Result<()> {
    use rmcp::ServiceExt;
    use tracing_subscriber::EnvFilter;

    // Initialize tracing to stderr (stdout is used for MCP stdio transport)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let client = OrionClient::new(&server_url)?;

    if http {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };
        use tokio_util::sync::CancellationToken;

        let ct = CancellationToken::new();
        let http_service: StreamableHttpService<OrionService, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(OrionService::new(client.clone())),
                Default::default(),
                StreamableHttpServerConfig {
                    stateful_mode: true,
                    cancellation_token: ct.child_token(),
                    ..Default::default()
                },
            );

        let router = axum::Router::new().nest_service("/mcp", http_service);
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind to {bind}: {e}"))?;

        tracing::info!("Starting Orion MCP HTTP server on {bind} (connected to {server_url})");
        eprintln!("Orion MCP HTTP server listening on {bind}");

        axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct.cancelled_owned().await })
            .await
            .map_err(|e| anyhow::anyhow!("HTTP server error: {e}"))?;
    } else {
        tracing::info!("Starting Orion MCP server (connected to {server_url})");
        let service = OrionService::new(client);
        let server = service.serve(rmcp::transport::stdio()).await?;
        server.waiting().await?;
    }

    Ok(())
}

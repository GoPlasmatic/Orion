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
        description = "Get the current engine status including version, uptime, active workflows count, and available channels"
    )]
    async fn engine_status(&self) -> Result<String, String> {
        tools::engine::status(&self.client).await
    }

    #[tool(
        description = "Reload the engine to pick up workflow and channel changes. This hot-reloads without server restart."
    )]
    async fn engine_reload(&self) -> Result<String, String> {
        tools::engine::reload(&self.client).await
    }

    // ── Workflows ──────────────────────────────────────────────────────

    #[tool(
        description = "List all workflows in the Orion engine. Optionally filter by status (draft/active/archived) or tag. Supports pagination with limit and offset."
    )]
    async fn workflows_list(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsListParams>,
    ) -> Result<String, String> {
        tools::workflows::list(&self.client, params).await
    }

    #[tool(
        description = "Get a workflow by its ID, including full details like condition, tasks, tags, and version history count"
    )]
    async fn workflows_get(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsGetParams>,
    ) -> Result<String, String> {
        tools::workflows::get(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/workflows_create.md")]
    #[tool]
    async fn workflows_create(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsCreateParams>,
    ) -> Result<String, String> {
        tools::workflows::create(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/workflows_update.md")]
    #[tool]
    async fn workflows_update(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsUpdateParams>,
    ) -> Result<String, String> {
        tools::workflows::update(&self.client, params).await
    }

    #[tool(description = "Delete a workflow by its ID. This is irreversible.")]
    async fn workflows_delete(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsDeleteParams>,
    ) -> Result<String, String> {
        tools::workflows::delete(&self.client, params).await
    }

    #[tool(
        description = "Activate a workflow by setting its status to 'active'. Active workflows are evaluated during data processing."
    )]
    async fn workflows_activate(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsStatusParams>,
    ) -> Result<String, String> {
        tools::workflows::activate(&self.client, params).await
    }

    #[tool(
        description = "Archive a workflow by setting its status to 'archived'. Archived workflows are not evaluated and hidden from default listings."
    )]
    async fn workflows_archive(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsStatusParams>,
    ) -> Result<String, String> {
        tools::workflows::archive(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/workflows_test.md")]
    #[tool]
    async fn workflows_test(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsTestParams>,
    ) -> Result<String, String> {
        tools::workflows::test(&self.client, params).await
    }

    #[tool(
        description = "Validate a workflow definition without creating it. Returns validation errors and warnings. Useful for checking workflow syntax before creating or updating."
    )]
    async fn workflows_validate(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsValidateParams>,
    ) -> Result<String, String> {
        tools::workflows::validate(&self.client, params).await
    }

    #[tool(
        description = "Update the rollout percentage for a workflow (0-100). Controls what percentage of matching data is processed by this workflow. Useful for gradual rollouts."
    )]
    async fn workflows_rollout(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsRolloutParams>,
    ) -> Result<String, String> {
        tools::workflows::rollout(&self.client, params).await
    }

    #[tool(
        description = "List all versions of a workflow by its ID. Shows the version history including changes over time."
    )]
    async fn workflows_versions(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsVersionsParams>,
    ) -> Result<String, String> {
        tools::workflows::versions(&self.client, params).await
    }

    #[tool(
        description = "Create a new draft version of an existing workflow. Snapshots the current state as a new version for version tracking."
    )]
    async fn workflows_create_version(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsVersionsParams>,
    ) -> Result<String, String> {
        tools::workflows::create_version(&self.client, params).await
    }

    #[tool(
        description = "Export workflows from the server as a JSON array. Optionally filter by status or tag. Supports pagination. Useful for backup or GitOps workflows."
    )]
    async fn workflows_export(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsExportParams>,
    ) -> Result<String, String> {
        tools::workflows::export(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/workflows_import.md")]
    #[tool]
    async fn workflows_import(
        &self,
        Parameters(params): Parameters<tools::workflows::WorkflowsImportParams>,
    ) -> Result<String, String> {
        tools::workflows::import(&self.client, params).await
    }

    // ── Channels ──────────────────────────────────────────────────────

    #[tool(
        description = "List all channels in the Orion engine. Optionally filter by status (draft/active/archived), channel type (sync/async), or protocol (http/rest/kafka). Supports pagination."
    )]
    async fn channels_list(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsListParams>,
    ) -> Result<String, String> {
        tools::channels::list(&self.client, params).await
    }

    #[tool(
        description = "Get a channel by its ID, including full details like protocol, route pattern, workflow link, and configuration"
    )]
    async fn channels_get(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsGetParams>,
    ) -> Result<String, String> {
        tools::channels::get(&self.client, params).await
    }

    #[doc = include_str!("tools/descriptions/channels_create.md")]
    #[tool]
    async fn channels_create(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsCreateParams>,
    ) -> Result<String, String> {
        tools::channels::create(&self.client, params).await
    }

    #[tool(
        description = "Update a draft channel by ID. Only draft channels can be updated. The channel_json contains only the fields to update."
    )]
    async fn channels_update(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsUpdateParams>,
    ) -> Result<String, String> {
        tools::channels::update(&self.client, params).await
    }

    #[tool(description = "Delete a channel by its ID. This is irreversible.")]
    async fn channels_delete(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsDeleteParams>,
    ) -> Result<String, String> {
        tools::channels::delete(&self.client, params).await
    }

    #[tool(
        description = "Activate a channel by setting its status to 'active'. Active channels accept and process incoming data."
    )]
    async fn channels_activate(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsStatusParams>,
    ) -> Result<String, String> {
        tools::channels::activate(&self.client, params).await
    }

    #[tool(
        description = "Archive a channel by setting its status to 'archived'. Archived channels no longer accept data."
    )]
    async fn channels_archive(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsStatusParams>,
    ) -> Result<String, String> {
        tools::channels::archive(&self.client, params).await
    }

    #[tool(description = "List all versions of a channel by its ID. Shows the version history.")]
    async fn channels_versions(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsVersionsParams>,
    ) -> Result<String, String> {
        tools::channels::versions(&self.client, params).await
    }

    #[tool(description = "Create a new draft version of an existing channel for version tracking.")]
    async fn channels_create_version(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsVersionsParams>,
    ) -> Result<String, String> {
        tools::channels::create_version(&self.client, params).await
    }

    #[tool(
        description = "Bulk import channels from a JSON array. Use dry_run=true to validate on the server without writing (returns would_create/would_fail counts and per-item errors)."
    )]
    async fn channels_import(
        &self,
        Parameters(params): Parameters<tools::channels::ChannelsImportParams>,
    ) -> Result<String, String> {
        tools::channels::import(&self.client, params).await
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
        description = "Enable a connector so it can be used by workflows for external service calls"
    )]
    async fn connectors_enable(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsToggleParams>,
    ) -> Result<String, String> {
        tools::connectors::enable(&self.client, params).await
    }

    #[tool(description = "Disable a connector to prevent it from being used by workflows")]
    async fn connectors_disable(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsToggleParams>,
    ) -> Result<String, String> {
        tools::connectors::disable(&self.client, params).await
    }

    #[tool(
        description = "Bulk import connectors from a JSON array. Use dry_run=true to validate on the server without writing (returns would_create/would_fail counts and per-item errors)."
    )]
    async fn connectors_import(
        &self,
        Parameters(params): Parameters<tools::connectors::ConnectorsImportParams>,
    ) -> Result<String, String> {
        tools::connectors::import(&self.client, params).await
    }

    // ── Circuit Breakers ──────────────────────────────────────────────

    #[tool(
        description = "List all circuit breaker states for connectors. Shows which connector-channel pairs have tripped breakers."
    )]
    async fn circuit_breakers_list(
        &self,
        Parameters(params): Parameters<tools::circuit_breakers::CircuitBreakersListParams>,
    ) -> Result<String, String> {
        tools::circuit_breakers::list(&self.client, params).await
    }

    #[tool(
        description = "Reset a circuit breaker by its key (format: connector:channel). Allows requests to flow through again after a breaker trip."
    )]
    async fn circuit_breaker_reset(
        &self,
        Parameters(params): Parameters<tools::circuit_breakers::CircuitBreakerResetParams>,
    ) -> Result<String, String> {
        tools::circuit_breakers::reset(&self.client, params).await
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

    // ── Audit Logs ──────────────────────────────────────────────────────

    #[doc = include_str!("tools/descriptions/audit_logs_list.md")]
    #[tool]
    async fn audit_logs_list(
        &self,
        Parameters(params): Parameters<tools::audit_logs::AuditLogsListParams>,
    ) -> Result<String, String> {
        tools::audit_logs::list(&self.client, params).await
    }

    // ── Backups ────────────────────────────────────────────────────────

    #[doc = include_str!("tools/descriptions/backups_create.md")]
    #[tool]
    async fn backups_create(&self) -> Result<String, String> {
        tools::backups::create(&self.client).await
    }

    #[doc = include_str!("tools/descriptions/backups_list.md")]
    #[tool]
    async fn backups_list(&self) -> Result<String, String> {
        tools::backups::list(&self.client).await
    }

    // ── Functions ──────────────────────────────────────────────────────

    #[doc = include_str!("tools/descriptions/functions_list.md")]
    #[tool]
    async fn functions_list(&self) -> Result<String, String> {
        tools::functions::list(&self.client).await
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
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2024_11_05;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(include_str!("tools/descriptions/instructions.md").to_string());
        info
    }
}

pub async fn serve(
    server_url: String,
    http: bool,
    bind: String,
    api_key: Option<(String, Option<String>)>,
) -> Result<()> {
    use rmcp::ServiceExt;
    use tracing_subscriber::EnvFilter;

    // Initialize tracing to stderr (stdout is used for MCP stdio transport)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut client = OrionClient::new(&server_url)?;
    if let Some((key, header)) = api_key {
        client = client.with_api_key(key, header);
    }

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
                {
                    let mut config = StreamableHttpServerConfig::default();
                    config.stateful_mode = true;
                    config.cancellation_token = ct.child_token();
                    config
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

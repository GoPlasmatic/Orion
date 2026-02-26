use serde::Serialize;
use utoipa::OpenApi;

/// Error response body matching Orion's `{"error": {"code": "...", "message": "..."}}` format.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ErrorDetail {
    code: String,
    message: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Orion Rules Engine API",
        version = env!("CARGO_PKG_VERSION"),
        description = "JSONLogic-based rules engine service",
        license(name = "Apache-2.0"),
    ),
    tags(
        (name = "Rules", description = "Rule management"),
        (name = "Connectors", description = "Connector management"),
        (name = "Engine", description = "Engine control"),
        (name = "Data", description = "Data processing"),
        (name = "Operational", description = "Health and metrics"),
    ),
    paths(
        // Rules (10)
        super::admin::list_rules,
        super::admin::create_rule,
        super::admin::get_rule,
        super::admin::update_rule,
        super::admin::delete_rule,
        super::admin::change_rule_status,
        super::admin::test_rule,
        super::admin::import_rules,
        super::admin::export_rules,
        super::admin::validate_rule,
        // Connectors (5)
        super::admin::list_connectors,
        super::admin::create_connector,
        super::admin::get_connector,
        super::admin::update_connector,
        super::admin::delete_connector,
        // Engine (2)
        super::admin::engine_status,
        super::admin::engine_reload,
        // Data (5)
        super::data::sync_process,
        super::data::async_submit,
        super::data::list_jobs,
        super::data::get_job,
        super::data::batch_process,
        // Operational (2)
        super::health_check,
        super::metrics_endpoint,
    ),
    components(
        schemas(
            crate::storage::models::Rule,
            crate::storage::models::Connector,
            crate::storage::models::Job,
            crate::storage::repositories::rules::CreateRuleRequest,
            crate::storage::repositories::rules::UpdateRuleRequest,
            crate::storage::repositories::rules::StatusChangeRequest,
            crate::storage::repositories::connectors::CreateConnectorRequest,
            crate::storage::repositories::connectors::UpdateConnectorRequest,
            super::admin::TestRuleRequest,
            super::admin::ValidationResponse,
            super::admin::ValidationIssue,
            super::data::ProcessRequest,
            super::data::BatchRequest,
            super::data::BatchMessage,
            ErrorResponse,
            ErrorDetail,
        )
    )
)]
pub(crate) struct ApiDoc;

mod channels;
pub(crate) mod common;
mod connectors;
mod cron;
pub mod endpoints;
pub mod ssrf;
mod workflows;

pub use channels::{validate_channel_id, validate_create_channel, validate_update_channel};

/// The refused-guard list, for `channel::guards`'s agreement test.
///
/// Exposed rather than duplicated: the test's whole point is that there is one
/// list, so it must read the real one.
#[cfg(test)]
pub(crate) fn cron_config_errors_for_test(
    config: &serde_json::Value,
) -> Vec<crate::errors::FieldError> {
    cron::cron_config_errors(config)
}
pub use connectors::{
    reject_masked_values, validate_connector_config, validate_create_connector,
    validate_update_connector,
};
pub use endpoints::{
    check_broker_endpoints, check_cache_endpoint, check_db_endpoint, check_mongo_hosts,
};
pub use ssrf::{PinnedDnsResolver, validate_hostport_not_private, validate_url_not_private};
pub use workflows::{
    EngineAdvisory, engine_advisories, secret_reference_errors, unresolvable_logic_warnings,
    validate_create_workflow, validate_update_workflow, validate_workflow_id,
    validate_workflow_loop_schema, validate_workflow_tasks_schema,
};

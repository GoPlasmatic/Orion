//! Consolidated integration-test binary.
//!
//! Every former `tests/*.rs` file is now a module here so the whole suite
//! links into a single binary instead of ~43 separate ones. This collapses
//! the per-edit relink cost dramatically. Add new test files under
//! `tests/integration/` and declare them below.
#![allow(dead_code)]

mod common;

mod admin_connectors_test;
mod admin_workflows_test;
mod async_trace_edge_test;
mod async_traces_test;
mod audit_log_test;
mod backup_restore_test;
mod bulk_import_test;
mod channel_call_test;
mod channel_config_test;
mod channel_config_validation_test;
mod channel_dedup_test;
mod channel_response_cache_test;
mod circuit_breaker_test;
mod cli_subcommands_test;
mod cluster_epoch_test;
mod concurrency_test;
mod connector_cache_test;
mod connector_db_test;
mod connector_ops_test;
mod connector_redis_test;
mod data_query_test;
mod data_roundtrip_test;
mod data_write_test;
mod drain_test;
mod error_envelope_test;
mod error_paths_test;
mod es_test;
mod function_schema_test;
mod kafka_test;
mod mongodb_test;
mod mysql_test;
mod openapi_test;
mod pipeline_test;
mod pool_exhaustion_test;
mod postgres_test;
mod profile_test;
mod protocol_required_fields_test;
mod rate_limit_test;
mod rest_routing_test;
mod scenario_api_gateway_test;
mod scenario_ecommerce_test;
mod scenario_webhook_test;
mod secret_references_test;
mod security_test;
mod shutdown_test;
mod task_trace_test;
mod trace_dlq_repo_test;
mod trace_storage_test;
mod typed_enum_dtos_test;
mod workflow_lifecycle_test;

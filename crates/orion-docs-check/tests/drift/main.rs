//! The drift guards: each holds a document to the code it describes and
//! fails when the two disagree — the config reference to `AppConfig`, the
//! metrics page to the recorders, the functions pages to the registry, the
//! book's routes to the router, the error and audit vocabularies to what the
//! source emits, the CI filters to the container-gated modules. One small
//! binary over the library crates, so checking a docs edit does not build
//! the server's integration suite.

// Tests unwrap and panic on what they did not expect; the workspace's
// lints against both are for production code.
#![allow(clippy::unwrap_used, clippy::panic)]

mod audit_actions_drift_test;
mod ci_filter_drift_test;
mod clippy_docs_drift_test;
mod config_docs_drift_test;
mod docs_routes_drift_test;
mod field_codes_drift_test;
mod fmt_style_drift_test;
mod functions_docs_drift_test;
mod mdbook_pin_drift_test;
mod metrics_docs_drift_test;
mod v2_error_codes_drift_test;

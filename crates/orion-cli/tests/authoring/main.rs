//! The offline authoring tools — `lint`, `compile`, `dry-run`, `test`, `fmt`,
//! `clippy`, `sql check` and `plugin` — driven through the compiled
//! `orion-cli` binary. One test binary: each file is a module declared here.
//! Fixtures are the server crate's (`../orion-server/tests/fixtures`), which
//! its own library-level suites share.

// Tests unwrap and panic on what they did not expect; the workspace's
// lints against both are for production code.
#![expect(clippy::unwrap_used, clippy::panic)]

mod common;

mod cli_subcommands_test;
mod clippy_cli_test;
mod clippy_fix_test;
mod compile_test;
mod fmt_cli_test;
mod secrets_offline_test;
mod signing_cli_test;
mod sql_check_cli_test;
mod sql_check_test;

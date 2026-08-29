//! Dependency direction, enforced (the item-9 drift guard).
//!
//! `CLAUDE.md` describes the module tree as layers, and nothing checked it.
//! Counting `crate::X` references per module found five upward edges, each one
//! small and each one arrived at by adding a feature in the nearest file
//! rather than the right one:
//!
//! - `engine` reached into `server::state` for `AppState` and `bootstrap` for
//!   `start_kafka_ingest`, because engine *reload* lived in `engine`;
//! - `connector::sigv4`, `channel::auth` and `jwt` reached into
//!   `engine::operators` for HMAC and the base64/hex table, because the
//!   JSONLogic operator module was where those primitives were first written;
//! - `errors` and `channel::error_body` read a task-local out of
//!   `server::request_context`;
//! - `engine::functions::http_common` reached into `server::trace_context` to
//!   propagate a `traceparent`;
//! - `connector::smtp_pool` reached into `server::tls` to install a rustls
//!   crypto provider.
//!
//! All five are gone — `runtime/`, `crypto/`, `request_context/` and
//! `trace_context/` are where those things live now. This test is what stops
//! them growing back: it is a text scan, deliberately, because the thing it
//! guards is a *convention*, and a convention no one can see is a convention
//! that erodes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A module, and the modules it may not name.
///
/// Read it as "below": `engine` is below `server`, so `engine` naming
/// `crate::server::…` is the edge pointing the wrong way. The list is not
/// every pair — only the ones that were actually violated, plus the ones the
/// same reasoning obviously extends to.
const FORBIDDEN: &[(&str, &[&str])] = &[
    // The request path sits below the HTTP layer and the startup sequence.
    ("engine", &["crate::server::", "crate::bootstrap::"]),
    ("channel", &["crate::server::", "crate::bootstrap::"]),
    ("connector", &["crate::server::", "crate::bootstrap::"]),
    ("kafka", &["crate::server::", "crate::bootstrap::"]),
    ("queue", &["crate::server::", "crate::bootstrap::"]),
    ("jwt", &["crate::server::", "crate::bootstrap::"]),
    // Storage and config are below everything that serves.
    (
        "storage",
        &["crate::server::", "crate::bootstrap::", "crate::engine::"],
    ),
    // Every module in the tree produces an `OrionError`, so the error type
    // cannot depend on any of them.
    (
        "errors.rs",
        &["crate::server::", "crate::bootstrap::", "crate::engine::"],
    ),
    // The shared primitives are leaves.
    (
        "crypto.rs",
        &["crate::server::", "crate::engine::", "crate::channel::"],
    ),
    ("trace_context.rs", &["crate::server::", "crate::engine::"]),
    (
        "request_context.rs",
        &["crate::server::", "crate::engine::"],
    ),
    // `runtime::reload` and `runtime::handler_deps` take an `AppState`, so
    // nothing below the HTTP layer may name them. `runtime::tasks` is
    // deliberately *not* on this list: the supervisor is a leaf that depends
    // on nothing in the tree, and `queue`, `cluster` and the retention jobs
    // take a `&TaskRegistry` as a parameter — injected downward, the same
    // shape as `metrics`. Forbidding all of `runtime::` would refuse that.
    (
        "engine",
        &["crate::runtime::reload", "crate::runtime::handler_deps"],
    ),
    (
        "channel",
        &["crate::runtime::reload", "crate::runtime::handler_deps"],
    ),
    (
        "queue",
        &["crate::runtime::reload", "crate::runtime::handler_deps"],
    ),
    ("storage", &["crate::runtime::"]),
];

/// Edges that are real, deliberate, and left alone — each with the reason.
///
/// Adding a row here is a decision, not a workaround: it says the edge is the
/// lesser evil, and the next reader gets to judge that.
const ALLOWED: &[(&str, &str)] = &[
    // `config::server` validates that a channel mount does not shadow a
    // platform route, and the list of platform routes is the router's to know.
    // Moving it into `config` would split it from the router it describes,
    // which is the drift this whole family of tests exists to prevent.
    (
        "config/server.rs",
        "crate::server::routes::shadowed_platform_route",
    ),
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/<module>` (or the file `src/<module>` itself
/// when the entry names one).
fn files_of(module: &str) -> Vec<PathBuf> {
    let root = src_dir().join(module);
    if root.is_file() {
        return vec![root];
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out
}

/// Whether `line` is a doc comment or a plain comment. A module may *mention*
/// a higher layer in prose — several of them explain why they no longer
/// depend on one — and that is not an edge.
fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

#[test]
fn no_module_depends_on_a_layer_above_it() {
    let allowed: BTreeSet<(&str, &str)> = ALLOWED.iter().copied().collect();
    let mut violations = Vec::new();

    for (module, forbidden) in FORBIDDEN {
        for file in files_of(module) {
            let text = std::fs::read_to_string(&file).expect("read source");
            let rel = file
                .strip_prefix(src_dir())
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            for (n, line) in text.lines().enumerate() {
                if is_comment(line) {
                    continue;
                }
                for needle in *forbidden {
                    if line.contains(needle)
                        && !allowed
                            .iter()
                            .any(|(f, snippet)| *f == rel && line.contains(snippet))
                    {
                        violations.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "these modules name a layer above them:\n  {}\n\n\
         A lower layer needing something from a higher one usually means the \
         thing itself is in the wrong place — that was true of all five edges \
         this test was written to close (see the module docs). Move it down, \
         or add a row to ALLOWED with the reason.",
        violations.join("\n  ")
    );
}

/// An `ALLOWED` row that no longer matches anything is a rule nobody is
/// following any more, and a reader would take it as describing the code.
#[test]
fn every_allowed_edge_still_exists() {
    for (file, snippet) in ALLOWED {
        let path = src_dir().join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("ALLOWED names {file}, which does not read: {e}"));
        assert!(
            text.contains(snippet),
            "ALLOWED says {file} contains `{snippet}`, and it does not — \
             delete the row rather than leaving it to describe code that is gone"
        );
    }
}

//! Dependency direction, enforced (the item-9 drift guard).
//!
//! `CLAUDE.md` describes the module tree as layers, and nothing checked it.
//! Counting `crate::X` references per module found five upward edges, each one
//! small and each one arrived at by adding a feature in the nearest file
//! rather than the right one:
//!
//! - `engine` reached into `server::state` for `AppState` and `bootstrap` for
//!   the ingest starter, because engine *reload* lived in `engine`;
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
//! All five are gone — `runtime/` is where reload lives now, and the shared
//! primitives (`crypto`, `request_context`, `trace_context`, `errors`,
//! `config`, …) are the `orion-common` crate, storage is `orion-storage` and
//! the engine with its connectors, query dialect and JWT core is
//! `orion-engine`: their crate boundaries enforce their being below
//! everything that serves. This test is what stops the rest growing back: it is a text scan, deliberately, because the thing it
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
    ("channel", &["crate::server::", "crate::bootstrap::"]),
    ("queue", &["crate::server::", "crate::bootstrap::"]),
    // The plugin sandbox produces registry entries and handlers for a
    // generation to carry; it names nothing above `engine`.
    ("plugin", &["crate::server::", "crate::bootstrap::"]),
    // The cron scheduler sits beside `queue`: it consumes the
    // engine, the channel estate and the storage layer, and nothing below it
    // may name it back. `engine` reaching up here is how `TRIGGER_KEY` briefly
    // ended up in the wrong module.
    ("cron", &["crate::server::", "crate::bootstrap::"]),
    ("channel", &["crate::cron::"]),
    ("queue", &["crate::cron::"]),
    // The one SQL lexer: read by `db_read`/`db_write` (below `definitions`),
    // by the `$sql` pass and the clippy rules (above `engine`), and by the
    // binary's `sql check`. None of them may reach the others for it.
    (
        "sql_lex.rs",
        &[
            "crate::server::",
            "crate::engine::",
            "crate::definitions::",
            "crate::connector::",
        ],
    ),
    // `runtime::reload` and `runtime::handler_deps` take an `AppState`, so
    // nothing below the HTTP layer may name them. `runtime::tasks` and
    // `runtime::generation` are deliberately *not* on this list: both are
    // leaves that depend on nothing above them, and both are injected
    // downward as a parameter or a field — `queue`, `cluster` and the
    // retention jobs take a `&TaskRegistry`; `engine`, `channel`, `cron` and
    // `queue` are handed an `Arc<RuntimeHandle>` and read the generation they
    // were given. That is the same shape as `metrics`. Forbidding all of
    // `runtime::` would refuse it.
    (
        "channel",
        &["crate::runtime::reload", "crate::runtime::handler_deps"],
    ),
    (
        "queue",
        &["crate::runtime::reload", "crate::runtime::handler_deps"],
    ),
    (
        "plugin",
        &["crate::runtime::reload", "crate::runtime::handler_deps"],
    ),
    // V2 extension seams and composition root sit below the HTTP server,
    // bootstrap, reload, and background workers (`TD-36`).
    (
        "extensions",
        &[
            "crate::server::",
            "crate::bootstrap::",
            "crate::runtime::reload",
            "crate::runtime::handler_deps",
            "crate::cron::",
            "crate::queue::",
        ],
    ),
];

/// Edges that are real, deliberate, and left alone — each with the reason.
///
/// Adding a row here is a decision, not a workaround: it says the edge is the
/// lesser evil, and the next reader gets to judge that.
const ALLOWED: &[(&str, &str)] = &[];

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
        let files = files_of(module);
        // A row naming a module that moved would otherwise pass vacuously.
        assert!(
            !files.is_empty(),
            "FORBIDDEN names `{module}`, which has no source here"
        );
        for file in files {
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

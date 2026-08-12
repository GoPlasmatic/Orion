//! Drift guard for the HTTP endpoints the book tells readers to call.
//!
//! `docs_link_test` keeps the book's internal links honest and
//! `config_docs_drift_test` keeps its settings honest, but the `curl` examples
//! — the lines a reader is most likely to paste into a terminal — were checked
//! by nobody. The 1.0 documentation audit found three that could not work, and
//! two of them had the same shape: `POST /admin/trace-dlq/purge` with
//! `?older_than_hours=168`, when the handler takes that value in a JSON body
//! and refuses a request without one. A wrong path or method fails loudly; a
//! parameter in the wrong *place* fails just as hard while looking right.
//!
//! The OpenAPI document is the route table, so it is what this checks against:
//! it is generated from the handlers themselves, and `openapi_test` already
//! pins the committed copy to it. For every endpoint reference in `docs/src`
//! this asserts:
//!
//! 1. The path resolves to a real route, with path parameters matching any
//!    single segment (so `/workflows/order-processing/status` matches
//!    `/workflows/{id}/status`).
//! 2. The HTTP method is one that route serves.
//! 3. Every query parameter is one the operation declares — the check that
//!    catches a body field written as a query string.
//!
//! Two reference forms are read, because the audit found a broken example in
//! each: a `curl` against `http://localhost:8080`, and an inline code span
//! like `` `POST /api/v1/admin/engine/reload` ``. The inline form is only read
//! when the path starts with `/api/v1/`, which is what separates a real
//! reference from the shorthand the prose uses for a family of endpoints
//! (`POST /{kind}/import`, `POST /engine/reload`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

const DOCS_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/src");
const HOST: &str = "http://localhost:8080";

/// Paths served by something other than a documented operation.
///
/// The data plane is a single dynamic handler: every channel defines its own
/// route pattern and method set at runtime, so `/api/v1/data/**` cannot be
/// resolved against a static table and any shape under it is legitimate. The
/// other two are mounted by the Swagger UI layer rather than declared as
/// operations, so they are absent from the document that describes them.
fn is_unroutable_by_design(path: &str) -> bool {
    path.starts_with("/api/v1/data/") || path == "/docs" || path == "/api/v1/openapi.json"
}

/// One route the server actually serves: method, path template, and the query
/// parameters that operation declares.
struct Route {
    method: String,
    segments: Vec<String>,
    query: BTreeSet<String>,
}

impl Route {
    /// Whether this route serves `segments`, treating a `{param}` template
    /// segment as matching any single concrete segment. Placeholder text in
    /// the docs (`{trace-id}`, `<trace-id>`) is therefore matched by the
    /// parameter it stands in for, without the test having to recognise it.
    fn matches(&self, segments: &[String]) -> bool {
        self.segments.len() == segments.len()
            && std::iter::zip(&self.segments, segments)
                .all(|(template, actual)| template.starts_with('{') || template == actual)
    }
}

/// The route table, read from the generated OpenAPI document.
///
/// `pretty_json` is the same accessor `orion-server dump-openapi` and the
/// committed-copy drift check use, so this cannot be checking a spec that
/// differs from the shipped one.
fn routes() -> Vec<Route> {
    let spec: Value = serde_json::from_str(&orion::server::routes::openapi::pretty_json())
        .expect("the generated OpenAPI document parses");
    let paths = spec["paths"].as_object().expect("spec has paths");

    let mut routes = Vec::new();
    for (path, operations) in paths {
        let segments: Vec<String> = path
            .split('/')
            .skip(1)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        for (method, operation) in operations.as_object().expect("path item is an object") {
            let method = method.to_uppercase();
            if !is_http_method(&method) {
                continue;
            }
            let query = operation["parameters"]
                .as_array()
                .map(|params| {
                    params
                        .iter()
                        .filter(|p| p["in"] == "query")
                        .filter_map(|p| p["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            routes.push(Route {
                method,
                segments: segments.clone(),
                query,
            });
        }
    }
    assert!(
        !routes.is_empty(),
        "the OpenAPI document declared no operations"
    );
    routes
}

fn is_http_method(word: &str) -> bool {
    matches!(
        word,
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    )
}

/// One endpoint reference found in the book.
struct Reference {
    file: String,
    line: usize,
    method: String,
    /// Path with the query string and fragment removed.
    path: String,
    query: Vec<String>,
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {} {}",
            self.file, self.line, self.method, self.path
        )
    }
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read docs dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

/// Split `/a/b?x=1#frag` into its segments and its query parameter names.
fn split_target(target: &str) -> (Vec<String>, Vec<String>) {
    let path = target.split('#').next().unwrap_or(target);
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let segments = path
        .split('/')
        .skip(1)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split('=').next().unwrap_or(p).to_string())
        .collect();
    (segments, query)
}

/// Every endpoint reference in one page.
///
/// A backslash-continued `curl` is reassembled into one logical command, so a
/// `-X` and a URL on different lines are read together. The reported line is
/// the real file line the command *starts* on — the place a reader has to edit
/// — which is why continuations are gathered here rather than by flattening the
/// whole page first: that shifts every subsequent line number.
fn references(file: &Path) -> Vec<Reference> {
    let name = file
        .strip_prefix(DOCS_SRC)
        .unwrap_or(file)
        .display()
        .to_string();
    let body = std::fs::read_to_string(file).expect("read markdown file");
    let mut found = Vec::new();

    for (start, command) in shell_commands(&body) {
        if !command.contains("curl") || !command.contains(HOST) {
            continue;
        }
        let method = explicit_method(&command).unwrap_or_else(|| "GET".to_string());
        for (offset, _) in command.match_indices(HOST) {
            let target: String = command[offset + HOST.len()..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !"\"'`|)".contains(*c))
                .collect();
            let (_, query) = split_target(&target);
            found.push(Reference {
                file: name.clone(),
                line: start,
                method: method.clone(),
                path: target.split(['?', '#']).next().unwrap_or("").to_string(),
                query,
            });
        }
    }

    for (index, line) in body.lines().enumerate() {
        for span in code_spans(line) {
            let Some((method, target)) = span.trim().split_once(' ') else {
                continue;
            };
            if !is_http_method(method) || !target.starts_with("/api/v1/") {
                continue;
            }
            let (_, query) = split_target(target);
            found.push(Reference {
                file: name.clone(),
                line: index + 1,
                method: method.to_string(),
                path: target.split(['?', '#']).next().unwrap_or("").to_string(),
                query,
            });
        }
    }

    found
}

/// The page's lines, with backslash continuations folded into the line they
/// continue, paired with that line's 1-based number in the file.
fn shell_commands(body: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = body.lines().collect();
    let mut out = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let start = index + 1;
        let mut command = String::new();
        loop {
            match lines[index].strip_suffix('\\') {
                Some(continued) if index + 1 < lines.len() => {
                    command.push_str(continued);
                    command.push(' ');
                    index += 1;
                }
                _ => {
                    command.push_str(lines[index]);
                    break;
                }
            }
        }
        out.push((start, command));
        index += 1;
    }
    out
}

/// `-X POST` / `--request POST`, if the command names one.
fn explicit_method(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    while let Some(word) = words.next() {
        if word == "-X" || word == "--request" {
            let candidate = words.next()?.to_uppercase();
            if is_http_method(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The contents of each single-backtick code span on a line.
fn code_spans(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        spans.push(&after[..close]);
        rest = &after[close + 1..];
    }
    spans
}

fn all_references() -> Vec<Reference> {
    let mut files = Vec::new();
    markdown_files(Path::new(DOCS_SRC), &mut files);
    files.sort();
    files.iter().flat_map(|f| references(f)).collect()
}

/// Every documented endpoint resolves to a route the server serves, by that
/// method, with only query parameters that operation declares.
#[test]
fn documented_endpoints_resolve_to_real_routes() {
    let routes = routes();
    let mut problems = Vec::new();

    for reference in all_references() {
        if is_unroutable_by_design(&reference.path) {
            continue;
        }
        let (segments, _) = split_target(&reference.path);
        if segments.is_empty() {
            continue;
        }

        let matched = routes
            .iter()
            .find(|r| r.method == reference.method && r.matches(&segments));

        let Some(route) = matched else {
            let others: Vec<&str> = routes
                .iter()
                .filter(|r| r.matches(&segments))
                .map(|r| r.method.as_str())
                .collect();
            let hint = if others.is_empty() {
                "no route with that path".to_string()
            } else {
                format!("that path serves {}", others.join(", "))
            };
            problems.push(format!("{reference} — {hint}"));
            continue;
        };

        for name in &reference.query {
            if !route.query.contains(name.as_str()) {
                problems.push(format!(
                    "{reference} — `{name}` is not a query parameter of that operation \
                     (a body field written as a query string looks exactly like this)"
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "the book documents {} endpoint call(s) the server does not serve:\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

/// The extractor itself finds something.
///
/// Without this, a regression that stops recognising `curl` lines would turn
/// the check above into a green no-op — the failure mode every "walk the docs"
/// test has.
#[test]
fn the_extractor_finds_the_books_endpoint_references() {
    let references = all_references();
    assert!(
        references.len() > 50,
        "expected the book to reference many endpoints, found {} — the extractor \
         has probably stopped recognising a form it used to read",
        references.len()
    );
    assert!(
        references.iter().any(|r| r.method == "PATCH"),
        "no PATCH reference found; the method parser has regressed"
    );
    assert!(
        references.iter().any(|r| !r.query.is_empty()),
        "no reference with a query string found; the query parser has regressed"
    );
}

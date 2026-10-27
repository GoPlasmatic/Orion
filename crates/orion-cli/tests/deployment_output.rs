//! Machine output must be one complete document, including when a wait fails.
#![expect(clippy::unwrap_used, clippy::panic)]

#[path = "common/http.rs"]
mod http;

use serde_json::{Value, json};

fn applied() -> Value {
    json!({
        "committed_revision": 7,
        "receipt": {
            "operation_id": "op-7", "idempotency_key": "apply-7", "principal": "admin",
            "request_hash": "hash", "committed_revision": 7, "manifest_digest": "digest",
            "epoch": 7, "receiving_node_id": "local", "created_at": "2026-10-27T00:00:00"
        },
        "warnings": ["retained warning"],
        "evaluation": {
            "converged": false, "target_revision": 7,
            "converged_nodes": [], "pending_nodes": ["local"], "degraded_nodes": []
        },
        "local_error": "temporary build failure"
    })
}

// None: polling fails. Some(n): n pending reports, then convergence.
fn server(pending_reports: Option<usize>) -> http::Server {
    let mut polls = 0;
    http::Server::new(move |method, path, body| match (method, path) {
        ("GET", "/api/v2/admin/runtime-capabilities") => (200, json!({"api_version": 1})),
        ("POST", "/api/v2/admin/deployments") => {
            assert_eq!(body.unwrap()["base_revision"], 6);
            (202, applied())
        }
        ("GET", "/api/v2/admin/deployments/7/convergence?target=observed") => {
            let Some(pending) = pending_reports else {
                return (
                    503,
                    json!({"error": {"code": "UNAVAILABLE", "message": "poll failed"}}),
                );
            };
            let converged = polls >= pending;
            polls += 1;
            (
                200,
                json!({
                    "converged": converged, "target_revision": 7,
                    "converged_nodes": if converged { vec!["local", "peer"] } else { vec!["local"] },
                    "pending_nodes": if converged { vec![] } else { vec!["peer"] },
                    "degraded_nodes": []
                }),
            )
        }
        _ => panic!("unexpected {method} {path}"),
    })
}

fn apply(server: &http::Server, format: &str, quiet: bool, wait_ms: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("manifest.json");
    std::fs::write(
        &manifest,
        r#"{"schema_version":2,"providers":[],"members":[]}"#,
    )
    .unwrap();
    server.cli(
        format,
        quiet,
        &[
            "deployments",
            "apply",
            "-f",
            manifest.to_str().unwrap(),
            "--base-revision",
            "6",
            "--reviewed-digest",
            "digest",
            "--idempotency-key",
            "apply-7",
            "--wait-ms",
            wait_ms,
            "--wait-target",
            "observed",
        ],
    )
}

fn document(output: &std::process::Output, format: &str, exit: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if format == "json" {
        serde_json::from_slice(&output.stdout).unwrap()
    } else {
        serde_saphyr::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap()
    }
}

#[test]
fn apply_wait_emits_one_document_preserving_receipt_and_both_evaluations() {
    for format in ["json", "yaml"] {
        let peer = server(Some(1));
        let result = document(&apply(&peer, format, false, "1000"), format, 0);
        for field in [
            "committed_revision",
            "receipt",
            "warnings",
            "evaluation",
            "local_error",
        ] {
            assert_eq!(result[field], applied()[field], "{result}");
        }
        assert_eq!(result["convergence"]["converged"], true, "{result}");
        assert_eq!(
            result["convergence"]["converged_nodes"],
            json!(["local", "peer"])
        );
    }
}

#[test]
fn apply_timeout_preserves_receipt_and_last_pending_report() {
    for format in ["json", "yaml"] {
        let peer = server(Some(usize::MAX));
        let output = apply(&peer, format, false, "50");
        let result = document(&output, format, 3);
        assert_eq!(result["receipt"], applied()["receipt"]);
        assert_eq!(result["committed_revision"], 7);
        assert_eq!(result["convergence"]["converged"], false);
        assert_eq!(result["convergence"]["pending_nodes"], json!(["peer"]));
        assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    }
}

#[test]
fn quiet_apply_prints_the_revision_once_with_or_without_wait() {
    for (pending, wait_ms, exit) in [(0, "1000", 0), (usize::MAX, "50", 3), (0, "0", 0)] {
        let peer = server(Some(pending));
        let output = apply(&peer, "json", true, wait_ms);
        assert_eq!(output.status.code(), Some(exit));
        assert_eq!(output.stdout, b"7\n");
    }
}

#[test]
fn apply_without_wait_preserves_the_existing_shape_and_poll_errors_keep_the_receipt() {
    let peer = server(None);
    let output = apply(&peer, "json", false, "0");
    assert_eq!(document(&output, "json", 0), applied());
    let output = apply(&peer, "json", false, "1000");
    assert_eq!(document(&output, "json", 1), applied());
    assert!(String::from_utf8_lossy(&output.stderr).contains("poll failed"));
}

#[test]
fn standalone_wait_emits_one_report_on_success_and_timeout() {
    for (pending, exit) in [(0, 0), (usize::MAX, 3)] {
        for format in ["json", "yaml"] {
            for quiet in [false, true] {
                let peer = server(Some(pending));
                let output = peer.cli(
                    format,
                    quiet,
                    &[
                        "deployments",
                        "wait",
                        "7",
                        "--target",
                        "observed",
                        "--timeout-ms",
                        "50",
                    ],
                );
                if quiet {
                    assert_eq!(output.status.code(), Some(exit));
                    assert_eq!(output.stdout, b"7\n");
                } else {
                    let result = document(&output, format, exit);
                    assert_eq!(result["target_revision"], 7);
                    assert_eq!(result["converged"], exit == 0);
                }
            }
        }
    }
}

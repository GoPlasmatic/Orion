//! Human build dates use UTC; structured health output retains the wire value.
#![expect(clippy::unwrap_used, clippy::panic)]

#[path = "common/http.rs"]
mod http;

use serde_json::{Value, json};

#[test]
fn health_formats_build_seconds_only_in_human_output() {
    let peer = http::Server::new(|method, path, _| {
        assert_eq!((method, path), ("GET", "/health"));
        (
            200,
            json!({"status": "ok", "build_timestamp": "1793059200"}),
        )
    });
    let human = peer.cli("table", false, &["health"]);
    assert!(human.status.success());
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("Built:        2026-10-27 00:00:00 UTC")
    );
    for format in ["json", "yaml"] {
        let output = peer.cli(format, false, &["health"]);
        assert!(output.status.success());
        let result: Value = if format == "json" {
            serde_json::from_slice(&output.stdout).unwrap()
        } else {
            serde_saphyr::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap()
        };
        assert_eq!(result["build_timestamp"], "1793059200");
    }
    assert_eq!(peer.cli("table", true, &["health"]).stdout, b"ok\n");
}

#[test]
fn health_handles_the_epoch_and_unrecognised_build_timestamps() {
    for (raw, displayed) in [
        ("0", Some("1970-01-01 00:00:00 UTC")),
        ("unknown", Some("unknown")),
        ("9223372036854775807", Some("9223372036854775807")),
        ("", None),
    ] {
        let peer = http::Server::new(move |_, _, _| {
            (200, json!({"status": "ok", "build_timestamp": raw}))
        });
        let output = peer.cli("table", false, &["health"]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        match displayed {
            Some(date) => assert!(text.contains(&format!("Built:        {date}")), "{text}"),
            None => assert!(!text.contains("Built:"), "{text}"),
        }
    }
}

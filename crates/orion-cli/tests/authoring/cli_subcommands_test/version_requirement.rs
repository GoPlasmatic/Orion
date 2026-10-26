//! #343: `package.requires.orion` — the binary checks itself first

use super::*;

#[test]
fn a_set_this_binary_is_too_old_for_stops_with_one_line() {
    let scratch = versioned_set(">=99.0.0");
    let dir = scratch.path().to_str().unwrap().to_string();
    let wf = format!("{dir}/wf.json");
    let input = format!("{dir}/in.json");
    let out = format!("{dir}/out.json");
    for args in [
        vec!["lint", dir.as_str()],
        vec!["clippy", dir.as_str()],
        vec!["compile", dir.as_str(), "-o", out.as_str()],
        vec!["lint", wf.as_str(), "--definitions", dir.as_str()],
        vec![
            "dry-run",
            "-w",
            wf.as_str(),
            "-i",
            input.as_str(),
            "--definitions",
            dir.as_str(),
        ],
        vec!["test", dir.as_str(), "--definitions", dir.as_str()],
    ] {
        let (ok, stdout, stderr) = run_bin(&args);
        assert!(!ok, "{args:?}: {stdout}{stderr}");
        assert!(
            stderr.contains("requires Orion >=99.0.0; this is orion-server"),
            "{args:?}: {stderr}"
        );
        assert!(
            !stderr.contains("no_such_function_x"),
            "{args:?}: the version line must come before any schema error: {stderr}"
        );
    }
    assert!(!std::path::Path::new(&out).exists());

    // `fmt` formats nothing with the wrong binary.
    let before = std::fs::read_to_string(&wf).unwrap();
    let (ok, _, stderr) = run_bin(&["fmt", dir.as_str()]);
    assert!(!ok);
    assert!(stderr.contains("requires Orion >=99.0.0") && stderr.contains("nothing was formatted"));
    assert_eq!(std::fs::read_to_string(&wf).unwrap(), before);
}

#[test]
fn a_satisfied_range_changes_nothing() {
    let scratch = versioned_set(">=1.0.0, <99");
    let dir = scratch.path();
    std::fs::write(
        dir.join("wf.json"),
        r#"{"workflow_id":"wf","name":"wf","tasks":[
             {"id":"t","name":"t","function":{"name":"map","input":{"mappings":[]}}}]}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(ok, "{report}");
    assert!(
        !report.contains("package.json is not a channel"),
        "a package document is part of the set: {report}"
    );

    // A malformed range is refused, naming how to write one.
    std::fs::write(
        dir.join("package.json"),
        r#"{"package": {"requires": {"orion": ">=1.8.x <"}}}"#,
    )
    .unwrap();
    let (ok, report) = lint_dir(dir, &[]);
    assert!(!ok);
    assert!(report.contains("is not a version range"), "{report}");
}

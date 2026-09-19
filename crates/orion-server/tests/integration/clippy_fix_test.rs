//! `clippy --fix` over `tests/fixtures/clippy-fix/<case>/{before,after}/`:
//! the binary rewrites a scratch copy of `before/`, which must then equal
//! `after/` byte for byte; a second `--fix` changes nothing; and the folded
//! workflow runs as the flat one did.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::{ScratchDir, orion_bin};

const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clippy-fix");

fn clippy(args: &[&str]) -> (i32, String) {
    let out = Command::new(orion_bin())
        .arg("clippy")
        .args(args)
        .output()
        .expect("run clippy");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
}

/// A scratch copy of a case's `before/`.
fn copy_before(case: &str) -> ScratchDir {
    let scratch = ScratchDir::new(&format!("clippy_fix_{case}"));
    for file in json_files(&Path::new(CASES).join(case).join("before")) {
        std::fs::copy(&file, scratch.path().join(file.file_name().expect("name"))).expect("copy");
    }
    scratch
}

fn assert_matches_after(case: &str, dir: &Path) {
    let after = Path::new(CASES).join(case).join("after");
    let expected = json_files(&after);
    assert_eq!(
        json_files(dir).len(),
        expected.len(),
        "{case}: the fix must not add or remove files"
    );
    for file in expected {
        let name = file.file_name().expect("name");
        let want = std::fs::read_to_string(&file).expect("after");
        let got = std::fs::read_to_string(dir.join(name)).expect("fixed");
        assert_eq!(got, want, "{case}: {} differs from after/", name.display());
    }
}

#[test]
fn every_fixture_case_fixes_to_its_after_tree_and_stays_fixed() {
    let mut cases: Vec<String> = std::fs::read_dir(CASES)
        .expect("fixtures")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    cases.sort();
    assert!(cases.len() >= 6, "{cases:?}");
    for case in &cases {
        let scratch = copy_before(case);
        let dir = scratch.path().to_str().expect("utf8");
        let (code, stderr) = clippy(&["--fix", dir]);
        assert_eq!(code, 0, "{case}: {stderr}");
        assert_matches_after(case, scratch.path());
        // Idempotent: nothing left to fold.
        let (code, stderr) = clippy(&["--fix", "--check", dir]);
        assert_eq!(
            code, 0,
            "{case}: a second --fix would change something: {stderr}"
        );
        assert!(
            !stderr.lines().any(|line| line.starts_with("fixed ")),
            "{case}: {stderr}"
        );
    }
}

#[test]
fn a_fixed_run_is_no_longer_reported_and_a_refused_one_still_is() {
    let simple = copy_before("simple");
    let dir = simple.path().to_str().expect("utf8");
    let (code, _) = clippy(&["--deny-warnings", dir]);
    assert_eq!(code, 1, "the run is a warning before the fix");
    let (code, stderr) = clippy(&["--fix", "--deny-warnings", dir]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("folded `claim`, `read` into group `when_claim`"),
        "{stderr}"
    );

    let fragment = copy_before("fragment_steps");
    let dir = fragment.path().to_str().expect("utf8");
    let (code, stderr) = clippy(&["--fix", "--deny-warnings", dir]);
    assert_eq!(code, 1, "a refused fix leaves its warning: {stderr}");
    assert!(
        stderr.contains("not fixed — steps `u.claim`, `u.read` are not written in this file"),
        "{stderr}"
    );
}

#[test]
fn check_prints_the_diff_and_writes_nothing() {
    let simple = copy_before("simple");
    let before = std::fs::read_to_string(simple.path().join("w.json")).expect("before");
    let out = Command::new(orion_bin())
        .args(["clippy", "--fix", "--check"])
        .arg(simple.path())
        .output()
        .expect("run clippy");
    assert_eq!(out.status.code(), Some(1), "something would change");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("+      \"id\": \"when_claim\","),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(simple.path().join("w.json")).expect("after"),
        before
    );
}

/// The fold is sound, not only well-formed: the flat and the folded
/// workflow answer every input the same.
#[test]
fn the_folded_workflow_runs_as_the_flat_one_did() {
    let case = Path::new(CASES).join("simple");
    let scratch = ScratchDir::new("clippy_fix_run");
    for (kind, expect) in [
        (
            "claim",
            serde_json::json!({"data.claim": 1, "data.read": 1, "data.done": null}),
        ),
        (
            "other",
            serde_json::json!({"data.claim": null, "data.read": null, "data.done": 1}),
        ),
    ] {
        for side in ["before", "after"] {
            let path = scratch.path().join(format!("{side}-{kind}.case.json"));
            std::fs::write(
                &path,
                serde_json::json!({
                    "name": format!("{side} {kind}"),
                    "workflow": case.join(side).join("w.json"),
                    "input": {"kind": kind},
                    "expect": expect,
                })
                .to_string(),
            )
            .expect("case");
            let out = Command::new(orion_bin())
                .arg("test")
                .arg(&path)
                .output()
                .expect("run test");
            assert!(
                out.status.success(),
                "{side} {kind}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// The example packages trip no rule, so `--fix` must not touch them.
#[test]
fn fix_is_a_no_op_over_the_examples() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/packages");
    for entry in std::fs::read_dir(&examples)
        .expect("examples")
        .filter_map(Result::ok)
    {
        if !entry.path().is_dir() {
            continue;
        }
        let out = Command::new(orion_bin())
            .args(["clippy", "--fix", "--check"])
            .arg(entry.path())
            .output()
            .expect("run clippy");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("+++ "),
            "{} would change",
            entry.path().display()
        );
    }
}

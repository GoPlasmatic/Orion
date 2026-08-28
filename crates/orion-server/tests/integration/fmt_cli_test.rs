//! `orion-server fmt` as a process: exit codes, what reaches which stream,
//! and what happens to the files.
//!
//! Against the compiled binary, like `cli_subcommands_test`, because the
//! contract under test is the one a pre-commit hook or a CI step sees.

use std::process::{Command, Output};

use crate::common::{ScratchDir, orion_bin};

const MINIFIED: &str = r#"{"tasks":[{"function":{"name":"log","input":{"message":"x"}},"name":"T","id":"t"}],"name":"w"}"#;
const FORMATTED: &str = "{\n  \"name\": \"w\",\n  \"tasks\": [\n    {\n      \"id\": \"t\",\n      \"name\": \"T\",\n      \"function\": { \"name\": \"log\", \"input\": { \"message\": \"x\" } }\n    }\n  ]\n}\n";

fn fmt(args: &[&str]) -> Output {
    Command::new(orion_bin())
        .arg("fmt")
        .args(args)
        .output()
        .expect("invoke orion-server fmt")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_file_is_rewritten_in_place_and_the_run_exits_zero() {
    let dir = ScratchDir::new("fmt_write");
    let file = dir.path().join("wf.json");
    std::fs::write(&file, MINIFIED).unwrap();
    let out = fmt(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), FORMATTED);
    assert!(text(&out.stdout).contains("1 file(s) reformatted, 0 unchanged"));
    assert!(
        !text(&out.stderr).contains("no config file"),
        "fmt must not print the config note: {}",
        text(&out.stderr)
    );
    // No temp file left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn check_prints_a_diff_and_exits_one_without_writing() {
    let dir = ScratchDir::new("fmt_check");
    let file = dir.path().join("wf.json");
    std::fs::write(&file, MINIFIED).unwrap();
    let out = fmt(&["--check", dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        MINIFIED,
        "--check wrote"
    );
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("--- a/") && stderr.contains("+++ b/"),
        "{stderr}"
    );
    assert!(stderr.contains("+  \"name\": \"w\","), "{stderr}");
    assert!(text(&out.stdout).contains("1 file(s) would be reformatted"));
}

#[test]
fn a_clean_tree_exits_zero_with_nothing_on_stderr() {
    let dir = ScratchDir::new("fmt_clean");
    std::fs::write(dir.path().join("wf.json"), FORMATTED).unwrap();
    let out = fmt(&["--check", dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stderr), "");
    assert!(text(&out.stdout).contains("0 file(s) would be reformatted, 1 unchanged"));
}

#[test]
fn an_unparseable_file_is_reported_and_left_alone_while_its_siblings_are_formatted() {
    let dir = ScratchDir::new("fmt_broken");
    let bad = dir.path().join("bad.json");
    let good = dir.path().join("good.json");
    std::fs::write(&bad, "{\"a\": 1,}").unwrap();
    std::fs::write(&good, MINIFIED).unwrap();
    let out = fmt(&[dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{\"a\": 1,}");
    assert_eq!(std::fs::read_to_string(&good).unwrap(), FORMATTED);
    let stderr = text(&out.stderr);
    assert!(stderr.contains("bad.json: line 1, column 9"), "{stderr}");
    assert!(text(&out.stdout).contains("1 error(s)"));
}

#[test]
fn a_duplicate_key_is_refused_by_name() {
    let dir = ScratchDir::new("fmt_dup");
    let file = dir.path().join("dup.json");
    std::fs::write(&file, "{\"name\": \"a\", \"name\": \"b\", \"tasks\": []}").unwrap();
    let out = fmt(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("duplicate key \"name\""),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_missing_path_is_an_error() {
    let out = fmt(&["/definitely/not/here.json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("not a file or directory"));
}

#[test]
fn stdin_formats_to_stdout_and_a_bad_document_leaves_stdout_empty() {
    use std::io::Write;
    use std::process::Stdio;

    let run = |input: &str| {
        let mut child = Command::new(orion_bin())
            .args(["fmt", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let ok = run(MINIFIED);
    assert_eq!(ok.status.code(), Some(0));
    assert_eq!(text(&ok.stdout), FORMATTED);

    let already = run(FORMATTED);
    assert_eq!(
        text(&already.stdout),
        FORMATTED,
        "unchanged input is still echoed"
    );

    let bad = run("nope");
    assert_eq!(bad.status.code(), Some(2));
    assert_eq!(
        text(&bad.stdout),
        "",
        "an editor must never receive an error as the buffer"
    );
    assert!(
        text(&bad.stderr).contains("<stdin>: line 1, column 2"),
        "{}",
        text(&bad.stderr)
    );
}

#[test]
fn check_and_stdin_conflict() {
    let out = fmt(&["--check", "--stdin"]);
    assert_eq!(out.status.code(), Some(2), "clap usage error");
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_is_not_followed() {
    let dir = ScratchDir::new("fmt_symlink_dir");
    std::fs::write(dir.path().join("wf.json"), MINIFIED).unwrap();
    // A link back to the directory itself: following it would never end.
    std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
    let out = fmt(&[dir.path().to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("1 file(s) reformatted"),
        "{}",
        text(&out.stdout)
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_file_is_formatted_through_the_link_and_stays_a_link() {
    let dir = ScratchDir::new("fmt_symlink_file");
    let target = dir.path().join("real.json");
    let link = dir.path().join("link.json");
    std::fs::write(&target, MINIFIED).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let out = fmt(&[link.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), FORMATTED);
}

#[cfg(unix)]
#[test]
fn file_permissions_survive_the_rewrite() {
    use std::os::unix::fs::PermissionsExt;
    let dir = ScratchDir::new("fmt_perms");
    let file = dir.path().join("wf.json");
    std::fs::write(&file, MINIFIED).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = fmt(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn an_unwritable_directory_reports_the_write_and_leaves_no_temp_file() {
    use std::os::unix::fs::PermissionsExt;
    // Root can write anywhere; the case is meaningless there.
    if unsafe { libc_geteuid() } == 0 {
        return;
    }
    let dir = ScratchDir::new("fmt_readonly");
    let file = dir.path().join("wf.json");
    std::fs::write(&file, MINIFIED).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = fmt(&[file.to_str().unwrap()]);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        MINIFIED,
        "original untouched"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "temp file left behind"
    );
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

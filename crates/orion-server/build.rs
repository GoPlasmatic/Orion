use std::process::Command;
use std::time::SystemTime;

fn main() {
    // Capture git commit hash at build time. An already-set GIT_HASH env
    // var wins: Docker builds have no .git directory (.dockerignore), so
    // CI threads the SHA through as a build-arg instead.
    let git_hash = std::env::var("GIT_HASH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={git_hash}");
    println!("cargo:rerun-if-env-changed=GIT_HASH");

    // Capture build timestamp as Unix seconds (no external crate needed)
    let build_timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={build_timestamp}");

    // Only re-run if git HEAD changes or build.rs itself changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    // P20: `.git/HEAD` only changes on branch switch — a normal commit moves
    // the *branch ref*, so GIT_HASH went stale across local commits (and,
    // because emitting any rerun-if-changed disables cargo's default rule,
    // stayed stale until something unrelated changed). Track the ref HEAD
    // points at, plus packed-refs for when it is packed. Existence-guarded:
    // cargo re-runs every build for a listed file that does not exist.
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(reference) = head.strip_prefix("ref: ")
    {
        let ref_file = format!(".git/{}", reference.trim());
        if std::path::Path::new(&ref_file).exists() {
            println!("cargo:rerun-if-changed={ref_file}");
        }
    }
    if std::path::Path::new(".git/packed-refs").exists() {
        println!("cargo:rerun-if-changed=.git/packed-refs");
    }
    println!("cargo:rerun-if-changed=build.rs");
    // Recompile when migrations change: sqlx::migrate! embeds the directory
    // contents at macro expansion, so a NEW migration file is silently
    // invisible until the crate rebuilds.
    println!("cargo:rerun-if-changed=migrations");
}

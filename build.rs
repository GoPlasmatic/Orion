use std::process::Command;
use std::time::SystemTime;

fn main() {
    // Capture git commit hash at build time
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={git_hash}");

    // Capture build timestamp as Unix seconds (no external crate needed)
    let build_timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={build_timestamp}");

    // Only re-run if git HEAD changes or build.rs itself changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
}

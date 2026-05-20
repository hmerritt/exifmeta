use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=EXIFMETA_VERSION_PRERELEASE");
    println!("cargo:rerun-if-env-changed=EXIFMETA_VERSION_METADATA");

    emit_env("EXIFMETA_BUILD_DATE", &build_date());
    emit_env(
        "EXIFMETA_GIT_COMMIT",
        &git(["rev-parse", "--short=12", "HEAD"]),
    );
    emit_env(
        "EXIFMETA_GIT_BRANCH",
        &git(["rev-parse", "--abbrev-ref", "HEAD"]),
    );
    emit_env(
        "EXIFMETA_GIT_DIRTY",
        if is_git_dirty() { "true" } else { "" },
    );
    emit_env(
        "EXIFMETA_VERSION_PRERELEASE",
        &std::env::var("EXIFMETA_VERSION_PRERELEASE").unwrap_or_default(),
    );
    emit_env(
        "EXIFMETA_VERSION_METADATA",
        &std::env::var("EXIFMETA_VERSION_METADATA").unwrap_or_default(),
    );
}

fn emit_env(key: &str, value: &str) {
    println!("cargo:rustc-env={key}={value}");
}

fn build_date() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default()
}

fn git<const N: usize>(args: [&str; N]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn is_git_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false)
}

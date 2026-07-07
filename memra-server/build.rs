use std::process::Command;

fn main() {
    let build_commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let build_dirty = Command::new("git")
        .args([
            "diff-index",
            "--quiet",
            "HEAD",
            "--",
            ".",
            "../memra-core",
            "../Cargo.toml",
            "../Cargo.lock",
        ])
        .status()
        .ok()
        .map(|status| !status.success())
        .unwrap_or(true);

    println!("cargo:rustc-env=MA_BUILD_GIT_SHA={build_commit}");
    println!("cargo:rustc-env=MA_BUILD_GIT_DIRTY={build_dirty}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads/main");
    println!("cargo:rerun-if-changed=../memra-server/src/cli/phase4.rs");
}

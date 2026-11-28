//! Build script (FLEETD-SELFUPDATE-001): same git-hash stamp as the root
//! crate — each package needs its own build script to see the env var.
use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FLEETD_BUILD_HASH={hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}

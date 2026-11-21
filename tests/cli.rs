//! The built binary, run as people run it.

use std::process::Command;

#[test]
fn up_says_docker_is_unreachable_instead_of_building() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_logos-devnet"))
        .args(["--home"])
        .arg(home.path())
        .args(["up", "--port", "39871"])
        // No daemon answers here, whether or not Docker is installed.
        .env("DOCKER_HOST", "unix:///nonexistent/docker.sock")
        .env_remove("DOCKER_CONTEXT")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("no Docker daemon answers"), "{stderr}");
    assert!(!stderr.contains("Building"), "{stderr}");
}

#[test]
fn the_version_names_the_lez_release_it_runs() {
    let out = Command::new(env!("CARGO_BIN_EXE_logos-devnet"))
        .arg("--version")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let home =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/home.rs")).unwrap();
    let lez = home
        .lines()
        .find_map(|l| l.strip_prefix("pub const DEFAULT_LEZ: &str = \""))
        .and_then(|l| l.strip_suffix("\";"))
        .unwrap();
    assert_eq!(
        stdout.trim(),
        format!("logos-devnet {} (LEZ {lez})", env!("CARGO_PKG_VERSION"))
    );
}

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn openyak_package_release_stages_runnable_artifact() {
    let output_root = unique_temp_dir("openyak-package-release-smoke");
    fs::create_dir_all(&output_root).expect("output root should exist");

    let package_output = Command::new(common::openyak_binary())
        .args(["package-release", "--output-dir"])
        .arg(&output_root)
        .output()
        .expect("package-release should run");
    assert!(
        package_output.status.success(),
        "package-release should succeed: {}",
        String::from_utf8_lossy(&package_output.stderr)
    );
    let stdout = String::from_utf8(package_output.stdout).expect("stdout should be utf8");
    let artifact_dir = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Release artifact staged at "))
        .map(PathBuf::from)
        .expect("artifact path should be reported");
    let packaged_binary = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Packaged binary: "))
        .map(PathBuf::from)
        .expect("packaged binary path should be reported");

    assert!(artifact_dir.is_dir());
    assert!(packaged_binary.is_file());
    assert!(artifact_dir.join("INSTALL.txt").is_file());
    assert!(artifact_dir.join("release-metadata.json").is_file());

    let packaged_help = Command::new(&packaged_binary)
        .arg("--help")
        .output()
        .expect("packaged binary should run");
    assert!(
        packaged_help.status.success(),
        "packaged binary should succeed: {}",
        String::from_utf8_lossy(&packaged_help.stderr)
    );
    let help = String::from_utf8(packaged_help.stdout).expect("help output should be utf8");
    assert!(help.contains("openyak CLI"));

    fs::remove_dir_all(output_root).expect("temp dir cleanup should succeed");
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
}

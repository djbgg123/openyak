use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn openyak_onboard_help_is_available_and_root_help_lists_it() {
    let root_help = Command::new(common::openyak_binary())
        .arg("--help")
        .output()
        .expect("root help should run");
    assert!(root_help.status.success(), "root help should succeed");
    let root_stdout = String::from_utf8(root_help.stdout).expect("root help should be utf8");
    assert!(root_stdout.contains("openyak onboard"));

    let onboard_help = Command::new(common::openyak_binary())
        .args(["onboard", "--help"])
        .output()
        .expect("onboard help should run");
    assert!(onboard_help.status.success(), "onboard help should succeed");
    let onboard_stdout =
        String::from_utf8(onboard_help.stdout).expect("onboard help should be utf8");
    assert!(onboard_stdout.contains("Usage: openyak onboard"));
    assert!(onboard_stdout.contains("interactive"));
    assert!(onboard_stdout.contains("local daemon"), "{onboard_stdout}");
    assert!(
        onboard_stdout.contains("openyak doctor"),
        "{onboard_stdout}"
    );
}

#[test]
fn openyak_onboard_fails_safely_without_a_tty() {
    let root = unique_temp_dir("openyak-onboard-smoke");
    let workspace = root.join("workspace");
    let config_home = root.join("openyak-home");
    fs::create_dir_all(&workspace).expect("workspace should exist");

    let output = Command::new(common::openyak_binary())
        .arg("onboard")
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("onboard should run");

    assert!(
        !output.status.success(),
        "onboard should fail without a tty"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("interactive-only") || stdout.contains("interactive terminal"));
    assert!(stdout.contains("openyak init"));
    assert!(stdout.contains("openyak doctor"));
    assert!(stdout.contains("Local daemon"), "{stdout}");
    assert!(
        stdout.contains("openyak server install --bind 127.0.0.1:0"),
        "{stdout}"
    );
    assert!(stdout.contains("openyak server start --detach"), "{stdout}");
    assert!(stdout.contains("openyak server status"), "{stdout}");
    assert!(
        !config_home.join("settings.json").exists(),
        "non-interactive onboarding must not create user settings"
    );
    assert!(
        !workspace.join(".openyak.json").exists(),
        "non-interactive onboarding must not initialize the repo"
    );

    fs::remove_dir_all(root).expect("cleanup temp dir should succeed");
}

#[test]
fn openyak_onboard_mentions_staged_install_bundle_guidance_without_a_tty() {
    let root = unique_temp_dir("openyak-onboard-install-bundle");
    let workspace = root.join("workspace");
    let config_home = root.join("openyak-home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");

    let install_output = Command::new(common::openyak_binary())
        .args(["server", "install", "--bind", "127.0.0.1:0"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("server install should run");
    assert!(
        install_output.status.success(),
        "server install should succeed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );

    let output = Command::new(common::openyak_binary())
        .arg("onboard")
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("onboard should run");

    assert!(
        !output.status.success(),
        "onboard should fail without a tty"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("bundle-only") && stdout.contains("staged at"),
        "{stdout}"
    );
    assert!(stdout.contains("README.txt"), "{stdout}");

    fs::remove_dir_all(root).expect("cleanup temp dir should succeed");
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
}

#[allow(dead_code)]
fn joined_path(bin_dir: &Path) -> String {
    std::env::join_paths([bin_dir])
        .expect("path should join")
        .to_string_lossy()
        .to_string()
}

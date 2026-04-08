use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn openyak_doctor_reports_healthy_environment() {
    let root = unique_temp_dir("openyak-doctor-smoke-healthy");
    let workspace = root.join("workspace");
    let config_home = root.join("openyak-home");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&bin_dir).expect("bin dir should exist");
    write_fake_gh(&bin_dir, true);

    let output = Command::new(common::openyak_binary())
        .arg("doctor")
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .env("ANTHROPIC_API_KEY", "doctor-test-key")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env("PATH", joined_path(&bin_dir))
        .output()
        .expect("doctor should run");

    assert!(
        output.status.success(),
        "doctor should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("doctor stdout should be utf8");
    assert!(stdout.contains("openyak Doctor"));
    assert!(stdout.contains("Summary"));

    fs::remove_dir_all(root).expect("temp dir cleanup should succeed");
}

#[test]
fn openyak_doctor_fails_on_incomplete_oauth_config() {
    let root = unique_temp_dir("openyak-doctor-smoke-bad-oauth");
    let workspace = root.join("workspace");
    let config_home = root.join("openyak-home");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&bin_dir).expect("bin dir should exist");
    fs::write(
        config_home.join("settings.json"),
        "{\n  \"oauth\": {\n    \"callbackPort\": 4557\n  }\n}\n",
    )
    .expect("settings should write");
    write_fake_gh(&bin_dir, true);

    let output = Command::new(common::openyak_binary())
        .arg("doctor")
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .env("ANTHROPIC_API_KEY", "doctor-test-key")
        .env("PATH", joined_path(&bin_dir))
        .output()
        .expect("doctor should run");

    assert!(
        !output.status.success(),
        "doctor should fail for incomplete oauth config"
    );
    let stdout = String::from_utf8(output.stdout).expect("doctor stdout should be utf8");
    assert!(stdout.contains("settings.oauth is incomplete"));

    fs::remove_dir_all(root).expect("temp dir cleanup should succeed");
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
}

fn joined_path(bin_dir: &Path) -> String {
    std::env::join_paths([bin_dir])
        .expect("path should join")
        .to_string_lossy()
        .to_string()
}

#[test]
fn openyak_doctor_warns_when_github_cli_is_not_logged_in() {
    let root = unique_temp_dir("openyak-doctor-smoke-gh-auth");
    let workspace = root.join("workspace");
    let config_home = root.join("openyak-home");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&bin_dir).expect("bin dir should exist");
    write_fake_gh(&bin_dir, false);

    let output = Command::new(common::openyak_binary())
        .arg("doctor")
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .env("ANTHROPIC_API_KEY", "doctor-test-key")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env("PATH", joined_path(&bin_dir))
        .output()
        .expect("doctor should run");

    assert!(
        output.status.success(),
        "doctor should surface warnings without failing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("doctor stdout should be utf8");
    assert!(stdout.contains("github cli"), "{stdout}");
    assert!(stdout.contains("gh auth status"), "{stdout}");
    assert!(stdout.contains("gh auth login --web"), "{stdout}");

    fs::remove_dir_all(root).expect("temp dir cleanup should succeed");
}

fn write_fake_gh(dir: &Path, auth_ready: bool) -> PathBuf {
    let path = if cfg!(windows) {
        dir.join("gh.cmd")
    } else {
        dir.join("gh")
    };
    let script = if cfg!(windows) {
        if auth_ready {
            "@echo off\r\nif \"%~1 %~2\"==\"auth status\" exit /b 0\r\nexit /b 0\r\n"
        } else {
            "@echo off\r\nif \"%~1 %~2\"==\"auth status\" (\r\necho gh: not logged in 1>&2\r\nexit /b 1\r\n)\r\nexit /b 0\r\n"
        }
    } else if auth_ready {
        "#!/bin/sh\nif [ \"$1 $2\" = \"auth status\" ]; then\n  exit 0\nfi\nexit 0\n"
    } else {
        "#!/bin/sh\nif [ \"$1 $2\" = \"auth status\" ]; then\n  echo 'gh: not logged in' >&2\n  exit 1\nfi\nexit 0\n"
    };
    fs::write(&path, script).expect("fake command should write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&path)
            .expect("fake command metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("fake command permissions should update");
    }
    path
}

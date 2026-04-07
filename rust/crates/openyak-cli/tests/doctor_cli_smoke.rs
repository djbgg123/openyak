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
    write_fake_command(&bin_dir, "gh");

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
    write_fake_command(&bin_dir, "gh");

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

fn write_fake_command(dir: &Path, name: &str) -> PathBuf {
    let path = if cfg!(windows) {
        dir.join(format!("{name}.cmd"))
    } else {
        dir.join(name)
    };
    fs::write(&path, "@echo off\r\n").expect("fake command should write");
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

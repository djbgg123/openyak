use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

mod common;

struct ChildGuard {
    child: Child,
    workspace: PathBuf,
    cleanup_workspace: bool,
}

struct DetachedWorkspaceGuard {
    workspace: PathBuf,
}

impl ChildGuard {
    fn spawn() -> Self {
        Self::spawn_in(unique_temp_dir("openyak-server-smoke"), true)
    }

    fn spawn_in(workspace: PathBuf, cleanup_workspace: bool) -> Self {
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let child = Command::new(common::openyak_binary())
            .args(["server", "--bind", "127.0.0.1:0"])
            .current_dir(&workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("openyak server should spawn");
        Self {
            child,
            workspace,
            cleanup_workspace,
        }
    }

    fn stdout_reader(&mut self) -> BufReader<std::process::ChildStdout> {
        BufReader::new(
            self.child
                .stdout
                .take()
                .expect("server stdout should be piped"),
        )
    }

    fn state_db_path(&self) -> PathBuf {
        self.workspace.join(".openyak").join("state.sqlite3")
    }

    fn server_info_path(&self) -> PathBuf {
        self.workspace.join(".openyak").join("thread-server.json")
    }

    fn operator_token(&self) -> String {
        discovery_operator_token(&self.server_info_path())
    }

    fn workspace(&self) -> &PathBuf {
        &self.workspace
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn advertised_address(&mut self) -> String {
        let mut stdout = self.stdout_reader();
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .expect("server should print its startup line");
        assert!(
            line.starts_with("Local thread server listening on http://"),
            "unexpected startup line: {line:?}"
        );
        line.trim()
            .strip_prefix("Local thread server listening on http://")
            .expect("startup line should include http address")
            .to_string()
    }

    fn wait_for_exit(&mut self) {
        for _ in 0..80 {
            match self.child.try_wait().expect("try_wait should succeed") {
                Some(_status) => return,
                None => thread::sleep(Duration::from_millis(25)),
            }
        }
        panic!("server process did not exit in time");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.cleanup_workspace {
            let _ = std::fs::remove_dir_all(&self.workspace);
        }
    }
}

impl DetachedWorkspaceGuard {
    fn new(prefix: &str) -> Self {
        let workspace = unique_temp_dir(prefix);
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        Self { workspace }
    }

    fn workspace(&self) -> &PathBuf {
        &self.workspace
    }
}

impl Drop for DetachedWorkspaceGuard {
    fn drop(&mut self) {
        if self.workspace.exists() {
            let _ = Command::new(common::openyak_binary())
                .args(["server", "stop"])
                .current_dir(&self.workspace)
                .output();
            let _ = std::fs::remove_dir_all(&self.workspace);
        }
    }
}

#[test]
fn openyak_server_surfaces_thread_routes() {
    let mut child = ChildGuard::spawn();
    let address = child.advertised_address();
    let operator_token = child.operator_token();
    let auth = auth_header(&operator_token);
    let server_info = std::fs::read_to_string(child.server_info_path())
        .expect("thread server info file should exist");
    let server_info_json: Value =
        serde_json::from_str(&server_info).expect("thread server info should be json");
    assert_eq!(
        server_info_json["baseUrl"],
        format!("http://{address}"),
        "thread server info should match the advertised address"
    );
    assert_eq!(server_info_json["truthLayer"], "daemon_local_v1");
    assert_eq!(
        server_info_json["operatorPlane"],
        "local_loopback_operator_v1"
    );
    assert_eq!(server_info_json["persistence"], "workspace_sqlite_v1");
    assert_eq!(server_info_json["attachApi"], "/v1/threads");
    assert!(server_info_json["operatorToken"].as_str().is_some());

    let create = http_request_with_retry(
        &address,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert!(
        create.starts_with("HTTP/1.1 201"),
        "create response should be 201, got: {create}"
    );
    let created: Value =
        serde_json::from_str(response_body(&create)).expect("create body should be json");
    let thread_id = created["thread_id"]
        .as_str()
        .expect("thread_id should be present")
        .to_string();
    assert_eq!(created["contract"]["truth_layer"], "daemon_local_v1");
    assert_eq!(
        created["contract"]["operator_plane"],
        "local_loopback_operator_v1"
    );
    assert_eq!(created["contract"]["persistence"], "workspace_sqlite_v1");
    assert_eq!(created["contract"]["attach_api"], "/v1/threads");
    assert!(
        child.state_db_path().exists(),
        "durable state db should exist at {}",
        child.state_db_path().display()
    );

    let list = http_request_with_retry(
        &address,
        &format!("GET /v1/threads HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"),
    );
    assert!(
        list.starts_with("HTTP/1.1 200"),
        "list response should be 200, got: {list}"
    );
    let listed: Value =
        serde_json::from_str(response_body(&list)).expect("list body should be json");
    let threads = listed["threads"]
        .as_array()
        .expect("threads should be an array");
    assert!(
        threads
            .iter()
            .any(|entry| entry["thread_id"].as_str() == Some(thread_id.as_str())),
        "created thread should appear in thread list: {listed}"
    );
    assert_eq!(
        listed["threads"][0]["contract"]["truth_layer"],
        "daemon_local_v1"
    );
    assert_eq!(
        listed["threads"][0]["contract"]["operator_plane"],
        "local_loopback_operator_v1"
    );
    assert_eq!(
        listed["threads"][0]["contract"]["persistence"],
        "workspace_sqlite_v1"
    );
    assert_eq!(
        listed["threads"][0]["contract"]["attach_api"],
        "/v1/threads"
    );
}

#[test]
fn openyak_server_persists_threads_across_restart_and_keeps_legacy_session_route() {
    let workspace = unique_temp_dir("openyak-server-restart");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let mut child = ChildGuard::spawn_in(workspace.clone(), false);
    let address = child.advertised_address();
    let operator_token = child.operator_token();
    let auth = auth_header(&operator_token);

    let create = http_request_with_retry(
        &address,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert!(
        create.starts_with("HTTP/1.1 201"),
        "create response should be 201, got: {create}"
    );
    let created: Value =
        serde_json::from_str(response_body(&create)).expect("create body should be json");
    let thread_id = created["thread_id"]
        .as_str()
        .expect("thread_id should be present")
        .to_string();

    let legacy_before_restart = http_request_with_retry(
        &address,
        &format!(
            "GET /sessions/{thread_id} HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"
        ),
    );
    assert!(
        legacy_before_restart.starts_with("HTTP/1.1 200"),
        "legacy get should be 200 before restart, got: {legacy_before_restart}"
    );
    let legacy_value: Value = serde_json::from_str(response_body(&legacy_before_restart))
        .expect("legacy session body should be json");
    assert_eq!(legacy_value["id"], thread_id);

    drop(child);

    let mut restarted = ChildGuard::spawn_in(workspace.clone(), false);
    let restarted_address = restarted.advertised_address();
    let restarted_operator_token = restarted.operator_token();
    let restarted_auth = auth_header(&restarted_operator_token);
    assert_eq!(
        workspace,
        *restarted.workspace(),
        "restarted server should reuse the same workspace"
    );

    let list = http_request_with_retry(
        &restarted_address,
        &format!(
            "GET /v1/threads HTTP/1.1\r\nHost: {restarted_address}\r\n{restarted_auth}Connection: close\r\n\r\n"
        ),
    );
    assert!(
        list.starts_with("HTTP/1.1 200"),
        "list response should be 200 after restart, got: {list}"
    );
    let listed: Value =
        serde_json::from_str(response_body(&list)).expect("list body should be json");
    let threads = listed["threads"]
        .as_array()
        .expect("threads should be an array");
    let recovered = threads
        .iter()
        .find(|entry| entry["thread_id"].as_str() == Some(thread_id.as_str()))
        .expect("restarted thread list should include the original thread");
    assert_eq!(recovered["state"]["status"], "idle");

    let thread_snapshot = http_request_with_retry(
        &restarted_address,
        &format!(
            "GET /v1/threads/{thread_id} HTTP/1.1\r\nHost: {restarted_address}\r\n{restarted_auth}Connection: close\r\n\r\n"
        ),
    );
    assert!(
        thread_snapshot.starts_with("HTTP/1.1 200"),
        "get thread should be 200 after restart, got: {thread_snapshot}"
    );
    let thread_value: Value = serde_json::from_str(response_body(&thread_snapshot))
        .expect("thread snapshot body should be json");
    assert_eq!(thread_value["thread_id"], thread_id);
    assert_eq!(thread_value["state"]["status"], "idle");

    let legacy_after_restart = http_request_with_retry(
        &restarted_address,
        &format!(
            "GET /sessions/{thread_id} HTTP/1.1\r\nHost: {restarted_address}\r\n{restarted_auth}Connection: close\r\n\r\n"
        ),
    );
    assert!(
        legacy_after_restart.starts_with("HTTP/1.1 200"),
        "legacy get should be 200 after restart, got: {legacy_after_restart}"
    );
    let legacy_recovered: Value = serde_json::from_str(response_body(&legacy_after_restart))
        .expect("legacy restart body should be json");
    assert_eq!(legacy_recovered["id"], thread_id);
    assert_eq!(
        legacy_recovered["session"]["messages"],
        serde_json::json!([])
    );

    drop(restarted);
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_rejects_non_loopback_bind() {
    let workspace = unique_temp_dir("openyak-server-non-loopback");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let output = Command::new(common::openyak_binary())
        .args(["server", "--bind", "0.0.0.0:0"])
        .current_dir(&workspace)
        .output()
        .expect("openyak server should run");

    assert!(!output.status.success(), "non-loopback bind should fail");
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf8");
    assert!(
        stderr.contains("must resolve to a loopback address"),
        "{stderr}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn openyak_server_status_reports_running_operator_surface() {
    let workspace = unique_temp_dir("openyak-server-status-running");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let mut child = ChildGuard::spawn_in(workspace.clone(), false);
    let address = child.advertised_address();

    let text_output = Command::new(common::openyak_binary())
        .args(["server", "status"])
        .current_dir(child.workspace())
        .output()
        .expect("server status should run");
    assert!(text_output.status.success(), "server status should succeed");
    let stdout = String::from_utf8(text_output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Status           running"), "{stdout}");
    assert!(
        stdout.contains(&format!("Base URL         http://{address}")),
        "{stdout}"
    );
    assert!(
        stdout.contains("Truth layer      daemon_local_v1"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Operator plane   local_loopback_operator_v1"),
        "{stdout}"
    );
    assert!(stdout.contains("Attach API       /v1/threads"), "{stdout}");

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(child.workspace())
        .output()
        .expect("json server status should run");
    assert!(
        json_output.status.success(),
        "json server status should succeed"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("json server status should parse");
    assert_eq!(report["status"], "running");
    assert_eq!(report["base_url"], format!("http://{address}"));
    assert_eq!(report["reachable"], true);
    assert_eq!(report["state_db_present"], true);
    assert_eq!(report["contract"]["truth_layer"], "daemon_local_v1");
    assert_eq!(
        report["contract"]["operator_plane"],
        "local_loopback_operator_v1"
    );
    assert_eq!(report["contract"]["persistence"], "workspace_sqlite_v1");
    assert_eq!(report["contract"]["attach_api"], "/v1/threads");

    drop(child);
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_status_reports_not_running_workspace_guidance() {
    let workspace = unique_temp_dir("openyak-server-status-missing");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let output = Command::new(common::openyak_binary())
        .args(["server", "status"])
        .current_dir(&workspace)
        .output()
        .expect("server status should run");
    assert!(output.status.success(), "server status should succeed");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Status           not_running"), "{stdout}");
    assert!(
        stdout.contains("openyak server start --detach --bind 127.0.0.1:0"),
        "{stdout}"
    );

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(&workspace)
        .output()
        .expect("json server status should run");
    assert!(
        json_output.status.success(),
        "json server status should succeed"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("json server status should parse");
    assert_eq!(report["status"], "not_running");
    assert_eq!(report["reachable"], false);
    assert_eq!(report["state_db_present"], false);

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_start_detached_launches_local_server() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-start-detached");

    let output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server start --detach should run");
    assert!(
        output.status.success(),
        "server start --detach should succeed"
    );
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("json server start should parse");
    assert_eq!(report["status"], "started");
    assert_eq!(report["requested_bind"], "127.0.0.1:0");
    assert_eq!(report["stale_registration_cleared"], false);
    let base_url = report["base_url"]
        .as_str()
        .expect("started report should include base_url");
    let pid = report["pid"]
        .as_u64()
        .expect("started report should include pid");
    assert_eq!(report["contract"]["truth_layer"], "daemon_local_v1");
    assert_eq!(
        report["contract"]["operator_plane"],
        "local_loopback_operator_v1"
    );
    assert_eq!(report["contract"]["persistence"], "workspace_sqlite_v1");
    assert_eq!(report["contract"]["attach_api"], "/v1/threads");
    let address = base_url
        .strip_prefix("http://")
        .expect("base_url should be http");
    let operator_token = discovery_operator_token(
        &workspace
            .workspace()
            .join(".openyak")
            .join("thread-server.json"),
    );
    let auth = auth_header(&operator_token);

    let identity = http_request_with_retry(
        address,
        &format!(
            "GET /v1/operator/identity HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\n\r\n"
        ),
    );
    assert!(
        identity.starts_with("HTTP/1.1 200"),
        "identity response should be 200, got: {identity}"
    );
    let identity_value: Value =
        serde_json::from_str(response_body(&identity)).expect("identity body should be json");
    assert_eq!(identity_value["pid"], pid);
    assert_eq!(identity_value["attach_api"], "/v1/threads");

    let status_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server status should run");
    assert!(
        status_output.status.success(),
        "server status should succeed"
    );
    let status_report: Value =
        serde_json::from_slice(&status_output.stdout).expect("json status should parse");
    assert_eq!(status_report["status"], "running");
    assert_eq!(status_report["pid"], pid);
    assert_eq!(status_report["base_url"], base_url);
}

#[test]
fn openyak_server_start_detached_is_idempotent_while_running() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-start-detached-idempotent");

    let first_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("first detached start should run");
    assert!(
        first_output.status.success(),
        "first detached start should succeed"
    );
    let first_report: Value =
        serde_json::from_slice(&first_output.stdout).expect("first start json should parse");

    let second_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("second detached start should run");
    assert!(
        second_output.status.success(),
        "second detached start should succeed idempotently"
    );
    let second_report: Value =
        serde_json::from_slice(&second_output.stdout).expect("second start json should parse");
    assert_eq!(second_report["status"], "already_running");
    assert_eq!(second_report["requested_bind"], "127.0.0.1:0");
    assert_eq!(second_report["stale_registration_cleared"], false);
    assert_eq!(second_report["pid"], first_report["pid"]);
    assert_eq!(second_report["base_url"], first_report["base_url"]);
}

#[test]
fn openyak_server_start_detached_fails_when_requested_bind_conflicts_with_running_server() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-start-detached-bind-conflict");

    let first_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("first detached start should run");
    assert!(
        first_output.status.success(),
        "first detached start should succeed"
    );
    let first_report: Value =
        serde_json::from_slice(&first_output.stdout).expect("first start json should parse");

    let conflicting_output = Command::new(common::openyak_binary())
        .args([
            "--output-format",
            "json",
            "server",
            "start",
            "--detach",
            "--bind",
            "127.0.0.1:4105",
        ])
        .current_dir(workspace.workspace())
        .output()
        .expect("conflicting detached start should run");
    assert!(
        !conflicting_output.status.success(),
        "conflicting detached start should fail"
    );
    let conflicting_report: Value = serde_json::from_slice(&conflicting_output.stdout)
        .expect("conflicting start json should parse");
    assert_eq!(conflicting_report["status"], "bind_conflict");
    assert_eq!(conflicting_report["base_url"], first_report["base_url"]);
    assert!(
        conflicting_report["problem"]
            .as_str()
            .is_some_and(|problem| problem.contains("does not match")),
        "{conflicting_report}"
    );
}

#[test]
fn openyak_server_start_detached_replaces_stale_registration() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-start-detached-stale");
    let openyak_dir = workspace.workspace().join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    std::fs::write(
        openyak_dir.join("thread-server.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": "http://127.0.0.1:9",
            "pid": 4242_u32,
            "truthLayer": "daemon_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": "fixture-token"
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("detached start should run");
    assert!(
        output.status.success(),
        "detached start should succeed from stale registration"
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("start json should parse");
    assert_eq!(report["status"], "started");
    assert_eq!(report["stale_registration_cleared"], true);
    assert_ne!(report["base_url"], "http://127.0.0.1:9");
}

#[test]
fn openyak_server_start_detached_rejects_unsafe_registration() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-start-detached-unsafe");
    let openyak_dir = workspace.workspace().join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    let discovery_path = openyak_dir.join("thread-server.json");
    std::fs::write(
        &discovery_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": "http://127.0.0.1:4100",
            "pid": 4242_u32,
            "truthLayer": "process_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": "fixture-token"
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("detached start should run");
    assert!(
        !output.status.success(),
        "detached start should reject unsafe discovery"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("Status           invalid_registration"),
        "{stdout}"
    );
    assert!(
        discovery_path.exists(),
        "unsafe discovery should remain for inspection"
    );
}

#[test]
fn openyak_server_recover_reports_nothing_to_recover_without_persisted_truth() {
    let workspace = unique_temp_dir("openyak-server-recover-empty");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "recover"])
        .current_dir(&workspace)
        .output()
        .expect("server recover should run");
    assert!(output.status.success(), "server recover should succeed");
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("json server recover should parse");
    assert_eq!(report["status"], "nothing_to_recover");
    assert_eq!(report["recovery_kind"], "no_persisted_truth");
    assert_eq!(report["reachable"], false);
    assert_eq!(report["state_db_present"], false);
    assert!(
        report["recommended_actions"]
            .as_array()
            .expect("recommended actions should be present")
            .iter()
            .any(|value| value
                .as_str()
                .is_some_and(|line| line.contains("openyak server start --detach"))),
        "{report}"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_recover_reports_nothing_to_recover_for_empty_state_db() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-recover-empty-state-db");

    let start_output = Command::new(common::openyak_binary())
        .args(["server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server start --detach should run");
    assert!(
        start_output.status.success(),
        "detached start should succeed"
    );

    let stop_output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server stop should run");
    assert!(stop_output.status.success(), "server stop should succeed");

    let recover_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "recover"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server recover should run");
    assert!(
        recover_output.status.success(),
        "server recover should succeed"
    );
    let report: Value =
        serde_json::from_slice(&recover_output.stdout).expect("json server recover should parse");
    assert_eq!(report["status"], "nothing_to_recover");
    assert_eq!(report["recovery_kind"], "no_persisted_truth");
    assert_eq!(report["state_db_present"], true);
}

#[test]
fn openyak_server_recover_reattaches_persisted_truth() {
    let workspace = unique_temp_dir("openyak-server-recover-persisted");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let mut child = ChildGuard::spawn_in(workspace.clone(), false);
    let address = child.advertised_address();
    let operator_token = child.operator_token();
    let auth = auth_header(&operator_token);
    let discovery_path = child.server_info_path();

    let create = http_request_with_retry(
        &address,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: {address}\r\n{auth}Connection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert!(
        create.starts_with("HTTP/1.1 201"),
        "create response should be 201, got: {create}"
    );
    let created: Value =
        serde_json::from_str(response_body(&create)).expect("create body should be json");
    let thread_id = created["thread_id"]
        .as_str()
        .expect("thread_id should be present")
        .to_string();

    drop(child);
    let _ = std::fs::remove_file(&discovery_path);

    let output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "recover"])
        .current_dir(&workspace)
        .output()
        .expect("server recover should run");
    assert!(
        output.status.success(),
        "server recover should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("json server recover should parse");
    assert_eq!(report["status"], "recovered");
    assert_eq!(report["recovery_kind"], "reattach_persisted_truth");
    assert_eq!(report["state_db_present"], true);
    let base_url = report["base_url"]
        .as_str()
        .expect("recover report should include base_url");
    let recovered_address = base_url
        .strip_prefix("http://")
        .expect("base_url should be http");
    let recovered_operator_token = discovery_operator_token(&discovery_path);
    let recovered_auth = auth_header(&recovered_operator_token);

    let list = http_request_with_retry(
        recovered_address,
        &format!(
            "GET /v1/threads HTTP/1.1\r\nHost: {recovered_address}\r\n{recovered_auth}Connection: close\r\n\r\n"
        ),
    );
    assert!(
        list.starts_with("HTTP/1.1 200"),
        "recovered list should be 200, got: {list}"
    );
    let listed: Value =
        serde_json::from_str(response_body(&list)).expect("list body should be json");
    assert!(
        listed["threads"]
            .as_array()
            .expect("threads should be an array")
            .iter()
            .any(|entry| entry["thread_id"].as_str() == Some(thread_id.as_str())),
        "recovered thread should still exist: {listed}"
    );

    let stop_output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("server stop should run");
    assert!(stop_output.status.success(), "server stop should succeed");

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_recover_clears_stale_registration_and_restores_server() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-recover-stale");
    let openyak_dir = workspace.workspace().join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    std::fs::write(
        openyak_dir.join("thread-server.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": "http://127.0.0.1:9",
            "pid": 4242_u32,
            "truthLayer": "daemon_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": "fixture-token"
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "recover"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server recover should run");
    assert!(output.status.success(), "server recover should succeed");
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("json server recover should parse");
    assert_eq!(report["status"], "recovered");
    assert_eq!(report["recovery_kind"], "clear_stale_and_restart");
    assert_eq!(report["stale_registration_cleared"], true);
    assert_ne!(report["base_url"], "http://127.0.0.1:9");
    let performed_actions = report["performed_actions"]
        .as_array()
        .expect("performed actions should be present");
    assert!(
        performed_actions.iter().any(|value| value
            .as_str()
            .is_some_and(|line| line.contains("cleared the stale workspace discovery record"))),
        "{report}"
    );
    assert!(
        performed_actions.iter().any(|value| value
            .as_str()
            .is_some_and(|line| line.contains("started a detached local thread server"))),
        "{report}"
    );
    assert!(
        !performed_actions.iter().any(|value| value
            .as_str()
            .is_some_and(|line| line.contains("reconciled the persisted workspace thread truth"))),
        "{report}"
    );
}

#[test]
fn openyak_server_recover_rejects_unsafe_registration() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-recover-unsafe");
    let openyak_dir = workspace.workspace().join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    let discovery_path = openyak_dir.join("thread-server.json");
    std::fs::write(
        &discovery_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": "http://127.0.0.1:4100",
            "pid": 4242_u32,
            "truthLayer": "process_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": "fixture-token"
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["server", "recover"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server recover should run");
    assert!(
        !output.status.success(),
        "server recover should reject unsafe discovery"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("Status           invalid_registration"),
        "{stdout}"
    );
    assert!(
        discovery_path.exists(),
        "unsafe discovery should remain for manual inspection"
    );
}

#[test]
fn openyak_server_stop_stops_running_local_server() {
    let workspace = unique_temp_dir("openyak-server-stop-running");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let mut child = ChildGuard::spawn_in(workspace.clone(), false);
    let address = child.advertised_address();

    let text_output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(child.workspace())
        .output()
        .expect("server stop should run");
    assert!(text_output.status.success(), "server stop should succeed");
    let stdout = String::from_utf8(text_output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Status           stopped"), "{stdout}");
    assert!(
        stdout.contains(&format!("Base URL         http://{address}")),
        "{stdout}"
    );
    assert!(stdout.contains("Discovery clear  yes"), "{stdout}");

    child.wait_for_exit();
    assert!(
        !child.server_info_path().exists(),
        "stop should clear the discovery file"
    );

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("json server stop should run");
    assert!(
        json_output.status.success(),
        "json stop after exit should be idempotent"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("json server stop should parse");
    assert_eq!(report["status"], "already_stopped");

    drop(child);
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_is_idempotent_without_running_server() {
    let workspace = unique_temp_dir("openyak-server-stop-missing");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("server stop should run");
    assert!(output.status.success(), "server stop should succeed");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("Status           already_stopped"),
        "{stdout}"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_clears_stale_registration() {
    let workspace = unique_temp_dir("openyak-server-stop-stale");
    let openyak_dir = workspace.join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    std::fs::write(
        openyak_dir.join("thread-server.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": "http://127.0.0.1:9",
            "pid": 4242_u32,
            "truthLayer": "daemon_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": "fixture-token"
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("server stop should run");
    assert!(output.status.success(), "server stop should succeed");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("Status           stale_registration_cleared"),
        "{stdout}"
    );
    assert!(
        !openyak_dir.join("thread-server.json").exists(),
        "stale stop should clear the discovery file"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_rejects_unsafe_registration() {
    let workspace = unique_temp_dir("openyak-server-stop-unsafe");
    let openyak_dir = workspace.join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    let discovery_path = openyak_dir.join("thread-server.json");
    std::fs::write(
        &discovery_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": "http://127.0.0.1:4100",
            "pid": 4242_u32,
            "truthLayer": "process_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": "fixture-token"
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("server stop should run");
    assert!(
        !output.status.success(),
        "server stop should reject unsafe discovery"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("Status           invalid_registration"),
        "{stdout}"
    );
    assert!(
        discovery_path.exists(),
        "unsafe discovery should remain for manual inspection"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_rejects_reachable_listener_with_mismatched_pid() {
    let workspace = unique_temp_dir("openyak-server-stop-mismatched-pid");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let spare_workspace = unique_temp_dir("openyak-server-stop-mismatched-pid-spare");
    std::fs::create_dir_all(&spare_workspace).expect("spare workspace should create");

    let mut target = ChildGuard::spawn_in(workspace.clone(), false);
    let target_address = target.advertised_address();

    let mut spare = ChildGuard::spawn_in(spare_workspace.clone(), false);
    let _spare_address = spare.advertised_address();

    let discovery_path = target.server_info_path();
    std::fs::write(
        &discovery_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "baseUrl": format!("http://{target_address}"),
            "pid": spare.pid(),
            "truthLayer": "daemon_local_v1",
            "operatorPlane": "local_loopback_operator_v1",
            "persistence": "workspace_sqlite_v1",
            "attachApi": "/v1/threads",
            "operatorToken": target.operator_token()
        }))
        .expect("thread server info should serialize"),
    )
    .expect("thread server info should write");

    let output = Command::new(common::openyak_binary())
        .args(["server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("server stop should run");
    assert!(
        !output.status.success(),
        "server stop should reject mismatched reachable pid ownership"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.contains("Status           invalid_registration"),
        "{stdout}"
    );
    assert!(stdout.contains("reported pid"), "{stdout}");
    assert!(
        discovery_path.exists(),
        "mismatched discovery should remain"
    );
    assert!(
        TcpStream::connect(&target_address).is_ok(),
        "target server should still be reachable"
    );
    assert!(
        target
            .child
            .try_wait()
            .expect("target try_wait should succeed")
            .is_none(),
        "target server should still be running"
    );
    assert!(
        spare
            .child
            .try_wait()
            .expect("spare try_wait should succeed")
            .is_none(),
        "spare server should still be running"
    );

    let _ = std::fs::remove_dir_all(workspace);
    let _ = std::fs::remove_dir_all(spare_workspace);
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
}

fn discovery_operator_token(path: &std::path::Path) -> String {
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(path).expect("thread server info should be readable"),
    )
    .expect("thread server info should parse");
    record["operatorToken"]
        .as_str()
        .expect("thread server info should include operatorToken")
        .to_string()
}

fn auth_header(token: &str) -> String {
    format!("Authorization: Bearer {token}\r\n")
}

fn http_request_with_retry(address: &str, request: &str) -> String {
    for _ in 0..40 {
        let response = http_request(address, request);
        if !response.is_empty() {
            return response;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("server never produced an http response at {address}");
}

fn http_request(address: &str, request: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("server should accept tcp connections");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout should set");
    stream
        .write_all(request.as_bytes())
        .expect("request should be sent");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("response should be readable");
    response
}

fn response_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("http response should include header/body separator")
}

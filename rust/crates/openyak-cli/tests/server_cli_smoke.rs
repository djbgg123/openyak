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

    fn workspace(&self) -> &PathBuf {
        &self.workspace
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

#[test]
fn openyak_server_surfaces_thread_routes() {
    let mut child = ChildGuard::spawn();
    let address = child.advertised_address();
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

    let create = http_request_with_retry(
        &address,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
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
        &format!("GET /v1/threads HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
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

    let create = http_request_with_retry(
        &address,
        &format!(
            "POST /v1/threads HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
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
            "GET /sessions/{thread_id} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
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
    assert_eq!(
        workspace,
        *restarted.workspace(),
        "restarted server should reuse the same workspace"
    );

    let list = http_request_with_retry(
        &restarted_address,
        &format!(
            "GET /v1/threads HTTP/1.1\r\nHost: {restarted_address}\r\nConnection: close\r\n\r\n"
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
            "GET /v1/threads/{thread_id} HTTP/1.1\r\nHost: {restarted_address}\r\nConnection: close\r\n\r\n"
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
            "GET /sessions/{thread_id} HTTP/1.1\r\nHost: {restarted_address}\r\nConnection: close\r\n\r\n"
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
        stdout.contains("openyak server --bind 127.0.0.1:0"),
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

fn unique_temp_dir(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{counter}"))
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

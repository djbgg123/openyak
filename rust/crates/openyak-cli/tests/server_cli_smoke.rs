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

fn assert_daemon_local_thread_contract(report: &Value) {
    assert_eq!(report["contract"]["truth_layer"], "daemon_local_v1");
    assert_eq!(
        report["contract"]["operator_plane"],
        "local_loopback_operator_v1"
    );
    assert_eq!(report["contract"]["persistence"], "workspace_sqlite_v1");
    assert_eq!(report["contract"]["attach_api"], "/v1/threads");
}

fn assert_missing_thread_contract(report: &Value) {
    assert!(report["contract"]["truth_layer"].is_null(), "{report}");
    assert!(report["contract"]["operator_plane"].is_null(), "{report}");
    assert!(report["contract"]["persistence"].is_null(), "{report}");
    assert!(report["contract"]["attach_api"].is_null(), "{report}");
}

fn assert_disabled_mcp_capability(report: &Value) {
    assert_eq!(report["capabilities"]["mcp"]["status"], "disabled");
    assert_eq!(report["capabilities"]["mcp"]["configured_count"], 0);
    assert_eq!(report["capabilities"]["mcp"]["ready_count"], 0);
    assert_eq!(report["capabilities"]["mcp"]["auth_required_count"], 0);
    assert_eq!(report["capabilities"]["mcp"]["degraded_count"], 0);
    assert!(
        report["capabilities"]["mcp"]["servers"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{report}"
    );
}

fn assert_lifecycle_status(report: &Value, status: &str) {
    assert_eq!(report["lifecycle"]["status"], status);
}

fn assert_lifecycle_has_no_recovery(report: &Value) {
    assert!(report["lifecycle"]["failure_kind"].is_null(), "{report}");
    assert!(report["lifecycle"]["recovery"].is_null(), "{report}");
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
    assert!(stdout.contains("Install status   missing"), "{stdout}");
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");

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
    assert_eq!(report["install"]["status"], "missing");
    assert_eq!(report["capabilities"]["mcp"]["status"], "disabled");

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
    assert!(stdout.contains("Install status   missing"), "{stdout}");
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains("Install stage    openyak server install --bind 127.0.0.1:0"),
        "{stdout}"
    );
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
    assert_eq!(report["install"]["status"], "missing");
    assert_eq!(report["capabilities"]["mcp"]["status"], "disabled");
    assert_eq!(
        report["install"]["suggested_command"],
        "openyak server install --bind 127.0.0.1:0"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_status_surfaces_mcp_degraded_capability_states() {
    let workspace = unique_temp_dir("openyak-server-status-mcp-capabilities");
    std::fs::create_dir_all(workspace.join(".openyak"))
        .expect("workspace config dir should create");
    std::fs::write(
        workspace.join(".openyak").join("settings.json"),
        r#"{
  "mcpServers": {
    "config-auth-required": {
      "type": "http",
      "url": "https://vendor.example/mcp",
      "oauth": {
        "clientId": "demo-client"
      }
    },
    "config-unsupported-sdk": {
      "type": "sdk",
      "name": "demo-sdk"
    },
    "config-stdio": {
      "type": "stdio",
      "command": "cargo",
      "args": ["--version"]
    }
  }
}"#,
    )
    .expect("settings should write");

    let text_output = Command::new(common::openyak_binary())
        .args(["server", "status"])
        .current_dir(&workspace)
        .output()
        .expect("server status should run");
    assert!(text_output.status.success(), "server status should succeed");
    let stdout = String::from_utf8(text_output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("MCP capability   degraded"), "{stdout}");
    assert!(stdout.contains("config-auth-required"), "{stdout}");
    assert!(stdout.contains("config-unsupported-sdk"), "{stdout}");
    assert!(stdout.contains("config-stdio"), "{stdout}");

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
    assert_eq!(report["capabilities"]["mcp"]["status"], "degraded");
    assert_eq!(report["capabilities"]["mcp"]["configured_count"], 3);
    assert_eq!(report["capabilities"]["mcp"]["ready_count"], 1);
    assert_eq!(report["capabilities"]["mcp"]["auth_required_count"], 1);
    assert_eq!(report["capabilities"]["mcp"]["degraded_count"], 1);
    assert!(
        report["capabilities"]["mcp"]["recommended_actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|action| action
                == "repair unsupported MCP transports or invalid MCP config before relying on MCP-backed operator capability")),
        "{report}"
    );
    assert!(
        report["capabilities"]["mcp"]["recommended_actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|action| action
                == "complete auth for configured MCP servers before relying on MCP-backed operator capability")),
        "{report}"
    );
    assert!(
        report["capabilities"]["mcp"]["servers"]
            .as_array()
            .is_some_and(|servers| servers
                .iter()
                .any(|server| server["server"] == "config-auth-required")),
        "{report}"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_status_surfaces_ready_mcp_capability_state() {
    let workspace = unique_temp_dir("openyak-server-status-mcp-ready");
    std::fs::create_dir_all(workspace.join(".openyak"))
        .expect("workspace config dir should create");
    std::fs::write(
        workspace.join(".openyak").join("settings.json"),
        r#"{
  "mcpServers": {
    "config-stdio": {
      "type": "stdio",
      "command": "cargo",
      "args": ["--version"]
    }
  }
}"#,
    )
    .expect("settings should write");

    let text_output = Command::new(common::openyak_binary())
        .args(["server", "status"])
        .current_dir(&workspace)
        .output()
        .expect("server status should run");
    assert!(text_output.status.success(), "server status should succeed");
    let stdout = String::from_utf8(text_output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("MCP capability   ready"), "{stdout}");
    assert!(
        stdout.contains("config-stdio (ready, transport stdio, auth local)"),
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
    assert_eq!(report["capabilities"]["mcp"]["status"], "ready");
    assert_eq!(report["capabilities"]["mcp"]["configured_count"], 1);
    assert_eq!(report["capabilities"]["mcp"]["ready_count"], 1);
    assert_eq!(report["capabilities"]["mcp"]["auth_required_count"], 0);
    assert_eq!(report["capabilities"]["mcp"]["degraded_count"], 0);

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_status_keeps_auth_required_mcp_capability_with_saved_oauth_credentials() {
    let workspace = unique_temp_dir("openyak-server-status-mcp-auth-required-with-creds");
    let config_home = unique_temp_dir("openyak-server-status-mcp-auth-required-home");
    std::fs::create_dir_all(workspace.join(".openyak"))
        .expect("workspace config dir should create");
    std::fs::create_dir_all(&config_home).expect("config home should create");
    std::fs::write(
        config_home.join("credentials.json"),
        r#"{
  "oauth": {
    "accessToken": "demo-access-token",
    "refreshToken": "demo-refresh-token",
    "expiresAt": 4102444800,
    "scopes": ["mcp:test"]
  }
}"#,
    )
    .expect("credentials should write");
    std::fs::write(
        workspace.join(".openyak").join("settings.json"),
        r#"{
  "mcpServers": {
    "config-auth-required": {
      "type": "http",
      "url": "https://vendor.example/mcp",
      "oauth": {
        "clientId": "demo-client"
      }
    }
  }
}"#,
    )
    .expect("settings should write");

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("json server status should run");
    assert!(
        json_output.status.success(),
        "json server status should succeed"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("json server status should parse");
    assert_eq!(report["capabilities"]["mcp"]["status"], "auth_required");
    assert_eq!(report["capabilities"]["mcp"]["configured_count"], 1);
    assert_eq!(report["capabilities"]["mcp"]["ready_count"], 0);
    assert_eq!(report["capabilities"]["mcp"]["auth_required_count"], 1);
    assert_eq!(report["capabilities"]["mcp"]["degraded_count"], 0);

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&config_home);
}

#[test]
#[allow(clippy::too_many_lines)]
fn openyak_server_install_stages_local_service_bundle() {
    let workspace = unique_temp_dir("openyak-server-install");
    let config_home = unique_temp_dir("openyak-server-install-home");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    std::fs::create_dir_all(&config_home).expect("config home should create");

    let first_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "install"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("server install should run");
    assert!(
        first_output.status.success(),
        "server install should succeed: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let first_report: Value =
        serde_json::from_slice(&first_output.stdout).expect("server install json should parse");
    assert_eq!(first_report["status"], "bundle_staged");
    assert_eq!(first_report["install_mode"], "bundle_only");
    assert_eq!(first_report["requested_bind"], "127.0.0.1:0");
    assert_eq!(first_report["replaced_existing_bundle"], false);
    assert_eq!(first_report["state_db_present"], false);
    assert!(
        first_report["activation_commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty()),
        "{first_report}"
    );
    assert!(
        first_report["removal_commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty()),
        "{first_report}"
    );
    let canonical_config_home = config_home.canonicalize().expect("config home canonical");
    let install_root = PathBuf::from(
        first_report["install_root"]
            .as_str()
            .expect("install_root should be present"),
    );
    assert!(
        install_root.starts_with(&canonical_config_home),
        "install root should stay under config home: {first_report}"
    );
    let manifest_path = PathBuf::from(
        first_report["manifest_path"]
            .as_str()
            .expect("manifest_path should be present"),
    );
    let readme_path = PathBuf::from(
        first_report["readme_path"]
            .as_str()
            .expect("readme_path should be present"),
    );
    let launcher_path = PathBuf::from(
        first_report["launcher_path"]
            .as_str()
            .expect("launcher_path should be present"),
    );
    assert!(manifest_path.is_file(), "{first_report}");
    assert!(readme_path.is_file(), "{first_report}");
    assert!(launcher_path.is_file(), "{first_report}");
    let readme = std::fs::read_to_string(&readme_path).expect("install readme should read");
    assert!(
        readme.contains("This command only stages a reversible local bundle"),
        "{readme}"
    );
    if cfg!(windows) {
        assert_eq!(first_report["service_manager"], "windows_task_scheduler");
        assert!(first_report["service_definition_path"].is_null());
        let helper_paths = first_report["helper_paths"]
            .as_array()
            .expect("helper_paths should be present");
        assert_eq!(helper_paths.len(), 2, "{first_report}");
        for helper_path in helper_paths {
            assert!(
                PathBuf::from(helper_path.as_str().expect("helper path should be string"))
                    .is_file(),
                "{first_report}"
            );
        }
    } else if cfg!(target_os = "macos") {
        assert_eq!(first_report["service_manager"], "launchd_agent");
        assert!(
            first_report["service_definition_path"]
                .as_str()
                .is_some_and(|path| PathBuf::from(path).is_file()),
            "{first_report}"
        );
    } else {
        assert_eq!(first_report["service_manager"], "systemd_user");
        assert!(
            first_report["service_definition_path"]
                .as_str()
                .is_some_and(|path| PathBuf::from(path).is_file()),
            "{first_report}"
        );
    }

    let status_text_output = Command::new(common::openyak_binary())
        .args(["server", "status"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("server status should run after install");
    assert!(
        status_text_output.status.success(),
        "server status should succeed after install"
    );
    let status_stdout =
        String::from_utf8(status_text_output.stdout).expect("status stdout should be utf8");
    assert!(
        status_stdout.contains("Install status   bundle_staged"),
        "{status_stdout}"
    );
    assert!(
        status_stdout.contains(&format!("Install root     {}", install_root.display())),
        "{status_stdout}"
    );
    assert!(
        status_stdout.contains("Install activate "),
        "{status_stdout}"
    );
    assert!(
        status_stdout.contains("Install remove   "),
        "{status_stdout}"
    );

    let status_json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("json server status should run after install");
    assert!(
        status_json_output.status.success(),
        "json server status should succeed after install"
    );
    let status_report: Value =
        serde_json::from_slice(&status_json_output.stdout).expect("status json should parse");
    assert_eq!(status_report["status"], "not_running");
    assert_eq!(status_report["install"]["status"], "bundle_staged");
    assert_eq!(
        status_report["install"]["install_root"],
        install_root.display().to_string()
    );
    assert_eq!(
        status_report["install"]["readme_path"],
        readme_path.display().to_string()
    );
    assert_eq!(
        status_report["install"]["manifest_path"],
        manifest_path.display().to_string()
    );
    assert!(
        status_report["install"]["activation_commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty()),
        "{status_report}"
    );

    let second_output = Command::new(common::openyak_binary())
        .args(["server", "install"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("second server install should run");
    assert!(
        second_output.status.success(),
        "second server install should succeed"
    );
    let second_stdout = String::from_utf8(second_output.stdout).expect("stdout should be utf8");
    assert!(
        second_stdout.contains("Status           bundle_staged"),
        "{second_stdout}"
    );
    assert!(
        second_stdout.contains("Replaced bundle  yes"),
        "{second_stdout}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&config_home);
}

#[test]
fn openyak_server_status_rejects_tampered_install_paths() {
    let workspace = unique_temp_dir("openyak-server-install-path-tamper");
    let config_home = unique_temp_dir("openyak-server-install-path-tamper-home");
    let forged_root = unique_temp_dir("openyak-server-install-forged-root");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    std::fs::create_dir_all(&config_home).expect("config home should create");

    let install_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "install"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("server install should run");
    assert!(
        install_output.status.success(),
        "server install should succeed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );
    let install_report: Value =
        serde_json::from_slice(&install_output.stdout).expect("install report should parse");
    let manifest_path = PathBuf::from(
        install_report["manifest_path"]
            .as_str()
            .expect("install manifest path should exist"),
    );

    let mut manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).expect("manifest should read"),
    )
    .expect("manifest should parse");
    manifest["install_root"] = Value::String(forged_root.display().to_string());
    manifest["manifest_path"] = Value::String(
        forged_root
            .join("install-manifest.json")
            .display()
            .to_string(),
    );
    manifest["readme_path"] = Value::String(forged_root.join("README.txt").display().to_string());
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).expect("manifest should serialize"),
    )
    .expect("tampered manifest should write");

    let status_text_output = Command::new(common::openyak_binary())
        .args(["server", "status"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("server status should run after tamper");
    assert!(
        status_text_output.status.success(),
        "server status should still succeed after tamper"
    );
    let status_stdout =
        String::from_utf8(status_text_output.stdout).expect("status stdout should be utf8");
    assert!(
        status_stdout.contains("Install status   invalid_manifest"),
        "{status_stdout}"
    );
    assert!(
        status_stdout.contains(&format!("Install manifest {}", manifest_path.display())),
        "{status_stdout}"
    );

    let status_json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("json server status should run after tamper");
    assert!(
        status_json_output.status.success(),
        "json server status should still succeed after tamper"
    );
    let status_report: Value =
        serde_json::from_slice(&status_json_output.stdout).expect("status report should parse");
    assert_eq!(status_report["install"]["status"], "invalid_manifest");
    assert_eq!(
        status_report["install"]["manifest_path"],
        manifest_path.display().to_string()
    );
    assert!(
        status_report["install"]["problem"]
            .as_str()
            .is_some_and(|problem| problem.contains("install_root")),
        "{status_report}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&config_home);
    let _ = std::fs::remove_dir_all(&forged_root);
}

#[test]
fn openyak_server_status_rejects_missing_install_artifacts() {
    let workspace = unique_temp_dir("openyak-server-install-artifact-tamper");
    let config_home = unique_temp_dir("openyak-server-install-artifact-tamper-home");
    std::fs::create_dir_all(&workspace).expect("workspace should create");
    std::fs::create_dir_all(&config_home).expect("config home should create");

    let install_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "install"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("server install should run");
    assert!(
        install_output.status.success(),
        "server install should succeed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );
    let install_report: Value =
        serde_json::from_slice(&install_output.stdout).expect("install report should parse");
    let readme_path = PathBuf::from(
        install_report["readme_path"]
            .as_str()
            .expect("install readme path should exist"),
    );
    std::fs::remove_file(&readme_path).expect("readme should remove");

    let status_json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "status"])
        .current_dir(&workspace)
        .env("OPENYAK_CONFIG_HOME", &config_home)
        .output()
        .expect("json server status should run after artifact removal");
    assert!(
        status_json_output.status.success(),
        "json server status should still succeed after artifact removal"
    );
    let status_report: Value =
        serde_json::from_slice(&status_json_output.stdout).expect("status report should parse");
    assert_eq!(status_report["install"]["status"], "invalid_manifest");
    assert!(
        status_report["install"]["problem"]
            .as_str()
            .is_some_and(|problem| problem.contains(&readme_path.display().to_string())),
        "{status_report}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&config_home);
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
    assert_daemon_local_thread_contract(&report);
    assert_lifecycle_status(&report, "started");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{report}"
    );
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
fn openyak_server_start_detached_text_reports_operator_contract() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-start-detached-text-contract");

    let output = Command::new(common::openyak_binary())
        .args(["server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server start --detach should run");
    assert!(
        output.status.success(),
        "server start --detach should succeed"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Local thread server start"), "{stdout}");
    assert!(stdout.contains("Status           started"), "{stdout}");
    assert!(
        stdout.contains("Base URL         http://127.0.0.1:"),
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
    assert!(
        stdout.contains("Persistence      workspace_sqlite_v1"),
        "{stdout}"
    );
    assert!(stdout.contains("Attach API       /v1/threads"), "{stdout}");
    assert!(stdout.contains("Lifecycle        started"), "{stdout}");
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains(
            "Scope            local-only detached start action; broader daemon lifecycle controls remain unshipped"
        ),
        "{stdout}"
    );
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

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "start", "--detach"])
        .current_dir(workspace.workspace())
        .output()
        .expect("detached start json should run");
    assert!(
        !json_output.status.success(),
        "detached start should reject unsafe discovery"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("start json should parse");
    assert_eq!(report["status"], "invalid_registration");
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "invalid_registration");
    assert_eq!(
        report["lifecycle"]["failure_kind"],
        "daemon_local_invalid_registration"
    );
    assert_eq!(
        report["lifecycle"]["recovery"]["recovery_kind"],
        "manual_repair_required"
    );
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|value| value
                .as_str()
                .is_some_and(|action| action.contains("openyak server start --detach")))),
        "{report}"
    );

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
        stdout.contains("Lifecycle        invalid_registration"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains(
            "Scope            local-only detached start action; broader daemon lifecycle controls remain unshipped"
        ),
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
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "nothing_to_recover");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
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

    let text_output = Command::new(common::openyak_binary())
        .args(["server", "recover"])
        .current_dir(&workspace)
        .output()
        .expect("server recover text should run");
    assert!(
        text_output.status.success(),
        "server recover text should succeed"
    );
    let stdout = String::from_utf8(text_output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Local thread server recovery"), "{stdout}");
    assert!(
        stdout.contains("Status           nothing_to_recover"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Recovery kind    no_persisted_truth"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Lifecycle        nothing_to_recover"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains(
            "Scope            local-only recovery action for current-workspace daemon_local_v1 thread truth"
        ),
        "{stdout}"
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
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "nothing_to_recover");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
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
    assert_daemon_local_thread_contract(&report);
    assert_lifecycle_status(&report, "recovered");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{report}"
    );
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
    assert_daemon_local_thread_contract(&report);
    assert_lifecycle_status(&report, "recovered");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
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
fn openyak_server_recover_text_reports_operator_contract_after_stale_restart() {
    let workspace = DetachedWorkspaceGuard::new("openyak-server-recover-stale-text-contract");
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
        .args(["server", "recover"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server recover should run");
    assert!(output.status.success(), "server recover should succeed");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Local thread server recovery"), "{stdout}");
    assert!(stdout.contains("Status           recovered"), "{stdout}");
    assert!(
        stdout.contains("Recovery kind    clear_stale_and_restart"),
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
    assert!(
        stdout.contains("Persistence      workspace_sqlite_v1"),
        "{stdout}"
    );
    assert!(stdout.contains("Attach API       /v1/threads"), "{stdout}");
    assert!(stdout.contains("Lifecycle        recovered"), "{stdout}");
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains("Did              cleared the stale workspace discovery record"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "Scope            local-only recovery action for current-workspace daemon_local_v1 thread truth"
        ),
        "{stdout}"
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

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "recover"])
        .current_dir(workspace.workspace())
        .output()
        .expect("server recover json should run");
    assert!(
        !json_output.status.success(),
        "server recover should reject unsafe discovery"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("recover json should parse");
    assert_eq!(report["status"], "invalid_registration");
    assert_eq!(report["recovery_kind"], "manual_repair_required");
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "invalid_registration");
    assert_eq!(
        report["lifecycle"]["failure_kind"],
        "daemon_local_invalid_registration"
    );
    assert_eq!(
        report["lifecycle"]["recovery"]["recovery_kind"],
        "manual_repair_required"
    );
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|value| value
                .as_str()
                .is_some_and(|action| action.contains("openyak server start --detach")))),
        "{report}"
    );

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
        stdout.contains("Recovery kind    manual_repair_required"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Lifecycle        invalid_registration"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains(
            "Scope            local-only recovery action for current-workspace daemon_local_v1 thread truth"
        ),
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
    assert!(
        stdout.contains("Truth layer      daemon_local_v1"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Operator plane   local_loopback_operator_v1"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Persistence      workspace_sqlite_v1"),
        "{stdout}"
    );
    assert!(stdout.contains("Attach API       /v1/threads"), "{stdout}");
    assert!(stdout.contains("Lifecycle        stopped"), "{stdout}");
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains(
            "Scope            local-only stop action; broader daemon lifecycle controls remain unshipped"
        ),
        "{stdout}"
    );

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
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "already_stopped");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);

    drop(child);
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_json_reports_operator_contract_for_running_server() {
    let workspace = unique_temp_dir("openyak-server-stop-running-json");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let mut child = ChildGuard::spawn_in(workspace.clone(), false);
    let _address = child.advertised_address();

    let output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "stop"])
        .current_dir(child.workspace())
        .output()
        .expect("server stop should run");
    assert!(output.status.success(), "server stop should succeed");
    let report: Value = serde_json::from_slice(&output.stdout).expect("stop json should parse");
    assert_eq!(report["status"], "stopped");
    assert_eq!(report["discovery_cleared"], true);
    assert_eq!(report["reachable_before_stop"], true);
    assert_daemon_local_thread_contract(&report);
    assert_lifecycle_status(&report, "stopped");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{report}"
    );

    child.wait_for_exit();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_is_idempotent_without_running_server() {
    let workspace = unique_temp_dir("openyak-server-stop-missing");
    std::fs::create_dir_all(&workspace).expect("workspace should create");

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("json server stop should run");
    assert!(
        json_output.status.success(),
        "json server stop should succeed"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("stop json should parse");
    assert_eq!(report["status"], "already_stopped");
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "already_stopped");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{report}"
    );

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
    assert!(
        stdout.contains("Lifecycle        already_stopped"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains(
            "Scope            local-only stop action; broader daemon lifecycle controls remain unshipped"
        ),
        "{stdout}"
    );

    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn openyak_server_stop_clears_stale_registration() {
    let workspace = unique_temp_dir("openyak-server-stop-stale");
    let openyak_dir = workspace.join(".openyak");
    std::fs::create_dir_all(&openyak_dir).expect("openyak dir should create");
    let stale_record = serde_json::to_string_pretty(&serde_json::json!({
        "baseUrl": "http://127.0.0.1:9",
        "pid": 4242_u32,
        "truthLayer": "daemon_local_v1",
        "operatorPlane": "local_loopback_operator_v1",
        "persistence": "workspace_sqlite_v1",
        "attachApi": "/v1/threads",
        "operatorToken": "fixture-token"
    }))
    .expect("thread server info should serialize");
    std::fs::write(openyak_dir.join("thread-server.json"), &stale_record)
        .expect("thread server info should write");

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("json server stop should run");
    assert!(
        json_output.status.success(),
        "json server stop should succeed"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("json stop should parse");
    assert_eq!(report["status"], "stale_registration_cleared");
    assert_eq!(report["discovery_cleared"], true);
    assert_eq!(report["reachable_before_stop"], false);
    assert_daemon_local_thread_contract(&report);
    assert_lifecycle_status(&report, "stale_registration_cleared");
    assert_lifecycle_has_no_recovery(&report);
    assert_disabled_mcp_capability(&report);
    assert!(
        report["recommended_actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|value| value
                .as_str()
                .is_some_and(|action| action.contains("openyak server start --detach")))),
        "{report}"
    );
    assert!(
        !openyak_dir.join("thread-server.json").exists(),
        "stale stop should clear the discovery file"
    );

    std::fs::write(openyak_dir.join("thread-server.json"), &stale_record)
        .expect("thread server info should rewrite");

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
        stdout.contains("Lifecycle        stale_registration_cleared"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
    assert!(
        stdout.contains("Truth layer      daemon_local_v1"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Try              start `openyak server start --detach --bind 127.0.0.1:0` in this workspace if you want a new local thread server"),
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

    let json_output = Command::new(common::openyak_binary())
        .args(["--output-format", "json", "server", "stop"])
        .current_dir(&workspace)
        .output()
        .expect("json server stop should run");
    assert!(
        !json_output.status.success(),
        "server stop should reject unsafe discovery"
    );
    let report: Value =
        serde_json::from_slice(&json_output.stdout).expect("stop json should parse");
    assert_eq!(report["status"], "invalid_registration");
    assert_missing_thread_contract(&report);
    assert_lifecycle_status(&report, "invalid_registration");
    assert_eq!(
        report["lifecycle"]["failure_kind"],
        "daemon_local_invalid_registration"
    );
    assert_eq!(
        report["lifecycle"]["recovery"]["recovery_kind"],
        "manual_repair_required"
    );
    assert_disabled_mcp_capability(&report);

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
        stdout.contains("Lifecycle        invalid_registration"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
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
    assert!(
        stdout.contains("Lifecycle        invalid_registration"),
        "{stdout}"
    );
    assert!(stdout.contains("MCP capability   disabled"), "{stdout}");
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

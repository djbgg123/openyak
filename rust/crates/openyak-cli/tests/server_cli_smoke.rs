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
}

impl ChildGuard {
    fn spawn() -> Self {
        let workspace = unique_temp_dir("openyak-server-smoke");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let child = Command::new(common::openyak_binary())
            .args(["server", "--bind", "127.0.0.1:0"])
            .current_dir(&workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("openyak server should spawn");
        Self { child, workspace }
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
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

#[test]
fn openyak_server_surfaces_thread_routes() {
    let mut child = ChildGuard::spawn();
    let mut stdout = child.stdout_reader();
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("server should print its startup line");
    assert!(
        line.starts_with("Local thread server listening on http://"),
        "unexpected startup line: {line:?}"
    );
    let address = line
        .trim()
        .strip_prefix("Local thread server listening on http://")
        .expect("startup line should include http address")
        .to_string();
    let server_info = std::fs::read_to_string(child.server_info_path())
        .expect("thread server info file should exist");
    let server_info_json: Value =
        serde_json::from_str(&server_info).expect("thread server info should be json");
    assert_eq!(
        server_info_json["baseUrl"],
        format!("http://{address}"),
        "thread server info should match the advertised address"
    );

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

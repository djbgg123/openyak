use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::{json, Value};

mod common;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
#[allow(clippy::too_many_lines)]
fn clean_env_cli_reaches_mock_anthropic_service_across_scripted_parity_scenarios() {
    let manifest_entries = load_scenario_manifest();
    let manifest = manifest_entries
        .iter()
        .cloned()
        .map(|entry| (entry.name.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let cases = [
        ScenarioCase {
            name: "streaming_text",
            permission_mode: "read-only",
            allowed_tools: None,
            prepare: prepare_noop,
            assert: assert_streaming_text,
        },
        ScenarioCase {
            name: "read_file_roundtrip",
            permission_mode: "read-only",
            allowed_tools: Some("read_file"),
            prepare: prepare_read_fixture,
            assert: assert_read_file_roundtrip,
        },
        ScenarioCase {
            name: "grep_chunk_assembly",
            permission_mode: "read-only",
            allowed_tools: Some("grep_search"),
            prepare: prepare_grep_fixture,
            assert: assert_grep_chunk_assembly,
        },
        ScenarioCase {
            name: "write_file_allowed",
            permission_mode: "workspace-write",
            allowed_tools: Some("write_file"),
            prepare: prepare_noop,
            assert: assert_write_file_allowed,
        },
        ScenarioCase {
            name: "write_file_denied",
            permission_mode: "read-only",
            allowed_tools: Some("write_file"),
            prepare: prepare_noop,
            assert: assert_write_file_denied,
        },
        ScenarioCase {
            name: "multi_tool_turn_roundtrip",
            permission_mode: "read-only",
            allowed_tools: Some("read_file,grep_search"),
            prepare: prepare_multi_tool_fixture,
            assert: assert_multi_tool_turn_roundtrip,
        },
        ScenarioCase {
            name: "plugin_tool_roundtrip",
            permission_mode: "workspace-write",
            allowed_tools: None,
            prepare: prepare_plugin_fixture,
            assert: assert_plugin_tool_roundtrip,
        },
    ];

    let case_names = cases.iter().map(|case| case.name).collect::<Vec<_>>();
    let manifest_names = manifest_entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        case_names, manifest_names,
        "manifest and harness cases must stay aligned"
    );

    let mut scenario_reports = Vec::new();

    for case in cases {
        let workspace = HarnessWorkspace::new(unique_temp_dir(case.name));
        workspace.create().expect("workspace should exist");
        (case.prepare)(&workspace);

        let run = run_case(case, &workspace, &base_url);
        (case.assert)(&workspace, &run);

        let manifest_entry = manifest
            .get(case.name)
            .unwrap_or_else(|| panic!("missing manifest entry for {}", case.name));
        scenario_reports.push(build_scenario_report(
            case.name,
            manifest_entry,
            &run.response,
        ));

        fs::remove_dir_all(&workspace.root).expect("workspace cleanup should succeed");
    }

    let captured = runtime.block_on(server.captured_requests());
    assert_eq!(
        captured.len(),
        13,
        "seven scenarios should produce thirteen requests"
    );
    assert!(captured
        .iter()
        .all(|request| request.path == "/v1/messages"));

    let scenarios = captured
        .iter()
        .map(|request| request.scenario.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        scenarios,
        vec![
            "streaming_text",
            "read_file_roundtrip",
            "read_file_roundtrip",
            "grep_chunk_assembly",
            "grep_chunk_assembly",
            "write_file_allowed",
            "write_file_allowed",
            "write_file_denied",
            "write_file_denied",
            "multi_tool_turn_roundtrip",
            "multi_tool_turn_roundtrip",
            "plugin_tool_roundtrip",
            "plugin_tool_roundtrip",
        ]
    );

    let mut request_counts = BTreeMap::new();
    for request in &captured {
        *request_counts
            .entry(request.scenario.as_str())
            .or_insert(0_usize) += 1;
    }
    for report in &mut scenario_reports {
        report.request_count = *request_counts
            .get(report.name.as_str())
            .unwrap_or_else(|| panic!("missing request count for {}", report.name));
    }

    maybe_write_report(&scenario_reports);

    runtime
        .block_on(server.shutdown())
        .expect("mock service shutdown should succeed");
}

#[derive(Clone, Copy)]
struct ScenarioCase {
    name: &'static str,
    permission_mode: &'static str,
    allowed_tools: Option<&'static str>,
    prepare: fn(&HarnessWorkspace),
    assert: fn(&HarnessWorkspace, &ScenarioRun),
}

struct HarnessWorkspace {
    root: PathBuf,
    config_home: PathBuf,
    home: PathBuf,
}

impl HarnessWorkspace {
    fn new(root: PathBuf) -> Self {
        Self {
            config_home: root.join("config-home"),
            home: root.join("home"),
            root,
        }
    }

    fn create(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.config_home)?;
        fs::create_dir_all(&self.home)?;
        Ok(())
    }
}

struct ScenarioRun {
    response: Value,
}

#[derive(Debug, Clone)]
struct ScenarioManifestEntry {
    name: String,
    category: String,
    description: String,
    parity_refs: Vec<String>,
}

#[derive(Debug)]
struct ScenarioReport {
    name: String,
    category: String,
    description: String,
    parity_refs: Vec<String>,
    iterations: u64,
    request_count: usize,
    tool_uses: Vec<String>,
    tool_error_count: usize,
    final_message: String,
}

fn run_case(case: ScenarioCase, workspace: &HarnessWorkspace, base_url: &str) -> ScenarioRun {
    let current_path = std::env::var("PATH").unwrap_or_default();
    let mut command = Command::new(common::openyak_binary());
    command.current_dir(&workspace.root).env_clear();

    for key in [
        "SYSTEMROOT",
        "COMSPEC",
        "TMP",
        "TEMP",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }

    command
        .env("ANTHROPIC_API_KEY", "test-parity-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("OPENYAK_CONFIG_HOME", &workspace.config_home)
        .env("HOME", &workspace.home)
        .env("USERPROFILE", &workspace.home)
        .env("NO_COLOR", "1")
        .env("PATH", current_path)
        .args([
            "--model",
            "sonnet",
            "--permission-mode",
            case.permission_mode,
            "--output-format=json",
        ]);

    if let Some(allowed_tools) = case.allowed_tools {
        command.args(["--allowedTools", allowed_tools]);
    }

    let prompt = format!("{SCENARIO_PREFIX}{}", case.name);
    command.arg(prompt);

    let output = command.output().expect("openyak should launch");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    ScenarioRun {
        response: parse_json_output(&stdout),
    }
}

fn prepare_noop(_: &HarnessWorkspace) {}

fn prepare_read_fixture(workspace: &HarnessWorkspace) {
    fs::write(workspace.root.join("fixture.txt"), "alpha parity line\n")
        .expect("fixture should write");
}

fn prepare_grep_fixture(workspace: &HarnessWorkspace) {
    fs::write(
        workspace.root.join("fixture.txt"),
        "alpha parity line\nbeta line\ngamma parity line\n",
    )
    .expect("grep fixture should write");
}

fn prepare_multi_tool_fixture(workspace: &HarnessWorkspace) {
    fs::write(
        workspace.root.join("fixture.txt"),
        "alpha parity line\nbeta line\ngamma parity line\n",
    )
    .expect("multi tool fixture should write");
}

fn prepare_plugin_fixture(workspace: &HarnessWorkspace) {
    let plugin_root = workspace
        .root
        .join("external-plugins")
        .join("parity-plugin");
    let tool_dir = plugin_root.join("tools");
    let manifest_dir = plugin_root.join(".openyak-plugin");
    fs::create_dir_all(&tool_dir).expect("plugin tools dir");
    fs::create_dir_all(&manifest_dir).expect("plugin manifest dir");

    let (script_name, script_contents) = if cfg!(windows) {
        (
            "echo-json.bat",
            "@echo off\r\npowershell -NoLogo -NoProfile -Command \"$inputJson = [Console]::In.ReadToEnd(); Write-Output ('{\\\"plugin\\\":\\\"' + $env:OPENYAK_PLUGIN_ID + '\\\",\\\"tool\\\":\\\"' + $env:OPENYAK_TOOL_NAME + '\\\",\\\"input\\\":' + $inputJson + '}')\"\r\n".to_string(),
        )
    } else {
        (
            "echo-json.sh",
            "#!/bin/sh\nINPUT=$(cat)\nprintf '{\"plugin\":\"%s\",\"tool\":\"%s\",\"input\":%s}\\n' \"$OPENYAK_PLUGIN_ID\" \"$OPENYAK_TOOL_NAME\" \"$INPUT\"\n".to_string(),
        )
    };

    let script_path = tool_dir.join(script_name);
    fs::write(&script_path, script_contents).expect("plugin script should write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script_path)
            .expect("plugin script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("plugin script should be executable");
    }

    let manifest = format!(
        r#"{{
  "name": "parity-plugin",
  "version": "1.0.0",
  "description": "mock parity plugin",
  "tools": [
    {{
      "name": "plugin_echo",
      "description": "Echo JSON input",
      "inputSchema": {{
        "type": "object",
        "properties": {{
          "message": {{ "type": "string" }}
        }},
        "required": ["message"],
        "additionalProperties": false
      }},
      "command": "./tools/{script_name}",
      "requiredPermission": "workspace-write"
    }}
  ]
}}"#,
    );
    fs::write(manifest_dir.join("plugin.json"), manifest).expect("plugin manifest should write");

    fs::write(
        workspace.config_home.join("settings.json"),
        json!({
            "enabledPlugins": {
                "parity-plugin@external": true
            },
            "plugins": {
                "externalDirectories": [plugin_root.parent().expect("plugin parent").display().to_string()]
            }
        })
        .to_string(),
    )
    .expect("plugin settings should write");
}

fn assert_streaming_text(_: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(
        run.response["message"],
        Value::String("Mock streaming says hello from the parity harness.".to_string())
    );
    assert_eq!(run.response["iterations"], Value::from(1));
}

fn assert_read_file_roundtrip(workspace: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(run.response["iterations"], Value::from(2));
    assert_eq!(
        run.response["tool_uses"][0]["name"],
        Value::String("read_file".to_string())
    );
    let output = run.response["tool_results"][0]["output"]
        .as_str()
        .expect("tool output");
    assert!(
        output.contains("fixture.txt")
            || output.contains(&workspace.root.join("fixture.txt").display().to_string())
    );
    assert!(output.contains("alpha parity line"));
}

fn assert_grep_chunk_assembly(_: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(run.response["iterations"], Value::from(2));
    assert_eq!(
        run.response["tool_uses"][0]["name"],
        Value::String("grep_search".to_string())
    );
    assert!(run.response["message"]
        .as_str()
        .expect("message text")
        .contains("2 occurrences"));
}

fn assert_write_file_allowed(workspace: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(run.response["iterations"], Value::from(2));
    let tool_result = &run.response["tool_results"][0];
    assert_eq!(
        tool_result["is_error"],
        Value::Bool(false),
        "write_file_allowed unexpectedly failed: {tool_result}"
    );
    let tool_output = tool_result["output"].as_str().expect("tool output");
    let parsed_output: Value = serde_json::from_str(tool_output).expect("write_file output json");
    let generated = tool_output_file_path(workspace, &parsed_output);
    let contents = wait_for_file_contents(&generated).unwrap_or_else(|error| {
        panic!(
            "generated file should exist at {}: {error}",
            generated.display()
        )
    });
    assert_eq!(contents, "created by mock service\n");
}

fn assert_write_file_denied(workspace: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(run.response["iterations"], Value::from(2));
    let tool_output = run.response["tool_results"][0]["output"]
        .as_str()
        .expect("tool output");
    assert!(tool_output.contains("requires workspace-write permission"));
    assert_eq!(
        run.response["tool_results"][0]["is_error"],
        Value::Bool(true)
    );
    assert!(!workspace.root.join("generated").join("denied.txt").exists());
}

fn assert_multi_tool_turn_roundtrip(_: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(run.response["iterations"], Value::from(2));
    let tool_uses = run.response["tool_uses"]
        .as_array()
        .expect("tool uses array");
    assert_eq!(tool_uses.len(), 2);
    assert_eq!(tool_uses[0]["name"], Value::String("read_file".to_string()));
    assert_eq!(
        tool_uses[1]["name"],
        Value::String("grep_search".to_string())
    );
}

fn assert_plugin_tool_roundtrip(_: &HarnessWorkspace, run: &ScenarioRun) {
    assert_eq!(run.response["iterations"], Value::from(2));
    let tool_output = run.response["tool_results"][0]["output"]
        .as_str()
        .expect("tool output");
    let parsed: Value = serde_json::from_str(tool_output).expect("plugin output json");
    assert_eq!(
        parsed["plugin"],
        Value::String("parity-plugin@external".to_string())
    );
    assert_eq!(parsed["tool"], Value::String("plugin_echo".to_string()));
    assert_eq!(
        parsed["input"]["message"],
        Value::String("hello from plugin parity".to_string())
    );
}

fn parse_json_output(stdout: &str) -> Value {
    stdout
        .lines()
        .rev()
        .find_map(|line| {
            let trimmed = line.trim();
            let json_slice = trimmed
                .find('{')
                .map(|start| &trimmed[start..])
                .filter(|candidate| candidate.ends_with('}'));
            json_slice.and_then(|candidate| serde_json::from_str(candidate).ok())
        })
        .unwrap_or_else(|| panic!("no JSON response line found in stdout:\n{stdout}"))
}

fn tool_output_file_path(workspace: &HarnessWorkspace, parsed_output: &Value) -> PathBuf {
    let file_path = parsed_output["filePath"]
        .as_str()
        .expect("write_file filePath");
    let resolved = Path::new(file_path);
    let absolute = if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        workspace.root.join(resolved)
    };
    let normalized = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());
    let workspace_root = workspace
        .root
        .canonicalize()
        .unwrap_or_else(|_| workspace.root.clone());
    assert!(
        normalized.starts_with(&workspace_root),
        "generated file path should stay within workspace: {} vs {}",
        normalized.display(),
        workspace_root.display()
    );
    normalized
}

fn wait_for_file_contents(path: &Path) -> std::io::Result<String> {
    let mut last_error = None;
    for _ in 0..20 {
        match fs::read_to_string(path) {
            Ok(contents) => return Ok(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("file did not appear: {}", path.display()),
        )
    }))
}

fn build_scenario_report(
    name: &str,
    manifest_entry: &ScenarioManifestEntry,
    response: &Value,
) -> ScenarioReport {
    ScenarioReport {
        name: name.to_string(),
        category: manifest_entry.category.clone(),
        description: manifest_entry.description.clone(),
        parity_refs: manifest_entry.parity_refs.clone(),
        iterations: response["iterations"]
            .as_u64()
            .expect("iterations should exist"),
        request_count: 0,
        tool_uses: response["tool_uses"]
            .as_array()
            .expect("tool uses array")
            .iter()
            .filter_map(|value| value["name"].as_str().map(ToOwned::to_owned))
            .collect(),
        tool_error_count: response["tool_results"]
            .as_array()
            .expect("tool results array")
            .iter()
            .filter(|value| value["is_error"].as_bool().unwrap_or(false))
            .count(),
        final_message: response["message"]
            .as_str()
            .expect("message text")
            .to_string(),
    }
}

fn maybe_write_report(reports: &[ScenarioReport]) {
    let Some(path) = std::env::var_os("MOCK_PARITY_REPORT_PATH") else {
        return;
    };

    let payload = json!({
        "scenario_count": reports.len(),
        "request_count": reports.iter().map(|report| report.request_count).sum::<usize>(),
        "scenarios": reports.iter().map(scenario_report_json).collect::<Vec<_>>(),
    });
    fs::write(
        path,
        serde_json::to_vec_pretty(&payload).expect("report json should serialize"),
    )
    .expect("report should write");
}

fn scenario_report_json(report: &ScenarioReport) -> Value {
    json!({
        "name": report.name,
        "category": report.category,
        "description": report.description,
        "parity_refs": report.parity_refs,
        "iterations": report.iterations,
        "request_count": report.request_count,
        "tool_uses": report.tool_uses,
        "tool_error_count": report.tool_error_count,
        "final_message": report.final_message,
    })
}

fn assert_success(output: &Output) {
    if output.status.success() {
        return;
    }
    panic!(
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn load_scenario_manifest() -> Vec<ScenarioManifestEntry> {
    let raw: Value = serde_json::from_slice(include_bytes!("../../../mock_parity_scenarios.json"))
        .expect("scenario manifest should parse");
    raw.as_array()
        .expect("scenario manifest array")
        .iter()
        .map(|entry| ScenarioManifestEntry {
            name: entry["name"].as_str().expect("scenario name").to_string(),
            category: entry["category"]
                .as_str()
                .expect("scenario category")
                .to_string(),
            description: entry["description"]
                .as_str()
                .expect("scenario description")
                .to_string(),
            parity_refs: entry["parity_refs"]
                .as_array()
                .expect("scenario refs")
                .iter()
                .map(|value| value.as_str().expect("scenario ref").to_string())
                .collect(),
        })
        .collect()
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after unix epoch")
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mock-parity-{label}-{nanos}-{counter}"))
}

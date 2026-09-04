use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn initializes_and_publishes_compact_frame_schemas_without_a_daemon() {
    let directory = tempfile::TempDir::new().expect("create temporary directory");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cu"))
        .arg("mcp")
        .arg("--socket")
        .arg(directory.path().join("missing.sock"))
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cu mcp");

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "1.0" }
        }
    });
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let tools_list = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let mut stdin = child.stdin.take().expect("child stdin");
    writeln!(stdin, "{initialize}").expect("write initialize request");
    writeln!(stdin, "{initialized}").expect("write initialized notification");
    writeln!(stdin, "{tools_list}").expect("write tools/list request");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for cu mcp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "cu mcp failed: {stderr}");
    assert!(
        stderr.contains("desktop profile unavailable; using generic MCP instructions"),
        "missing fallback diagnostic: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).expect("MCP response is UTF-8");
    let responses = stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse MCP response"))
        .collect::<Vec<_>>();
    let initialize_response = responses
        .iter()
        .find(|response| response["id"] == 1)
        .expect("initialize response");
    assert_eq!(initialize_response["result"]["serverInfo"]["name"], "cu");
    let instructions = initialize_response["result"]["instructions"]
        .as_str()
        .expect("MCP instructions are text");
    assert!(instructions.contains("Use computer_observe before the first action"));
    assert!(!instructions.contains("Desktop profile:"));
    assert!(!instructions.contains("frame_id"));

    let tools_response = responses
        .iter()
        .find(|response| response["id"] == 2)
        .expect("tools/list response");
    let published = serde_json::to_string(&tools_response["result"]).unwrap();
    assert!(!published.contains("frame_id"));
    assert!(!published.contains("expected_frame_id"));
    let tools = tools_response["result"]["tools"].as_array().unwrap();
    let observe = tools
        .iter()
        .find(|tool| tool["name"] == "computer_observe")
        .unwrap();
    let act = tools
        .iter()
        .find(|tool| tool["name"] == "computer_act")
        .unwrap();
    assert_eq!(
        observe["outputSchema"]["properties"]["frame"]["type"],
        "integer"
    );
    assert_eq!(act["inputSchema"]["properties"]["frame"]["type"], "integer");
}

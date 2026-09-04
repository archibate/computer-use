use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn initializes_with_generic_instructions_when_daemon_is_unavailable() {
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
    writeln!(child.stdin.take().expect("child stdin"), "{initialize}")
        .expect("write initialize request");

    let output = child.wait_with_output().expect("wait for cu mcp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "cu mcp failed: {stderr}");
    assert!(
        stderr.contains("desktop profile unavailable; using generic MCP instructions"),
        "missing fallback diagnostic: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).expect("MCP response is UTF-8");
    let response: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("parse initialize response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["serverInfo"]["name"], "cu");
    let instructions = response["result"]["instructions"]
        .as_str()
        .expect("MCP instructions are text");
    assert!(instructions.contains("Use computer_observe before the first action"));
    assert!(!instructions.contains("Desktop profile:"));
}

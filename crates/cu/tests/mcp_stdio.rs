use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn initializes_when_the_mcp_host_strips_xdg_runtime_dir() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cu"))
        .arg("mcp")
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

    let stdout = String::from_utf8(output.stdout).expect("MCP response is UTF-8");
    let response: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("parse initialize response");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["serverInfo"]["name"], "cu");
}

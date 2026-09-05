use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use cu_protocol::{
    CoordinateSpace, DaemonRequest, DaemonResponse, Observation, Rect, RequestEnvelope,
    ResponseEnvelope, ResponseResult, WaitOutcome, WaitStatus,
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
    assert!(instructions.contains("computer_wait"));
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
    let wait = tools
        .iter()
        .find(|tool| tool["name"] == "computer_wait")
        .unwrap();
    assert_eq!(
        observe["outputSchema"]["properties"]["frame"]["type"],
        "integer"
    );
    assert_eq!(act["inputSchema"]["properties"]["frame"]["type"], "integer");
    assert_eq!(
        wait["inputSchema"]["properties"]["include_rects"]["minItems"],
        1
    );
    assert_eq!(
        wait["outputSchema"]["properties"]["elapsed_ms"]["type"],
        "integer"
    );
}

#[test]
#[ignore = "requires local Unix socket access"]
#[allow(clippy::too_many_lines)]
fn observe_timeout_and_changed_wait_form_one_mcp_frame_sequence() {
    let directory = tempfile::TempDir::new().expect("create temporary directory");
    let socket = directory.path().join("cu.sock");
    let first_image = directory.path().join("first.png");
    let second_image = directory.path().join("second.png");
    std::fs::write(&first_image, [1, 2, 3]).unwrap();
    std::fs::write(&second_image, [4, 5, 6]).unwrap();
    let listener = UnixListener::bind(&socket).expect("bind mock daemon");
    let daemon = thread::spawn(move || {
        let (profile_stream, profile) = read_daemon_request(&listener);
        assert!(matches!(profile.request, DaemonRequest::Profile));
        write_daemon_response(
            profile_stream,
            &ResponseEnvelope {
                request_id: profile.request_id,
                result: ResponseResult::Ok(DaemonResponse::Profile(None)),
            },
        );

        let (observe_stream, observe) = read_daemon_request(&listener);
        assert!(matches!(observe.request, DaemonRequest::Observe(_)));
        write_daemon_response(
            observe_stream,
            &ResponseEnvelope {
                request_id: observe.request_id,
                result: ResponseResult::Ok(DaemonResponse::Observe(observation(
                    "f_first",
                    &first_image,
                ))),
            },
        );

        let (timeout_stream, timed_out) = read_daemon_request(&listener);
        let DaemonRequest::Wait(wait) = timed_out.request else {
            panic!("expected timeout wait");
        };
        assert_eq!(wait.last_frame_id, "f_first");
        assert_eq!(wait.include_rects.len(), 1);
        assert_eq!(wait.exclude_rects.len(), 1);
        assert_eq!(wait.coalesce_ms, 2_000);
        write_daemon_response(
            timeout_stream,
            &ResponseEnvelope {
                request_id: timed_out.request_id,
                result: ResponseResult::Ok(DaemonResponse::Wait(WaitOutcome {
                    status: WaitStatus::Timeout,
                    elapsed_ms: 25,
                    frame_id: "f_first".to_owned(),
                    activity_bbox: None,
                    observation: None,
                })),
            },
        );

        let (changed_stream, changed) = read_daemon_request(&listener);
        let DaemonRequest::Wait(wait) = changed.request else {
            panic!("expected changed wait");
        };
        assert_eq!(wait.last_frame_id, "f_first");
        write_daemon_response(
            changed_stream,
            &ResponseEnvelope {
                request_id: changed.request_id,
                result: ResponseResult::Ok(DaemonResponse::Wait(WaitOutcome {
                    status: WaitStatus::Changed,
                    elapsed_ms: 3_250,
                    frame_id: "f_second".to_owned(),
                    activity_bbox: Some(Rect {
                        x: 10,
                        y: 20,
                        width: 30,
                        height: 40,
                    }),
                    observation: Some(observation("f_second", &second_image)),
                })),
            },
        );
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_cu"))
        .arg("mcp")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cu mcp");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));

    writeln!(stdin, "{}", initialize_request(1)).unwrap();
    assert_eq!(read_mcp_response(&mut stdout)["id"], 1);
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
    )
    .unwrap();

    writeln!(
        stdin,
        "{}",
        tool_call(2, "computer_observe", &serde_json::json!({}))
    )
    .unwrap();
    let observed = read_mcp_response(&mut stdout);
    assert_eq!(observed["result"]["structuredContent"]["frame"], 1);
    assert_eq!(observed["result"]["content"].as_array().unwrap().len(), 2);

    let wait_arguments = serde_json::json!({
        "frame": 1,
        "include_rects": [{"x": 0, "y": 0, "width": 100, "height": 80}],
        "exclude_rects": [{"x": 90, "y": 0, "width": 10, "height": 10}]
    });
    writeln!(stdin, "{}", tool_call(3, "computer_wait", &wait_arguments)).unwrap();
    let timed_out = read_mcp_response(&mut stdout);
    assert_eq!(
        timed_out["result"]["structuredContent"]["status"],
        "timeout"
    );
    assert_eq!(timed_out["result"]["structuredContent"]["frame"], 1);
    assert_eq!(timed_out["result"]["content"].as_array().unwrap().len(), 1);

    writeln!(stdin, "{}", tool_call(4, "computer_wait", &wait_arguments)).unwrap();
    let changed = read_mcp_response(&mut stdout);
    assert_eq!(changed["result"]["structuredContent"]["status"], "changed");
    assert_eq!(changed["result"]["structuredContent"]["frame"], 2);
    assert_eq!(
        changed["result"]["structuredContent"]["observation"]["frame"],
        2
    );
    assert_eq!(changed["result"]["content"].as_array().unwrap().len(), 2);
    assert_eq!(changed["result"]["content"][1]["type"], "image");

    drop(stdin);
    let output = child.wait_with_output().expect("wait for cu mcp");
    assert!(
        output.status.success(),
        "cu mcp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    daemon.join().expect("mock daemon thread");
}

#[test]
#[ignore = "requires local Unix socket access"]
#[allow(clippy::too_many_lines)]
fn mcp_cancellation_disconnects_the_inflight_daemon_wait() {
    let directory = tempfile::TempDir::new().expect("create temporary directory");
    let socket = directory.path().join("cu.sock");
    let image_path = directory.path().join("frame.png");
    std::fs::write(&image_path, [1, 2, 3]).unwrap();
    let listener = UnixListener::bind(&socket).expect("bind mock daemon");
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let (disconnected_tx, disconnected_rx) = mpsc::channel();
    let daemon = thread::spawn(move || {
        let (profile_stream, profile) = read_daemon_request(&listener);
        write_daemon_response(
            profile_stream,
            &ResponseEnvelope {
                request_id: profile.request_id,
                result: ResponseResult::Ok(DaemonResponse::Profile(None)),
            },
        );
        let (observe_stream, observed) = read_daemon_request(&listener);
        write_daemon_response(
            observe_stream,
            &ResponseEnvelope {
                request_id: observed.request_id,
                result: ResponseResult::Ok(DaemonResponse::Observe(observation(
                    "f_first",
                    &image_path,
                ))),
            },
        );
        let (mut wait_stream, waiting) = read_daemon_request(&listener);
        assert!(matches!(waiting.request, DaemonRequest::Wait(_)));
        waiting_tx.send(()).unwrap();
        wait_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0_u8; 1];
        disconnected_tx
            .send(
                wait_stream
                    .read(&mut byte)
                    .expect("watch daemon disconnect")
                    == 0,
            )
            .unwrap();
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_cu"))
        .arg("mcp")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cu mcp");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    writeln!(stdin, "{}", initialize_request(1)).unwrap();
    let _ = read_mcp_response(&mut stdout);
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        tool_call(2, "computer_observe", &serde_json::json!({}))
    )
    .unwrap();
    let _ = read_mcp_response(&mut stdout);
    writeln!(
        stdin,
        "{}",
        tool_call(
            3,
            "computer_wait",
            &serde_json::json!({
                "frame": 1,
                "include_rects": [{"x": 0, "y": 0, "width": 100, "height": 80}],
                "timeout_ms": 600_000
            }),
        )
    )
    .unwrap();
    waiting_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("daemon received wait");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 3, "reason": "integration test"}
        })
    )
    .unwrap();
    assert!(
        disconnected_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("daemon disconnect result")
    );

    drop(stdin);
    let output = child.wait_with_output().expect("wait for cu mcp");
    assert!(
        output.status.success(),
        "cu mcp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    daemon.join().expect("mock daemon thread");
}

fn initialize_request(id: u64) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "integration-test", "version": "1.0"}
        }
    })
}

fn tool_call(id: u64, name: &str, arguments: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    })
}

fn read_mcp_response(reader: &mut BufReader<impl std::io::Read>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read MCP response");
    serde_json::from_str(&line).expect("parse MCP response")
}

fn read_daemon_request(
    listener: &UnixListener,
) -> (std::os::unix::net::UnixStream, RequestEnvelope) {
    let (stream, _) = listener.accept().expect("accept daemon request");
    let mut reader = BufReader::new(stream.try_clone().expect("clone daemon stream"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read daemon request");
    (
        stream,
        serde_json::from_str(&line).expect("parse daemon request"),
    )
}

fn write_daemon_response(mut stream: std::os::unix::net::UnixStream, response: &ResponseEnvelope) {
    writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
}

fn observation(frame_id: &str, image_path: &std::path::Path) -> Observation {
    Observation {
        frame_id: frame_id.to_owned(),
        target: "mock:screen".to_owned(),
        width: 100,
        height: 80,
        coordinate_space: CoordinateSpace::FramePixels,
        settled: true,
        image_path: image_path.to_string_lossy().into_owned(),
    }
}

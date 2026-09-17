use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use cu_protocol::{
    ActOutcome, ActStatus, CoordinateSpace, CuError, DaemonRequest, DaemonResponse, ErrorCode,
    Observation, Rect, RequestEnvelope, ResponseEnvelope, ResponseResult, WaitOutcome,
};
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(5);

#[test]
#[ignore = "requires local Unix socket access"]
fn action_recovery_reaches_text_and_structured_clients_with_execution_evidence() {
    for case in [
        "partial",
        "expired",
        "expired_partial",
        "missing_image",
        "indeterminate",
    ] {
        let mut mcp = McpFixture::new();
        mcp.observe(2, "baseline");
        mcp.call(3, "computer_act", json!({
            "frame": 1,
            "actions": [{"type": "keypress", "keys": ["A"]}, {"type": "keypress", "keys": ["B"]}]
        }));
        let mut outcome = mcp.action_outcome("acted");
        if matches!(case, "partial" | "expired_partial" | "missing_image") {
            outcome.status = ActStatus::Partial;
            outcome.action_error =
                Some(CuError::new(ErrorCode::InputFailed, "second input failed").with_executed(1));
        }
        if matches!(case, "expired" | "expired_partial") {
            outcome.image_expired = true;
            outcome.observation = None;
        } else if case == "missing_image" {
            outcome.observation.as_mut().unwrap().image_path = mcp
                .directory
                .path()
                .join("missing.png")
                .to_string_lossy()
                .into_owned();
        }
        let response = if case == "indeterminate" {
            ResponseResult::Error(
                CuError::new(
                    ErrorCode::Indeterminate,
                    "one action executed; capture failed",
                )
                .with_executed(1),
            )
        } else {
            ResponseResult::Ok(DaemonResponse::Act(outcome))
        };
        mcp.daemon_call().respond(response);
        let response = mcp.response(3);
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["executed"], 1, "{case}: {result}");
        let message = result["message"].as_str().unwrap();
        assert!(message.contains("do not repeat"), "{case}: {message}");
        if case == "partial" {
            assert!(message.contains("returned observation"));
            assert_eq!(result["observation"]["frame"], 2);
        } else {
            assert!(message.contains("computer_observe"), "{case}: {message}");
            assert!(!message.contains("returned observation"));
            assert!(result.get("observation").is_none());
        }
        if matches!(case, "partial" | "expired_partial" | "missing_image") {
            assert_eq!(result["action_error"]["code"], "input_failed");
            assert_eq!(result["action_error"]["executed"], 1);
        }
        assert_eq!(
            response["result"]["isError"],
            matches!(case, "missing_image" | "indeterminate")
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                response["result"]["content"][0]["text"].as_str().unwrap()
            )
            .unwrap(),
            *result
        );
        mcp.close();
    }
}

#[test]
#[ignore = "requires local Unix socket access"]
fn daemon_cancelled_synchronous_wait_returns_recovery_and_clears_its_frame() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    mcp.call(3, "computer_wait", wait_arguments(1, false));
    mcp.daemon_call()
        .respond(ResponseResult::Error(CuError::new(
            ErrorCode::Cancelled,
            "screen-change wait was cancelled",
        )));
    let response = mcp.response(3);
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["code"], "cancelled");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("computer_observe")
    );
    assert_eq!(
        serde_json::from_str::<Value>(response["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        *result
    );
    mcp.call(4, "computer_wait", wait_arguments(1, false));
    assert_eq!(
        mcp.response(4)["result"]["structuredContent"]["code"],
        "stale_frame"
    );
    assert!(mcp.listener.accept().is_err());
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn lost_action_response_reports_uncertainty_and_observation_recovery() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    mcp.call(
        3,
        "computer_act",
        json!({"frame": 1, "actions": [{"type": "keypress", "keys": ["A"]}]}),
    );
    drop(mcp.daemon_call());
    let response = mcp.response(3);
    let result = &response["result"]["structuredContent"];
    assert_eq!(response["result"]["isError"], true);
    let message = result["message"].as_str().unwrap();
    assert!(message.contains("outcome is unknown"));
    assert!(message.contains("computer_observe"));
    assert!(result.get("executed").is_none());
    assert_eq!(
        serde_json::from_str::<Value>(response["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        *result
    );
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn async_wait_retains_private_results_across_new_starts_and_shutdown() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let first_path = mcp.start_wait(3, 1);
    let waiting = mcp.daemon_call();
    let DaemonRequest::Wait(request) = &waiting.request.request else {
        panic!("expected daemon wait");
    };
    assert_eq!(request.last_frame_id, "baseline");
    assert_eq!(request.timeout_ms, 60_000);
    assert!(
        !first_path.exists(),
        "started is returned before completion"
    );
    assert!(first_path.starts_with(mcp.directory.path().join("computer-use/mcp-waits")));
    for directory in [
        first_path.parent().unwrap(),
        first_path.parent().unwrap().parent().unwrap(),
    ] {
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    // The initiating request is over. A late request cancellation cannot own the task.
    mcp.send(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 3, "reason": "request already returned"}
    }));
    waiting.respond(ResponseResult::Ok(DaemonResponse::Wait(
        WaitOutcome::Timeout { elapsed_ms: 42 },
    )));
    let first_result = terminal_result(&first_path);
    assert_eq!(first_result, json!({"status": "timeout", "elapsed_ms": 42}));
    assert_eq!(
        first_path.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Start the detector only after publication: checking existing files still wakes once.
    assert_eq!(watcher_wake_count(&first_path), 1);

    let second_path = mcp.start_wait(4, 1);
    assert_ne!(first_path, second_path);
    assert_eq!(terminal_result(&first_path), first_result);
    let waiting = mcp.daemon_call();
    let DaemonRequest::Wait(request) = &waiting.request.request else {
        panic!("expected daemon wait");
    };
    assert_eq!(
        request.last_frame_id, "baseline",
        "timeout preserves grounding"
    );
    let observation = mcp.observation("changed", &mcp.directory.path().join("missing.png"));
    waiting.respond(ResponseResult::Ok(DaemonResponse::Wait(
        WaitOutcome::Changed {
            elapsed_ms: 91,
            activity_bbox: Rect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            observation,
        },
    )));
    let changed = terminal_result(&second_path);
    assert!(
        changed["message"]
            .as_str()
            .unwrap()
            .contains("computer_observe")
    );
    assert_eq!(
        changed,
        json!({
            "status": "changed", "elapsed_ms": 91, "settled": true,
            "activity_bbox": {"x": 1, "y": 2, "width": 3, "height": 4},
            "message": changed["message"]
        })
    );
    assert_eq!(watcher_wake_count(&second_path), 1);
    mcp.call(5, "computer_wait", wait_arguments(1, true));
    assert_eq!(
        mcp.response(5)["result"]["structuredContent"]["code"],
        "stale_frame"
    );
    assert!(
        second_path.exists(),
        "rejected start retains the previous result"
    );
    mcp.close();
    assert_eq!(terminal_result(&first_path), first_result);
    assert_eq!(terminal_result(&second_path)["status"], "changed");
    assert_eq!(watcher_wake_count(&first_path), 1);
    assert_eq!(watcher_wake_count(&second_path), 1);
    assert_eq!(
        std::fs::read_dir(second_path.parent().unwrap())
            .unwrap()
            .count(),
        2,
        "shutdown retains both complete results without staging files"
    );
}

#[test]
#[ignore = "requires local Unix socket access"]
fn observe_cancels_pending_wait_before_requesting_the_next_frame() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let path = mcp.start_wait(3, 1);
    let waiting = mcp.daemon_call();

    mcp.call(4, "computer_wait", wait_arguments(1, true));
    assert_eq!(
        mcp.response(4)["result"]["structuredContent"]["code"],
        "busy"
    );
    assert!(!path.exists());

    mcp.call(5, "computer_wait", wait_arguments(1, false));
    let synchronous_wait = mcp.daemon_call();
    assert!(matches!(
        synchronous_wait.request.request,
        DaemonRequest::Wait(_)
    ));
    synchronous_wait.respond(ResponseResult::Error(CuError::new(
        ErrorCode::Busy,
        "daemon wait slot is occupied",
    )));
    assert_eq!(
        mcp.response(5)["result"]["structuredContent"]["code"],
        "busy"
    );
    assert!(
        !path.exists(),
        "a rejected synchronous wait preserves the pending task"
    );

    mcp.call(6, "computer_observe", json!({}));
    let observe = mcp.daemon_call();
    assert!(matches!(observe.request.request, DaemonRequest::Observe(_)));
    waiting.assert_closed();
    assert_eq!(terminal_result(&path)["status"], "cancelled");
    assert_eq!(watcher_wake_count(&path), 1);
    observe.respond(ResponseResult::Ok(DaemonResponse::Observe(
        mcp.image_observation("new"),
    )));
    assert_eq!(mcp.response(6)["result"]["structuredContent"]["frame"], 2);

    let next_path = mcp.start_wait(7, 2);
    assert_ne!(path, next_path);
    assert_eq!(terminal_result(&path)["status"], "cancelled");
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Wait(
            WaitOutcome::Timeout { elapsed_ms: 1 },
        )));
    assert_eq!(terminal_result(&next_path)["status"], "timeout");
    mcp.close();
    assert_eq!(terminal_result(&path)["status"], "cancelled");
    assert_eq!(watcher_wake_count(&path), 1);
}

#[test]
#[ignore = "requires local Unix socket access"]
fn invalid_action_and_replay_preserve_wait_but_new_action_cancels_it() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "first");
    let first_action = json!({"frame": 1, "actions": [{"type": "keypress", "keys": ["A"]}]});
    mcp.call(3, "computer_act", first_action.clone());
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Act(
            mcp.action_outcome("acted"),
        )));
    assert_eq!(
        mcp.response(3)["result"]["structuredContent"]["observation"]["frame"],
        2
    );
    mcp.observe(4, "newer");
    let path = mcp.start_wait(5, 3);
    let waiting = mcp.daemon_call();

    mcp.call(6, "computer_act", json!({"frame": 3, "actions": []}));
    mcp.daemon_call()
        .respond(ResponseResult::Error(CuError::new(
            ErrorCode::InvalidAction,
            "empty actions",
        )));
    assert_eq!(
        mcp.response(6)["result"]["structuredContent"]["code"],
        "invalid_action"
    );
    assert!(
        !path.exists(),
        "invalid actions do not replace the wait baseline"
    );

    mcp.call(3, "computer_act", first_action);
    let replay = mcp.daemon_call();
    let DaemonRequest::Act(action) = &replay.request.request else {
        panic!("expected replayed daemon action");
    };
    assert_eq!(action.expected_frame_id, "first");
    replay.respond(ResponseResult::Ok(DaemonResponse::Act(
        mcp.action_outcome("acted"),
    )));
    assert_eq!(
        mcp.response(3)["result"]["structuredContent"]["observation"]["frame"],
        2
    );
    assert!(
        !path.exists(),
        "cached replay does not replace the current frame"
    );

    mcp.call(
        7,
        "computer_act",
        json!({"frame": 3, "actions": [{"type": "keypress", "keys": ["B"]}]}),
    );
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Act(
            mcp.action_outcome("latest"),
        )));
    assert_eq!(
        mcp.response(7)["result"]["structuredContent"]["observation"]["frame"],
        4
    );
    waiting.assert_closed();
    assert_eq!(terminal_result(&path)["status"], "cancelled");
    assert_eq!(watcher_wake_count(&path), 1);
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn daemon_failures_publish_visible_results_including_stale_and_cancelled_waits() {
    for code in [
        ErrorCode::Busy,
        ErrorCode::InvalidAction,
        ErrorCode::CaptureFailed,
        ErrorCode::ViewportChanged,
        ErrorCode::StaleFrame,
        ErrorCode::Cancelled,
    ] {
        let mut mcp = McpFixture::new();
        mcp.observe(2, "baseline");
        let path = mcp.start_wait(3, 1);
        mcp.daemon_call()
            .respond(ResponseResult::Error(CuError::new(
                code,
                "mock wait failure",
            )));
        let result = terminal_result(&path);
        if matches!(code, ErrorCode::StaleFrame | ErrorCode::Cancelled) {
            assert_eq!(result["status"], "cancelled", "{code}");
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("computer_observe")
            );
        } else {
            assert_eq!(result["status"], "error", "{code}");
            assert_eq!(result["code"], code.to_string());
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("mock wait failure")
            );
            if matches!(code, ErrorCode::ViewportChanged | ErrorCode::CaptureFailed) {
                assert!(
                    result["message"]
                        .as_str()
                        .unwrap()
                        .contains("computer_observe")
                );
            }
            if code == ErrorCode::Busy {
                assert!(result["message"].as_str().unwrap().contains("active wait"));
            }
        }
        assert_eq!(watcher_wake_count(&path), 1);
        assert!(result["elapsed_ms"].is_u64());
        mcp.close();
    }
}

#[test]
#[ignore = "requires local Unix socket access"]
fn daemon_disconnect_publishes_error_and_releases_local_pending_slot() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let path = mcp.start_wait(3, 1);
    drop(mcp.daemon_call());
    let result = terminal_result(&path);
    assert_eq!(result["status"], "error");
    assert!(result["code"].is_string());
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("computer_observe")
    );
    assert_eq!(watcher_wake_count(&path), 1);

    mcp.observe(4, "fresh");
    let next_path = mcp.start_wait(5, 2);
    assert_ne!(path, next_path);
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Wait(
            WaitOutcome::Timeout { elapsed_ms: 1 },
        )));
    assert_eq!(terminal_result(&next_path)["status"], "timeout");
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn polling_observes_only_complete_json_even_for_large_results() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let path = mcp.start_wait(3, 1);
    let waiting = mcp.daemon_call();
    let watched_path = path.clone();
    let (watching, ready) = mpsc::channel();
    let watcher = thread::spawn(move || {
        assert!(!watched_path.exists());
        watching.send(()).unwrap();
        terminal_result(&watched_path)
    });
    ready
        .recv_timeout(LIMIT)
        .expect("file detector starts before completion");
    let message = "large result ".repeat(16_384);
    waiting.respond(ResponseResult::Error(CuError::new(
        ErrorCode::Internal,
        &message,
    )));
    let result = watcher
        .join()
        .expect("file detector must never observe partial JSON");
    assert_eq!(result["status"], "error");
    assert_eq!(result["message"], message);
    let files = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1, "publication leaves no temporary files");
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn eof_and_signals_publish_cancellation_for_a_late_monitor() {
    for signal in [
        None,
        Some(rustix::process::Signal::TERM),
        Some(rustix::process::Signal::INT),
    ] {
        eprintln!("shutdown scenario: {signal:?}");
        let mut mcp = McpFixture::new();
        mcp.observe(2, "baseline");
        let path = mcp.start_wait(3, 1);
        let waiting = mcp.daemon_call();
        if let Some(signal) = signal {
            mcp.signal(signal);
            mcp.wait_for_exit();
        } else {
            mcp.close();
        }
        waiting.assert_closed();
        assert_eq!(terminal_result(&path)["status"], "cancelled");
        assert_eq!(watcher_wake_count(&path), 1);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "shutdown retains cancellation without staging files"
        );
    }
}

#[test]
#[ignore = "requires local Unix socket access"]
fn shutdown_does_not_wait_for_an_observe_holding_the_frame_state_lock() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let path = mcp.start_wait(3, 1);
    let waiting = mcp.daemon_call();
    mcp.call(4, "computer_observe", json!({}));
    let observing = mcp.daemon_call();
    waiting.assert_closed();
    assert_eq!(terminal_result(&path)["status"], "cancelled");
    let cancelled = std::fs::read(&path).unwrap();
    mcp.signal(rustix::process::Signal::TERM);
    mcp.wait_for_exit();
    observing.assert_closed();
    assert_eq!(std::fs::read(&path).unwrap(), cancelled);
    assert_eq!(watcher_wake_count(&path), 1);
}

#[test]
#[ignore = "requires local Unix socket access"]
fn eof_closes_async_publication_before_draining_a_pending_synchronous_wait() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let path = mcp.start_wait(3, 1);
    let mut asynchronous = mcp.daemon_call();
    mcp.call(4, "computer_wait", wait_arguments(1, false));
    let synchronous = mcp.daemon_call();
    assert!(matches!(
        synchronous.request.request,
        DaemonRequest::Wait(_)
    ));

    let deadline = Instant::now() + Duration::from_secs(2);
    asynchronous
        .stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    mcp.stdin.take();
    let mut byte = [0_u8; 1];
    assert_eq!(
        asynchronous
            .stream
            .read(&mut byte)
            .expect("EOF promptly closes the async daemon connection"),
        0
    );
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "EOF cancellation must precede rmcp's five-second handler drain"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        Instant::now() < deadline,
        "async cancellation exceeded two seconds"
    );
    assert_eq!(terminal_result(&path)["status"], "cancelled");
    let cancelled = std::fs::read(&path).unwrap();

    // Arrange a terminal reply only after the transport's shutdown barrier.
    let late_response = ResponseEnvelope {
        request_id: asynchronous.request.request_id,
        result: ResponseResult::Ok(DaemonResponse::Wait(WaitOutcome::Timeout { elapsed_ms: 1 })),
    };
    assert!(
        writeln!(
            asynchronous.stream,
            "{}",
            serde_json::to_string(&late_response).unwrap()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), cancelled);
    mcp.wait_for_exit();
    synchronous.assert_closed();
    assert_eq!(std::fs::read(&path).unwrap(), cancelled);
    assert_eq!(watcher_wake_count(&path), 1);
}

#[test]
#[ignore = "requires local Unix socket access"]
fn sigterm_before_mcp_initialize_exits_with_stdin_still_open() {
    let mut mcp = McpFixture::uninitialized();
    let status_path = format!("/proc/{}/status", mcp.child.id());
    let deadline = Instant::now() + LIMIT;
    loop {
        let status =
            std::fs::read_to_string(&status_path).expect("read owned MCP child's signal state");
        let caught = status
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .expect("Linux SigCgt field");
        let mask = u64::from_str_radix(caught.trim(), 16).expect("parse caught-signal mask");
        if mask & (1 << 14) != 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "MCP installs SIGTERM handler before initialization"
        );
        thread::sleep(Duration::from_millis(1));
    }
    mcp.signal(rustix::process::Signal::TERM);
    mcp.wait_for_exit();
    assert!(
        mcp.stdin.is_some(),
        "client stdin stays open throughout signal shutdown"
    );
}

#[test]
#[ignore = "requires local Unix socket access"]
fn explicit_false_wait_still_blocks_and_returns_the_synchronous_result() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    mcp.call(3, "computer_wait", wait_arguments(1, false));
    let waiting = mcp.daemon_call();
    assert!(matches!(
        mcp.responses.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    waiting.respond(ResponseResult::Ok(DaemonResponse::Wait(
        WaitOutcome::Timeout { elapsed_ms: 77 },
    )));
    let response = mcp.response(3);
    assert_eq!(
        response["result"]["structuredContent"],
        json!({
            "status": "timeout", "elapsed_ms": 77, "frame": 1, "settled": true
        })
    );
    assert_eq!(response["result"]["content"].as_array().unwrap().len(), 1);
    assert!(!mcp.directory.path().join("computer-use/mcp-waits").exists());
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn desktop_operations_require_an_explicit_connection() {
    let mut mcp = McpFixture::initialized();
    for (id, name, arguments) in [
        (2, "computer_observe", json!({})),
        (
            3,
            "computer_act",
            json!({"frame": 1, "actions": [{"type": "keypress", "keys": ["A"]}]}),
        ),
        (4, "computer_wait", wait_arguments(1, false)),
        (5, "computer_wait", wait_arguments(1, true)),
    ] {
        mcp.call(id, name, arguments);
        assert_eq!(
            mcp.response(id)["result"]["structuredContent"]["code"],
            "not_connected"
        );
    }
    assert!(
        mcp.listener.accept().is_err(),
        "unconnected MCP contacted a daemon"
    );
    for (id, arguments) in [
        (6, json!({"name": null})),
        (7, json!({"type": null})),
        (8, json!({"name": ""})),
    ] {
        mcp.call(id, "computer_connect", arguments);
        let response = mcp.response(id);
        assert!(response.get("error").is_some() || response["result"]["isError"] == true);
    }
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn omitted_and_explicit_default_names_connect_to_the_default_daemon() {
    let mut mcp = McpFixture::initialized();
    for (id, params) in [
        (2, json!({"name": "computer_connect", "arguments": {}})),
        (
            3,
            json!({"name": "computer_connect", "arguments": {"name": "@default"}}),
        ),
        (4, json!({"name": "computer_connect"})),
    ] {
        mcp.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        let profile = mcp.daemon_call();
        assert!(matches!(profile.request.request, DaemonRequest::Profile));
        profile.respond(ResponseResult::Ok(DaemonResponse::Profile(Some(
            "default daemon".to_owned(),
        ))));
        assert_eq!(
            mcp.response(id)["result"]["structuredContent"],
            json!({"profile": "default daemon"})
        );
        assert!(
            !mcp.directory
                .path()
                .join("computer-use/mcp-desktops")
                .exists()
        );
    }
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn invalid_connect_requests_preserve_connection_frame_and_pending_wait() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let signal = mcp.start_wait(3, 1);
    let waiting = mcp.daemon_call();
    for (index, arguments) in [
        json!({"instance": "default"}),
        json!({"type": "unknown"}),
        json!({"type": "private", "name": "work"}),
        json!({"type": "default", "name": null}),
        json!({"type": "named"}),
        json!({"type": "default"}),
        json!({"type": "private"}),
        json!({"type": "named", "name": "work"}),
        json!({"name": null}),
        json!({"name": ""}),
        json!({"name": "../escape"}),
        json!({"name": "."}),
        json!({"name": ".."}),
        json!({"name": "@unknown"}),
        json!({"name": "has/slash"}),
        json!({"name": "a".repeat(65)}),
        json!({"name": "@private", "extra": true}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 4 + u64::try_from(index).unwrap();
        mcp.call(id, "computer_connect", arguments);
        let response = mcp.response(id);
        assert_eq!(response["result"]["isError"], true);
        let message = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            message.contains("failed to deserialize parameters")
                || message.contains("invalid name")
        );
        assert!(!signal.exists(), "invalid connect cancelled the wait");
        assert!(
            mcp.listener.accept().is_err(),
            "invalid connect contacted a daemon"
        );
    }
    assert!(
        !mcp.directory
            .path()
            .join("computer-use/mcp-desktops")
            .exists()
    );
    waiting.respond(ResponseResult::Ok(DaemonResponse::Wait(
        WaitOutcome::Timeout { elapsed_ms: 1 },
    )));
    assert_eq!(terminal_result(&signal)["status"], "timeout");
    mcp.call(
        100,
        "computer_act",
        json!({"frame": 1, "actions": [{"type": "keypress", "keys": ["A"]}]}),
    );
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Act(
            mcp.action_outcome("acted"),
        )));
    assert_eq!(
        mcp.response(100)["result"]["structuredContent"]["observation"]["frame"],
        2
    );
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn named_default_and_private_are_independent_daemon_names() {
    for name in ["default", "private"] {
        let mut mcp = McpFixture::new();
        mcp.call(2, "computer_connect", json!({"name": name}));
        let error = mcp.response(2);
        assert_eq!(
            error["result"]["structuredContent"]["code"],
            "daemon_unavailable"
        );
        let message = error["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap();
        assert!(message.contains(&format!("cu daemon --instance {name}")));
        assert!(!message.contains("name @private"));
        assert!(mcp.listener.accept().is_err());

        let default_listener = mcp.replace_listener(name);
        mcp.call(3, "computer_connect", json!({"name": name}));
        let profile = mcp.daemon_call();
        assert!(matches!(profile.request.request, DaemonRequest::Profile));
        profile.respond(ResponseResult::Ok(DaemonResponse::Profile(Some(format!(
            "named {name}"
        )))));
        let response = mcp.response(3);
        assert_eq!(
            response["result"]["structuredContent"]["profile"],
            format!("named {name}")
        );
        assert!(default_listener.accept().is_err());
        assert!(
            !mcp.directory
                .path()
                .join("computer-use/mcp-desktops")
                .exists()
        );

        let named_listener = std::mem::replace(&mut mcp.listener, default_listener);
        mcp.connect(4, "@default");
        assert!(named_listener.accept().is_err());
        mcp.close();
    }
}

#[test]
#[ignore = "requires local Unix socket access"]
fn switching_instances_invalidates_old_frames_and_cached_actions() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "same-internal-id");
    let action = json!({"frame": 1, "actions": [{"type": "keypress", "keys": ["A"]}]});
    mcp.call(3, "computer_act", action.clone());
    let old_action = mcp.daemon_call();
    let old_request_id = old_action.request.request_id.clone();
    old_action.respond(ResponseResult::Ok(DaemonResponse::Act(
        mcp.action_outcome("action-frame"),
    )));
    assert_eq!(
        mcp.response(3)["result"]["structuredContent"]["observation"]["frame"],
        2
    );
    let old_listener = mcp.replace_listener("other");
    mcp.connect(4, "other");
    mcp.call(3, "computer_act", action);
    assert_eq!(
        mcp.response(3)["result"]["structuredContent"]["code"],
        "stale_frame"
    );
    assert!(
        mcp.listener.accept().is_err(),
        "old action was replayed against the new daemon"
    );
    mcp.call(5, "computer_observe", json!({}));
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Observe(
            mcp.image_observation("same-internal-id"),
        )));
    assert_eq!(mcp.response(5)["result"]["structuredContent"]["frame"], 3);
    mcp.call(
        3,
        "computer_act",
        json!({"frame": 3, "actions": [{"type": "keypress", "keys": ["B"]}]}),
    );
    let action = mcp.daemon_call();
    assert_ne!(action.request.request_id, old_request_id);
    action.respond(ResponseResult::Ok(DaemonResponse::Act(
        mcp.action_outcome("new-action"),
    )));
    assert_eq!(
        mcp.response(3)["result"]["structuredContent"]["observation"]["frame"],
        4
    );
    assert!(old_listener.accept().is_err());
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn reconnect_cancels_async_wait_and_requires_another_observation() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    let signal = mcp.start_wait(3, 1);
    let waiting = mcp.daemon_call();
    mcp.connect(4, "@default");
    waiting.assert_closed();
    let cancelled = terminal_result(&signal);
    assert_eq!(cancelled["status"], "cancelled");
    mcp.call(5, "computer_wait", wait_arguments(1, false));
    assert_eq!(
        mcp.response(5)["result"]["structuredContent"]["code"],
        "stale_frame"
    );
    mcp.observe(6, "next");
    assert_eq!(terminal_result(&signal), cancelled);
    let next = mcp.start_wait(7, 2);
    mcp.daemon_call()
        .respond(ResponseResult::Ok(DaemonResponse::Wait(
            WaitOutcome::Timeout { elapsed_ms: 1 },
        )));
    assert_eq!(terminal_result(&next)["status"], "timeout");
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn connect_actively_cancels_a_synchronous_wait_on_an_unchanged_daemon() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    mcp.call(3, "computer_wait", wait_arguments(1, false));
    let waiting = mcp.daemon_call();
    mcp.call(4, "computer_connect", json!({}));
    let profile = mcp.daemon_call();
    waiting.assert_closed();
    profile.respond(ResponseResult::Ok(DaemonResponse::Profile(Some(
        "reconnected".to_owned(),
    ))));
    let responses = mcp.responses_for(&[3, 4]);
    assert_eq!(
        responses[&3]["result"]["structuredContent"]["code"],
        "cancelled"
    );
    assert_eq!(
        responses[&4]["result"]["structuredContent"]["profile"],
        "reconnected"
    );
    mcp.observe(5, "next");
    mcp.close();
}

#[test]
#[ignore = "requires local Unix socket access"]
fn failed_or_cancelled_connect_stays_disconnected() {
    let mut mcp = McpFixture::new();
    mcp.observe(2, "baseline");
    mcp.call(3, "computer_connect", json!({"name": "missing"}));
    assert_eq!(
        mcp.response(3)["result"]["structuredContent"]["code"],
        "daemon_unavailable"
    );
    mcp.call(4, "computer_observe", json!({}));
    assert_eq!(
        mcp.response(4)["result"]["structuredContent"]["code"],
        "not_connected"
    );
    assert!(mcp.listener.accept().is_err());
    mcp.call(5, "computer_connect", json!({"name": "@default"}));
    let pending = mcp.daemon_call();
    mcp.send(
        &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5}}),
    );
    pending.assert_closed();
    // rmcp permits a cancellation to suppress its tool response.
    let _ = mcp.responses.recv_timeout(Duration::from_millis(100));
    mcp.call(6, "computer_observe", json!({}));
    assert_eq!(
        mcp.response(6)["result"]["structuredContent"]["code"],
        "not_connected"
    );
    assert!(mcp.listener.accept().is_err());
    mcp.close();
}

struct McpFixture {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    listener: UnixListener,
    directory: tempfile::TempDir,
    image: PathBuf,
}

impl McpFixture {
    fn connect(&mut self, id: u64, name: &str) {
        self.call(id, "computer_connect", json!({"name": name}));
        let profile = self.daemon_call();
        assert!(matches!(profile.request.request, DaemonRequest::Profile));
        profile.respond(ResponseResult::Ok(DaemonResponse::Profile(Some(format!(
            "profile for {name}"
        )))));
        let response = self.response(id);
        assert_eq!(
            response["result"]["structuredContent"],
            json!({"profile": format!("profile for {name}")})
        );
    }

    fn replace_listener(&mut self, name: &str) -> UnixListener {
        let directory = self
            .directory
            .path()
            .join("computer-use/instances")
            .join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let listener = UnixListener::bind(directory.join("cu.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::mem::replace(&mut self.listener, listener)
    }

    fn responses_for(&self, ids: &[u64]) -> std::collections::HashMap<u64, Value> {
        let mut results = std::collections::HashMap::new();
        while results.len() < ids.len() {
            let response = self.responses.recv_timeout(LIMIT).expect("MCP response");
            if let Some(id) = response["id"].as_u64() {
                assert!(ids.contains(&id));
                assert!(results.insert(id, response).is_none());
            }
        }
        results
    }

    fn new() -> Self {
        let mut fixture = Self::initialized();
        fixture.call(0, "computer_connect", json!({}));
        let profile = fixture.daemon_call();
        assert!(matches!(profile.request.request, DaemonRequest::Profile));
        profile.respond(ResponseResult::Ok(DaemonResponse::Profile(None)));
        assert_eq!(
            fixture.response(0)["result"]["structuredContent"],
            json!({"profile": ""})
        );
        fixture
    }

    fn initialized() -> Self {
        let mut fixture = Self::uninitialized();
        fixture.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "async-wait-test", "version": "1.0"}
            }
        }));
        assert_eq!(fixture.response(1)["result"]["serverInfo"]["name"], "cu");
        fixture.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        fixture
    }

    fn uninitialized() -> Self {
        let directory = tempfile::TempDir::new().expect("create test runtime");
        let runtime = directory.path().join("computer-use");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = runtime.join("cu.sock");
        let image = directory.path().join("frame.png");
        std::fs::write(&image, [1, 2, 3]).unwrap();
        let listener = UnixListener::bind(&socket).expect("bind mock daemon");
        listener.set_nonblocking(true).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_cu"))
            .arg("mcp")
            .env("XDG_RUNTIME_DIR", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn cu mcp");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(response) = serde_json::from_str(&line) else {
                    break;
                };
                if sender.send(response).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            responses,
            listener,
            directory,
            image,
        }
    }

    fn send(&mut self, value: &Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{value}").expect("write MCP request");
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) {
        let mut request = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name}
        });
        request["params"]["arguments"] = arguments;
        self.send(&request);
    }

    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + LIMIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let response = self
                .responses
                .recv_timeout(remaining)
                .expect("MCP response before deadline");
            if response.get("id").is_some() {
                assert_eq!(response["id"], id, "unexpected MCP response: {response}");
                return response;
            }
        }
    }

    fn daemon_call(&self) -> DaemonCall {
        let deadline = Instant::now() + LIMIT;
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "daemon request before deadline");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept mock daemon request: {error}"),
            }
        };
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .expect("read daemon request");
        let request = serde_json::from_str(&line).expect("parse daemon request");
        DaemonCall { stream, request }
    }

    fn observe(&mut self, id: u64, frame_id: &str) {
        self.call(id, "computer_observe", json!({}));
        let observing = self.daemon_call();
        assert!(matches!(
            observing.request.request,
            DaemonRequest::Observe(_)
        ));
        observing.respond(ResponseResult::Ok(DaemonResponse::Observe(
            self.image_observation(frame_id),
        )));
        assert!(self.response(id)["result"]["structuredContent"]["frame"].is_u64());
    }

    fn start_wait(&mut self, id: u64, frame: u64) -> PathBuf {
        self.call(id, "computer_wait", wait_arguments(frame, true));
        let response = self.response(id);
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["status"], "started", "{response}");
        assert_eq!(result.as_object().unwrap().len(), 3);
        assert!(result["message"].as_str().unwrap().contains("monitor"));
        assert!(result["message"].as_str().unwrap().contains("JSON result"));
        assert_eq!(response["result"]["content"].as_array().unwrap().len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(
                response["result"]["content"][0]["text"].as_str().unwrap()
            )
            .unwrap(),
            *result
        );
        PathBuf::from(
            result["signal_path"]
                .as_str()
                .expect("generated signal path"),
        )
    }

    fn image_observation(&self, frame_id: &str) -> Observation {
        self.observation(frame_id, &self.image)
    }

    #[allow(clippy::unused_self)]
    fn observation(&self, frame_id: &str, image: &Path) -> Observation {
        Observation {
            frame_id: frame_id.to_owned(),
            target: "mock:screen".to_owned(),
            width: 100,
            height: 80,
            coordinate_space: CoordinateSpace::FramePixels,
            settled: true,
            image_path: image.to_string_lossy().into_owned(),
        }
    }

    fn action_outcome(&self, frame_id: &str) -> ActOutcome {
        ActOutcome {
            status: ActStatus::Ok,
            executed: 1,
            action_error: None,
            image_expired: false,
            observation: Some(self.image_observation(frame_id)),
        }
    }

    fn signal(&self, signal: rustix::process::Signal) {
        let pid = rustix::process::Pid::from_raw(i32::try_from(self.child.id()).unwrap()).unwrap();
        rustix::process::kill_process(pid, signal).expect("signal owned MCP child");
    }

    fn close(&mut self) {
        self.stdin.take();
        self.wait_for_exit();
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll MCP child") {
                assert!(status.success(), "MCP child exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "MCP shutdown exceeded deadline");
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for McpFixture {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct DaemonCall {
    stream: UnixStream,
    request: RequestEnvelope,
}

impl DaemonCall {
    fn respond(mut self, result: ResponseResult) {
        let response = ResponseEnvelope {
            request_id: self.request.request_id,
            result,
        };
        writeln!(self.stream, "{}", serde_json::to_string(&response).unwrap())
            .expect("send daemon response");
    }

    fn assert_closed(mut self) {
        let mut byte = [0_u8; 1];
        assert_eq!(
            self.stream
                .read(&mut byte)
                .expect("wait for daemon connection closure"),
            0
        );
    }
}

fn wait_arguments(frame: u64, asynchronous: bool) -> Value {
    json!({
        "frame": frame, "async": asynchronous,
        "include_rects": [{"x": 0, "y": 0, "width": 100, "height": 80}],
        "timeout_ms": 60_000, "quiet_ms": 1
    })
}

fn terminal_result(path: &Path) -> Value {
    let deadline = Instant::now() + LIMIT;
    loop {
        match std::fs::read(path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).expect("published file is complete JSON");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    Instant::now() < deadline,
                    "terminal result before deadline: {}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("read terminal file: {error}"),
        }
    }
}

fn watcher_wake_count(path: &Path) -> usize {
    usize::from(matches!(
        terminal_result(path)["status"].as_str(),
        Some("changed" | "timeout" | "error" | "cancelled")
    ))
}

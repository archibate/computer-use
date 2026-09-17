use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cu_protocol::{
    DaemonRequest, DaemonResponse, RequestEnvelope, ResponseEnvelope, ResponseResult,
};
use rustix::process::{Pid, Signal};
use serde_json::{Value, json};
use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, PropMode, WindowClass},
    rust_connection::{DefaultStream, RustConnection},
    wrapper::ConnectionExt as _,
};

const LIMIT: Duration = Duration::from_secs(20);

#[test]
#[ignore = "requires Xvfb, Openbox and local Unix sockets"]
fn private_desktops_are_authenticated_reusable_and_operable() {
    let mut first = Mcp::new(None);
    let desktop = first.connect();
    assert!(RustConnection::connect_to_stream(desktop.stream(), 0).is_err());
    assert_eq!(
        fs::metadata(&desktop.auth).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let x = desktop.x11();
    let screen = &x.setup().roots[0];
    assert_eq!(
        (screen.width_in_pixels, screen.height_in_pixels),
        (1280, 800)
    );
    let blue = window(&x, "cu test: blue", 0x0028_5d9b);
    until(|| x.get_input_focus().unwrap().reply().unwrap().focus == blue);
    first.key(&["SUPER", "LEFT"]);
    until(|| x.get_geometry(blue).unwrap().reply().unwrap().width == 638);
    let green = window(&x, "cu test: green", 0x003b_8361);
    until(|| x.get_input_focus().unwrap().reply().unwrap().focus == green);
    first.key(&["SUPER", "RIGHT"]);
    until(|| {
        x.translate_coordinates(green, screen.root, 0, 0)
            .unwrap()
            .reply()
            .unwrap()
            .dst_x
            == 641
    });
    first.key(&["ALT", "TAB"]);
    until(|| x.get_input_focus().unwrap().reply().unwrap().focus == blue);
    first.key(&["SUPER", "UP"]);
    until(|| x.get_geometry(blue).unwrap().reply().unwrap().width == 1280);
    first.key(&["SUPER", "LEFT"]);
    first.snapshot();

    let original = desktop.profile;
    assert_eq!(first.connect().profile, original);
    let listener = UnixListener::bind(first.runtime.path().join("computer-use/cu.sock")).unwrap();
    let public = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let request: RequestEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(request.request, DaemonRequest::Profile));
        let response = ResponseEnvelope {
            request_id: request.request_id,
            result: ResponseResult::Ok(DaemonResponse::Profile(Some("public desktop".to_owned()))),
        };
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
    });
    assert_eq!(
        first.call("computer_connect", json!({"name":"@default"}))["structuredContent"]["profile"],
        "public desktop"
    );
    public.join().unwrap();
    assert_eq!(first.connect().profile, original);
    assert!(x.get_geometry(blue).unwrap().reply().is_ok());
    // A failed switch leaves the private desktop alive and the session disconnected.
    assert_eq!(
        first.call("computer_connect", json!({"name":"missing"}))["isError"],
        true
    );
    assert_eq!(
        first.call("computer_observe", json!({}))["structuredContent"]["code"],
        "not_connected"
    );
    assert_eq!(first.connect().profile, original);
    assert!(x.get_geometry(blue).unwrap().reply().is_ok());

    let mut second = Mcp::new(None);
    let other = second.connect();
    assert_ne!(other.auth, desktop.auth);
    assert_ne!(other.socket, desktop.socket);
    let wrong_cookie = fs::read(&desktop.auth).unwrap();
    assert!(
        RustConnection::connect_to_stream_with_auth_info(
            other.stream(),
            0,
            b"MIT-MAGIC-COOKIE-1".to_vec(),
            wrong_cookie[wrong_cookie.len() - 16..].to_vec(),
        )
        .is_err()
    );
    drop(x);
    first.close();
    assert!(!desktop.auth.exists());
    assert_eq!(
        second.call("computer_observe", json!({}))["structuredContent"]["width"],
        1280
    );
    second.close();
    assert!(!other.auth.exists());
}

#[test]
#[ignore = "requires Xvfb, Openbox and process signals"]
fn replaced_runtime_directories_survive_shutdown_and_reconnect() {
    for reconnect in [false, true] {
        let mut mcp = Mcp::new(None);
        let desktop = mcp.connect();
        let directory = desktop.auth.parent().unwrap();
        fs::rename(directory, directory.with_extension("moved")).unwrap();
        fs::create_dir(directory).unwrap();
        let sentinel = directory.join("unrelated");
        fs::write(&sentinel, "keep").unwrap();
        if reconnect {
            let worker = children(mcp.child.id())[0];
            signal(worker, Signal::KILL);
            until(|| !alive(worker));
            assert_ne!(mcp.connect().auth, desktop.auth);
        }
        mcp.close();
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    }
}

#[test]
#[ignore = "requires Xvfb, Openbox and process signals"]
fn private_desktop_recovers_from_component_loss_and_reaps_on_owner_loss() {
    for target in ["eof", "mcp-term", "mcp-kill", "worker", "Xvfb", "openbox"] {
        let mut mcp = Mcp::new(None);
        let desktop = mcp.connect();
        let worker = children(mcp.child.id());
        assert_eq!(worker.len(), 1);
        let components = children(worker[0]);
        assert_eq!(components.len(), 2, "{target}: {components:?}");
        let owned: Vec<_> = worker.iter().chain(&components).copied().collect();
        match target {
            "eof" => mcp.close(),
            "mcp-term" => signal(mcp.child.id(), Signal::TERM),
            "mcp-kill" => signal(mcp.child.id(), Signal::KILL),
            "worker" => signal(worker[0], Signal::KILL),
            name => {
                let pid = components
                    .iter()
                    .find(|pid| {
                        fs::read_to_string(format!("/proc/{pid}/comm"))
                            .unwrap()
                            .trim()
                            == name
                    })
                    .unwrap();
                signal(*pid, Signal::KILL);
            }
        }
        until(|| owned.iter().all(|pid| !alive(*pid)));
        if matches!(target, "worker" | "Xvfb" | "openbox") {
            assert_eq!(mcp.call("computer_observe", json!({}))["isError"], true);
            let replacement = mcp.connect();
            assert_ne!(desktop.auth, replacement.auth);
            assert!(!desktop.auth.exists());
            mcp.close();
        } else {
            until(|| mcp.child.try_wait().unwrap().is_some());
            assert!(!desktop.auth.exists());
        }
    }
}

#[test]
#[ignore = "requires Xvfb and local Unix sockets"]
fn failed_timed_out_and_cancelled_startup_roll_back() {
    for mode in [
        "missing-all",
        "missing-openbox",
        "timeout",
        "cancel",
        "eof",
        "kill",
    ] {
        let bin = tempfile::TempDir::new().unwrap();
        if mode == "missing-openbox" {
            std::os::unix::fs::symlink("/usr/bin/Xvfb", bin.path().join("Xvfb")).unwrap();
        } else if matches!(mode, "timeout" | "cancel" | "eof" | "kill") {
            let fake = bin.path().join("Xvfb");
            fs::write(&fake, "#!/bin/sh\nexec /usr/bin/sleep 30\n").unwrap();
            fs::set_permissions(fake, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut mcp = Mcp::new(Some(bin.path()));
        let id = mcp.start_call("computer_connect", json!({"name":"@private"}));
        let owned = if matches!(mode, "timeout" | "cancel" | "eof" | "kill") {
            until(|| !children(mcp.child.id()).is_empty());
            let worker = children(mcp.child.id())[0];
            until(|| !children(worker).is_empty());
            let mut owned = children(worker);
            owned.push(worker);
            owned
        } else {
            Vec::new()
        };
        if matches!(mode, "eof" | "kill") {
            if mode == "kill" {
                mcp.child.kill().unwrap();
            }
            mcp.close();
            until(|| owned.iter().all(|pid| !alive(*pid)));
        } else if mode == "cancel" {
            mcp.send(&json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}));
            // rmcp may suppress the cancelled request's response. Teardown and
            // the next call, rather than that optional response, prove rollback.
            until(|| owned.iter().all(|pid| !alive(*pid)));
        } else {
            let result = mcp.response(id);
            assert_eq!(result["result"]["isError"], true, "{mode}: {result}");
        }
        if !matches!(mode, "eof" | "kill") {
            assert_eq!(
                mcp.call("computer_observe", json!({}))["structuredContent"]["code"],
                "not_connected"
            );
        }
        until(|| owned.iter().all(|pid| !alive(*pid)));
        assert_eq!(
            fs::read_dir(mcp.runtime.path().join("computer-use/mcp-desktops"))
                .unwrap()
                .count(),
            0
        );
        mcp.close();
    }
}

struct Desktop {
    profile: String,
    auth: PathBuf,
    socket: PathBuf,
}

impl Desktop {
    fn stream(&self) -> DefaultStream {
        DefaultStream::from_unix_stream(UnixStream::connect(&self.socket).unwrap())
            .unwrap()
            .0
    }

    fn x11(&self) -> RustConnection {
        let record = fs::read(&self.auth).unwrap();
        RustConnection::connect_to_stream_with_auth_info(
            self.stream(),
            0,
            b"MIT-MAGIC-COOKIE-1".to_vec(),
            record[record.len() - 16..].to_vec(),
        )
        .unwrap()
    }
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    runtime: tempfile::TempDir,
    next_id: u64,
}

impl Mcp {
    fn new(path: Option<&Path>) -> Self {
        let runtime = tempfile::TempDir::new().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cu"));
        command
            .arg("mcp")
            .env("XDG_RUNTIME_DIR", runtime.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(serde_json::from_str(&line).unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Self {
            child,
            stdin,
            responses,
            runtime,
            next_id: 1,
        };
        mcp.send(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"desktop-test","version":"1"}}}));
        assert!(mcp.response(0)["result"].is_object());
        mcp.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        mcp
    }

    fn send(&mut self, value: &Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{value}").unwrap();
    }

    fn start_call(&mut self, name: &str, arguments: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut request =
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name}});
        request["params"]["arguments"] = arguments;
        self.send(&request);
        id
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.start_call(name, arguments);
        self.response(id)["result"].take()
    }

    fn response(&self, id: u64) -> Value {
        loop {
            let value = self
                .responses
                .recv_timeout(LIMIT)
                .expect("MCP response deadline");
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    fn connect(&mut self) -> Desktop {
        let result = self.call("computer_connect", json!({"name":"@private"}));
        assert_ne!(result["isError"], true, "{result}");
        let profile = result["structuredContent"]["profile"]
            .as_str()
            .unwrap()
            .to_owned();
        let display: String = serde_json::from_str(
            profile
                .split("display=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap(),
        )
        .unwrap();
        let auth: PathBuf = serde_json::from_str(
            profile
                .lines()
                .find_map(|line| line.strip_prefix("XAUTHORITY="))
                .unwrap(),
        )
        .unwrap();
        Desktop {
            profile,
            auth,
            socket: PathBuf::from(format!(
                "/tmp/.X11-unix/X{}",
                display.strip_prefix(':').unwrap()
            )),
        }
    }

    fn key(&mut self, keys: &[&str]) {
        let frame = self.call("computer_observe", json!({}))["structuredContent"]["frame"].clone();
        let result = self.call(
            "computer_act",
            json!({"frame":frame,"actions":[{"type":"keypress","keys":keys}]}),
        );
        assert_ne!(result["isError"], true, "{result}");
    }

    fn snapshot(&mut self) {
        let result = self.call("computer_observe", json!({}));
        if let Some(directory) = std::env::var_os("CU_VISUAL_QA_DIR") {
            let data = result["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|content| content["type"] == "image")
                .unwrap()["data"]
                .as_str()
                .unwrap();
            fs::write(
                Path::new(&directory).join("cu-private-desktop.png"),
                STANDARD.decode(data).unwrap(),
            )
            .unwrap();
        }
    }

    fn close(&mut self) {
        self.stdin.take();
        until(|| self.child.try_wait().unwrap().is_some());
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn window(x: &RustConnection, name: &str, color: u32) -> u32 {
    let screen = &x.setup().roots[0];
    let id = x.generate_id().unwrap();
    x.create_window(
        screen.root_depth,
        id,
        screen.root,
        100,
        100,
        400,
        300,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().background_pixel(color),
    )
    .unwrap()
    .check()
    .unwrap();
    x.change_property8(
        PropMode::REPLACE,
        id,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        name.as_bytes(),
    )
    .unwrap();
    x.map_window(id).unwrap();
    x.flush().unwrap();
    id
}

fn children(pid: u32) -> Vec<u32> {
    fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .flat_map(|task| {
            fs::read_to_string(task.path().join("children"))
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.split_once(") ").unwrap().1.starts_with('Z'))
}

fn signal(pid: u32, signal: Signal) {
    rustix::process::kill_process(Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(), signal)
        .unwrap();
}

fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !predicate() {
        assert!(Instant::now() < deadline, "desktop test deadline");
        thread::sleep(Duration::from_millis(20));
    }
}

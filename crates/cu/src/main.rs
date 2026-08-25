mod backend;
mod client;
mod daemon;
mod mcp;

use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result};
use backend::{BackendChoice, BackendOptions};
use clap::{Parser, Subcommand};
use cu_core::{CaptureLimits, Engine};
use cu_protocol::{ActRequest, DaemonRequest, ObserveRequest, RequestEnvelope};
use uuid::Uuid;

const ACT_LONG_HELP: &str = "\
INPUT JSON
  expected_frame_id  Latest frame_id returned by `cu observe` or a prior action.
  actions            1 to 16 sequential action objects.
  settle             Optional visual-settling policy for the resulting frame.

Coordinates are integer pixels in the referenced frame: x is in [0,width) and
y is in [0,height). Batch actions only when no intermediate inspection is needed.

Run `cu act --schema` for the complete JSON Schema, including action variants,
key names, coordinate conventions, defaults, and limits.

EXAMPLE
  printf '%s\\n' '{\"expected_frame_id\":\"<latest frame_id>\",\"actions\":[{\"type\":\"click\",\"x\":800,\"y\":450}]}' | cu act";

#[derive(Debug, Parser)]
#[command(
    name = "cu",
    about = "Grounded computer-use daemon, CLI, and MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the single-owner desktop session daemon.
    Daemon {
        /// Select a desktop backend from the session, or require one explicitly.
        #[arg(long, value_enum, default_value_t)]
        backend: BackendChoice,
        /// Target this exact X11 display; also selects X11 in auto mode.
        #[arg(
            long,
            alias = "x11-display",
            value_name = "DISPLAY",
            conflicts_with = "output"
        )]
        display: Option<String>,
        /// Require this direct Wayland output instead of accepting the detected one.
        #[arg(long)]
        output: Option<String>,
        /// Downscale captures to at most this width; native width when omitted.
        #[arg(long)]
        max_width: Option<u32>,
        /// Downscale captures to at most this height; native height when omitted.
        #[arg(long)]
        max_height: Option<u32>,
        #[arg(long, default_value_t = 32)]
        max_frames: usize,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        frame_dir: Option<PathBuf>,
    },
    /// Capture a settled frame and print its JSON metadata.
    Observe {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Execute a frame-grounded JSON action batch read from stdin.
    #[command(after_long_help = ACT_LONG_HELP)]
    Act {
        /// Print the accepted action-input JSON Schema and exit.
        #[arg(long, conflicts_with_all = ["socket", "request_id"])]
        schema: bool,
        /// Connect to this daemon socket instead of the owner-only default.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Stable retry key for session-scoped action idempotency.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Serve `computer_observe` and `computer_act` over MCP stdio.
    Mcp {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon {
            backend,
            display,
            output,
            max_width,
            max_height,
            max_frames,
            socket,
            frame_dir,
        } => {
            let runtime = if socket.is_none() || frame_dir.is_none() {
                let runtime = default_runtime_dir();
                secure_runtime_dir(&runtime)?;
                Some(runtime)
            } else {
                None
            };
            let socket = match socket {
                Some(socket) => socket,
                None => runtime
                    .as_ref()
                    .expect("default runtime resolved")
                    .join("cu.sock"),
            };
            let frame_dir = match frame_dir {
                Some(frame_dir) => frame_dir,
                None => runtime
                    .as_ref()
                    .expect("default runtime resolved")
                    .join("frames"),
            };
            let capture_limits = CaptureLimits {
                max_width,
                max_height,
            };
            let started = backend::start(&BackendOptions {
                choice: backend,
                display,
                output,
                capture_limits,
            })?;
            let engine = Engine::new(started.desktop, frame_dir, max_frames)?;
            eprintln!("computer-use daemon targeting {}", started.target);
            eprintln!("computer-use daemon listening at {}", socket.display());
            serve_until_shutdown(socket, engine).await
        }
        Command::Observe { socket } => {
            let response = client::request(
                &resolve_socket(socket),
                &RequestEnvelope {
                    request_id: Uuid::new_v4().to_string(),
                    request: DaemonRequest::Observe(ObserveRequest::default()),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
        Command::Act {
            schema,
            socket,
            request_id,
        } => {
            if schema {
                println!("{}", render_act_schema()?);
                return Ok(());
            }
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .context("failed to read action JSON from stdin")?;
            let act = serde_json::from_str::<ActRequest>(&input)
                .context("stdin is not valid action JSON; run `cu act --schema` for the format")?;
            let response = client::request(
                &resolve_socket(socket),
                &RequestEnvelope {
                    request_id: request_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                    request: DaemonRequest::Act(act),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
        Command::Mcp { socket } => mcp::serve(resolve_socket(socket)).await,
    }
}

fn render_act_schema() -> Result<String> {
    serde_json::to_string_pretty(&schemars::schema_for!(ActRequest))
        .context("failed to render action-input JSON Schema")
}

async fn serve_until_shutdown(socket: PathBuf, engine: Engine) -> Result<()> {
    tokio::select! {
        result = daemon::serve(socket, engine) => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl+C")?;
            eprintln!("computer-use daemon shutting down");
            Ok(())
        }
    }
}

fn default_runtime_dir() -> PathBuf {
    default_runtime_dir_from(
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        rustix::process::geteuid().as_raw(),
    )
}

fn default_runtime_dir_from(xdg_runtime_dir: Option<&OsStr>, effective_uid: u32) -> PathBuf {
    let root = xdg_runtime_dir.filter(|path| !path.is_empty()).map_or_else(
        || PathBuf::from("/run/user").join(effective_uid.to_string()),
        PathBuf::from,
    );
    root.join("computer-use")
}

fn resolve_socket(socket: Option<PathBuf>) -> PathBuf {
    socket.unwrap_or_else(|| default_runtime_dir().join("cu.sock"))
}

fn secure_runtime_dir(path: &PathBuf) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create runtime directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure runtime directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn daemon_needs_no_display_arguments() {
        let cli = Cli::try_parse_from(["cu", "daemon"]).unwrap();
        let Command::Daemon {
            backend,
            display,
            output,
            max_width,
            max_height,
            ..
        } = cli.command
        else {
            panic!("expected daemon command");
        };

        assert_eq!(backend, BackendChoice::Auto);
        assert_eq!(display, None);
        assert_eq!(output, None);
        assert_eq!(max_width, None);
        assert_eq!(max_height, None);
    }

    #[test]
    fn daemon_accepts_an_explicit_x11_display() {
        let cli = Cli::try_parse_from(["cu", "daemon", "--display", ":99"]).unwrap();
        let Command::Daemon { display, .. } = cli.command else {
            panic!("expected daemon command");
        };

        assert_eq!(display.as_deref(), Some(":99"));
    }

    #[test]
    fn daemon_rejects_x11_display_with_a_wayland_output() {
        assert!(
            Cli::try_parse_from(["cu", "daemon", "--display", ":99", "--output", "HDMI-A-1",])
                .is_err()
        );
    }

    #[test]
    fn runtime_dir_prefers_the_xdg_session_path() {
        assert_eq!(
            default_runtime_dir_from(Some(OsStr::new("/tmp/session-runtime")), 1234),
            PathBuf::from("/tmp/session-runtime/computer-use")
        );
    }

    #[test]
    fn runtime_dir_falls_back_to_the_effective_users_linux_runtime_path() {
        let expected = PathBuf::from("/run/user/1234/computer-use");
        assert_eq!(default_runtime_dir_from(None, 1234), expected);
        assert_eq!(
            default_runtime_dir_from(Some(OsStr::new("")), 1234),
            expected
        );
    }

    #[test]
    fn act_help_explains_json_without_leaking_rust_identifiers() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("act")
            .expect("act subcommand")
            .render_long_help()
            .to_string();

        assert!(!help.contains("ActRequest"));
        for required in [
            "frame-grounded JSON action batch",
            "expected_frame_id",
            "1 to 16 sequential action objects",
            "[0,width)",
            "cu act --schema",
        ] {
            assert!(help.contains(required), "act help omits {required:?}");
        }
    }

    #[test]
    fn act_schema_flag_prints_the_machine_readable_contract() {
        let cli = Cli::try_parse_from(["cu", "act", "--schema"]).unwrap();
        let Command::Act { schema, .. } = cli.command else {
            panic!("expected act command");
        };
        assert!(schema);
        assert!(Cli::try_parse_from(["cu", "act", "--schema", "--request-id", "unused"]).is_err());

        let schema: serde_json::Value =
            serde_json::from_str(&render_act_schema().unwrap()).unwrap();
        assert_eq!(schema["title"], "cu action input");
        assert_eq!(schema["properties"]["actions"]["minItems"], 1);
        assert_eq!(schema["properties"]["actions"]["maxItems"], 16);
        assert!(
            schema["properties"]["expected_frame_id"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("stale_frame"))
        );
    }
}

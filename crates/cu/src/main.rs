mod client;
mod daemon;
mod mcp;

use std::{
    env, fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cu_backend_niri::{CaptureLimits, NiriBackend};
use cu_core::Engine;
use cu_protocol::{ActRequest, DaemonRequest, ObserveRequest, RequestEnvelope};
use uuid::Uuid;

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
        /// Require this output name instead of accepting the detected one.
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
    /// Read an `ActRequest` JSON object from stdin and print the response.
    Act {
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
            output,
            max_width,
            max_height,
            max_frames,
            socket,
            frame_dir,
        } => {
            let runtime = if socket.is_none() || frame_dir.is_none() {
                let runtime = default_runtime_dir()?;
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
            let backend = NiriBackend::new(
                output.as_deref(),
                CaptureLimits {
                    max_width,
                    max_height,
                },
            )?;
            let output_name = backend.output_name().to_owned();
            let engine = Engine::new(Box::new(backend), frame_dir, max_frames)?;
            eprintln!("computer-use daemon targeting {output_name} at native capture size");
            eprintln!("computer-use daemon listening at {}", socket.display());
            daemon::serve(socket, engine).await
        }
        Command::Observe { socket } => {
            let response = client::request(
                &resolve_socket(socket)?,
                &RequestEnvelope {
                    request_id: Uuid::new_v4().to_string(),
                    request: DaemonRequest::Observe(ObserveRequest::default()),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
        Command::Act { socket, request_id } => {
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .context("failed to read ActRequest from stdin")?;
            let act = serde_json::from_str::<ActRequest>(&input)
                .context("stdin is not a valid ActRequest")?;
            let response = client::request(
                &resolve_socket(socket)?,
                &RequestEnvelope {
                    request_id: request_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                    request: DaemonRequest::Act(act),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
        Command::Mcp { socket } => mcp::serve(resolve_socket(socket)?).await,
    }
}

fn default_runtime_dir() -> Result<PathBuf> {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("computer-use"))
        .context("XDG_RUNTIME_DIR is unset; set it or pass explicit path options")
}

fn resolve_socket(socket: Option<PathBuf>) -> Result<PathBuf> {
    match socket {
        Some(socket) => Ok(socket),
        None => Ok(default_runtime_dir()?.join("cu.sock")),
    }
}

fn secure_runtime_dir(path: &PathBuf) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create runtime directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure runtime directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_needs_no_display_arguments() {
        let cli = Cli::try_parse_from(["cu", "daemon"]).unwrap();
        let Command::Daemon {
            output,
            max_width,
            max_height,
            ..
        } = cli.command
        else {
            panic!("expected daemon command");
        };

        assert_eq!(output, None);
        assert_eq!(max_width, None);
        assert_eq!(max_height, None);
    }
}

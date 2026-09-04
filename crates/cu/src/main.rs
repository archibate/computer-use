mod backend;
mod client;
mod daemon;
mod mcp;

use std::{
    env,
    ffi::OsStr,
    fmt, fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use backend::{BackendChoice, BackendOptions};
use clap::{Parser, Subcommand};
use cu_core::{CaptureLimits, Engine, MIN_RETAINED_FRAMES};
use cu_protocol::{ActRequest, DaemonRequest, ObserveRequest, RequestEnvelope};
use uuid::Uuid;

const MAX_PROFILE_BYTES: usize = 16 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstanceName(String);

impl Default for InstanceName {
    fn default() -> Self {
        Self("default".to_owned())
    }
}

impl fmt::Display for InstanceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for InstanceName {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 64
            || matches!(value, "." | "..")
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(
                "instance must be 1-64 ASCII letters, digits, dots, underscores, or hyphens"
                    .to_owned(),
            );
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonPaths {
    socket: PathBuf,
    frame_dir: PathBuf,
    instance_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one independently named desktop session daemon.
    Daemon {
        /// Named socket and frame-store namespace; defaults to `default`.
        #[arg(long, value_name = "NAME")]
        instance: Option<InstanceName>,
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
        /// Maximum retained frames for this instance; must preserve grounding and result frames.
        #[arg(
            long,
            default_value_t = 32,
            value_parser = parse_max_frames
        )]
        max_frames: usize,
        /// Trusted UTF-8 Markdown or text appended to MCP initialization instructions.
        #[arg(long, value_name = "PATH")]
        profile: Option<PathBuf>,
        /// Raw socket override; requires `--frame-dir` and conflicts with `--instance`.
        #[arg(long, conflicts_with = "instance", requires = "frame_dir")]
        socket: Option<PathBuf>,
        /// Raw frame-store override; requires `--socket` and conflicts with `--instance`.
        #[arg(long, conflicts_with = "instance", requires = "socket")]
        frame_dir: Option<PathBuf>,
    },
    /// Capture a settled frame and print its JSON metadata.
    Observe {
        /// Connect to this named instance; defaults to `default`.
        #[arg(long, value_name = "NAME", conflicts_with = "socket")]
        instance: Option<InstanceName>,
        /// Connect to this raw socket instead of a named instance.
        #[arg(long, conflicts_with = "instance")]
        socket: Option<PathBuf>,
    },
    /// Execute a frame-grounded JSON action batch read from stdin.
    #[command(after_long_help = ACT_LONG_HELP)]
    Act {
        /// Print the accepted action-input JSON Schema and exit.
        #[arg(long, conflicts_with_all = ["instance", "socket", "request_id"])]
        schema: bool,
        /// Connect to this named instance; defaults to `default`.
        #[arg(long, value_name = "NAME", conflicts_with = "socket")]
        instance: Option<InstanceName>,
        /// Connect to this daemon socket instead of the owner-only default.
        #[arg(long, conflicts_with = "instance")]
        socket: Option<PathBuf>,
        /// Stable retry key for session-scoped action idempotency.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Serve `computer_observe` and `computer_act` over MCP stdio.
    Mcp {
        /// Connect to this named instance; defaults to `default`.
        #[arg(long, value_name = "NAME", conflicts_with = "socket")]
        instance: Option<InstanceName>,
        /// Connect to this raw socket instead of a named instance.
        #[arg(long, conflicts_with = "instance")]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon {
            instance,
            backend,
            display,
            output,
            max_width,
            max_height,
            max_frames,
            profile,
            socket,
            frame_dir,
        } => {
            let profile = profile.as_deref().map(read_profile).transpose()?;
            let paths = resolve_daemon_paths(instance, socket, frame_dir)?;
            if let Some(instance_dir) = &paths.instance_dir {
                secure_managed_instance_dir(instance_dir)?;
            }
            let bound = daemon::bind(paths.socket.clone()).await?;
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
            let engine =
                Engine::new(started.desktop, paths.frame_dir, max_frames)?.with_profile(profile);
            eprintln!("computer-use daemon targeting {}", started.target);
            eprintln!(
                "computer-use daemon listening at {}",
                paths.socket.display()
            );
            serve_until_shutdown(bound, engine).await
        }
        Command::Observe { instance, socket } => {
            let response = client::request(
                &resolve_client_socket(instance, socket)?,
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
            instance,
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
                &resolve_client_socket(instance, socket)?,
                &RequestEnvelope {
                    request_id: request_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                    request: DaemonRequest::Act(act),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&response)?);
            Ok(())
        }
        Command::Mcp { instance, socket } => {
            mcp::serve(resolve_client_socket(instance, socket)?).await
        }
    }
}

fn render_act_schema() -> Result<String> {
    serde_json::to_string_pretty(&schemars::schema_for!(ActRequest))
        .context("failed to render action-input JSON Schema")
}

fn read_profile(path: &Path) -> Result<String> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open desktop profile {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read desktop profile {}", path.display()))?;
    if bytes.len() > MAX_PROFILE_BYTES {
        bail!("desktop profile must be at most {MAX_PROFILE_BYTES} bytes");
    }
    let profile = String::from_utf8(bytes)
        .with_context(|| format!("desktop profile {} is not UTF-8", path.display()))?;
    let profile = profile.trim();
    if profile.is_empty() {
        bail!("desktop profile must not be empty");
    }
    Ok(profile.to_owned())
}

fn parse_max_frames(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|error| format!("invalid frame count: {error}"))?;
    if value < MIN_RETAINED_FRAMES {
        return Err(format!("max-frames must be at least {MIN_RETAINED_FRAMES}"));
    }
    Ok(value)
}

async fn serve_until_shutdown(bound: daemon::BoundSocket, engine: Engine) -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to listen for SIGTERM")?;
    tokio::select! {
        result = daemon::serve(bound, engine) => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl+C")?;
            eprintln!("computer-use daemon shutting down");
            Ok(())
        }
        signal = terminate.recv() => {
            signal.context("SIGTERM listener closed unexpectedly")?;
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

fn resolve_daemon_paths(
    instance: Option<InstanceName>,
    socket: Option<PathBuf>,
    frame_dir: Option<PathBuf>,
) -> Result<DaemonPaths> {
    match (instance, socket, frame_dir) {
        (instance, None, None) => {
            let instance_dir = named_instance_dir(&instance.unwrap_or_default());
            Ok(DaemonPaths {
                socket: instance_dir.join("cu.sock"),
                frame_dir: instance_dir.join("frames"),
                instance_dir: Some(instance_dir),
            })
        }
        (None, Some(socket), Some(frame_dir)) => {
            if socket.starts_with(&frame_dir) || frame_dir.starts_with(&socket) {
                bail!("--socket and --frame-dir must identify separate resources");
            }
            Ok(DaemonPaths {
                socket,
                frame_dir,
                instance_dir: None,
            })
        }
        (Some(_), Some(_), Some(_)) => {
            bail!("--instance cannot be combined with raw --socket and --frame-dir overrides")
        }
        (_, Some(_), None) => {
            bail!("daemon --socket requires --frame-dir; use --instance for a managed namespace")
        }
        (_, None, Some(_)) => {
            bail!("daemon --frame-dir requires --socket; use --instance for a managed namespace")
        }
    }
}

fn resolve_client_socket(
    instance: Option<InstanceName>,
    socket: Option<PathBuf>,
) -> Result<PathBuf> {
    match (instance, socket) {
        (Some(_), Some(_)) => bail!("--instance cannot be combined with --socket"),
        (None, Some(socket)) => Ok(socket),
        (instance, None) => Ok(named_instance_dir(&instance.unwrap_or_default()).join("cu.sock")),
    }
}

fn named_instance_dir(instance: &InstanceName) -> PathBuf {
    let runtime = default_runtime_dir();
    if instance.0 == "default" {
        runtime
    } else {
        runtime.join("instances").join(&instance.0)
    }
}

fn secure_managed_instance_dir(path: &Path) -> Result<()> {
    let runtime = default_runtime_dir();
    secure_runtime_dir(&runtime)?;
    if path != runtime {
        secure_runtime_dir(&runtime.join("instances"))?;
        secure_runtime_dir(path)?;
    }
    Ok(())
}

fn secure_runtime_dir(path: &Path) -> Result<()> {
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
    fn daemon_accepts_a_desktop_profile_path() {
        let cli = Cli::try_parse_from(["cu", "daemon", "--profile", "/tmp/desktop.md"]).unwrap();
        let Command::Daemon { profile, .. } = cli.command else {
            panic!("expected daemon command");
        };

        assert_eq!(profile, Some(PathBuf::from("/tmp/desktop.md")));
    }

    #[test]
    fn profile_reader_accepts_bounded_utf8_and_rejects_invalid_input() {
        let directory = tempfile::TempDir::new().unwrap();
        let valid = directory.path().join("profile.md");
        fs::write(&valid, "  # Desktop\n\nSuper+Left tiles left.\n").unwrap();
        assert_eq!(
            read_profile(&valid).unwrap(),
            "# Desktop\n\nSuper+Left tiles left."
        );

        let empty = directory.path().join("empty.txt");
        fs::write(&empty, " \n").unwrap();
        assert!(read_profile(&empty).is_err());

        let oversized = directory.path().join("oversized.txt");
        fs::write(&oversized, vec![b'x'; MAX_PROFILE_BYTES + 1]).unwrap();
        assert!(read_profile(&oversized).is_err());

        let invalid = directory.path().join("invalid.txt");
        fs::write(&invalid, [0xff]).unwrap();
        assert!(read_profile(&invalid).is_err());
    }

    #[test]
    fn daemon_rejects_x11_display_with_a_wayland_output() {
        assert!(
            Cli::try_parse_from(["cu", "daemon", "--display", ":99", "--output", "HDMI-A-1",])
                .is_err()
        );
    }

    #[test]
    fn named_instances_have_distinct_runtime_resources() {
        let first = resolve_daemon_paths(Some("x11-99".parse().unwrap()), None, None).unwrap();
        let second = resolve_daemon_paths(Some("x11-100".parse().unwrap()), None, None).unwrap();

        assert_ne!(first.socket, second.socket);
        assert_ne!(first.frame_dir, second.frame_dir);
        assert!(first.socket.ends_with("instances/x11-99/cu.sock"));
        assert!(first.frame_dir.ends_with("instances/x11-99/frames"));
    }

    #[test]
    fn default_instance_preserves_the_legacy_runtime_paths() {
        let paths = resolve_daemon_paths(None, None, None).unwrap();
        let runtime = default_runtime_dir();

        assert_eq!(paths.socket, runtime.join("cu.sock"));
        assert_eq!(paths.frame_dir, runtime.join("frames"));
    }

    #[test]
    fn raw_daemon_paths_must_be_supplied_as_a_pair() {
        assert!(resolve_daemon_paths(None, Some(PathBuf::from("/tmp/a.sock")), None).is_err());
        assert!(resolve_daemon_paths(None, None, Some(PathBuf::from("/tmp/a-frames"))).is_err());
        assert!(Cli::try_parse_from(["cu", "daemon", "--socket", "/tmp/a.sock"]).is_err());
        assert!(
            resolve_daemon_paths(
                None,
                Some(PathBuf::from("/tmp/store/cu.sock")),
                Some(PathBuf::from("/tmp/store")),
            )
            .is_err()
        );
    }

    #[test]
    fn instance_names_are_single_safe_path_segments() {
        for valid in ["default", "x11-99", "wayland.HDMI-A-1", "a_b"] {
            assert!(valid.parse::<InstanceName>().is_ok(), "rejected {valid}");
        }
        for invalid in ["", ".", "..", "../escape", "has/slash", "空"] {
            assert!(
                invalid.parse::<InstanceName>().is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn instance_and_raw_socket_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from([
                "cu",
                "observe",
                "--instance",
                "x11-99",
                "--socket",
                "/tmp/a.sock",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "cu",
                "daemon",
                "--instance",
                "x11-99",
                "--socket",
                "/tmp/a.sock",
                "--frame-dir",
                "/tmp/a-frames",
            ])
            .is_err()
        );
    }

    #[test]
    fn daemon_requires_two_retained_frames() {
        assert!(Cli::try_parse_from(["cu", "daemon", "--max-frames", "1"]).is_err());
        assert!(Cli::try_parse_from(["cu", "daemon", "--max-frames", "2"]).is_ok());
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

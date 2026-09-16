use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use cu_core::{CaptureLimits, Engine};
use rustix::process::{Pid, Signal};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
};
use uuid::Uuid;

use crate::{
    DaemonPaths,
    backend::{self, BackendChoice, BackendOptions},
    daemon,
};

pub const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const OPENBOX_CONFIG: &str = include_str!("desktop/openbox.xml");
const OPENBOX_PROFILE: &str = include_str!("desktop/profile.md");

#[derive(Default)]
struct Control {
    closed: bool,
    worker: Option<Worker>,
}

struct Worker {
    child: Child,
    directory: PathBuf,
    identity: (u64, u64),
}

/// Ownership stays outside the MCP frame mutex, including during transport failure.
#[derive(Clone, Default)]
pub struct PrivateDesktop(Arc<Mutex<Control>>);

impl PrivateDesktop {
    pub fn launch(&self) -> Result<(PathBuf, bool)> {
        let mut control = self.0.lock().expect("desktop mutex poisoned");
        if control.closed {
            bail!("MCP session is closing");
        }
        if let Some(worker) = &mut control.worker
            && worker.child.try_wait()?.is_none()
        {
            return Ok((worker.directory.join("cu.sock"), false));
        }
        if let Some(worker) = control.worker.take()
            && let Err(error) = remove_directory(&worker.directory, worker.identity)
        {
            eprintln!("failed to clean stopped private desktop: {error:#}");
        }
        let runtime = crate::default_runtime_dir();
        private_directory(&runtime)?;
        let desktops = runtime.join("mcp-desktops");
        private_directory(&desktops)?;
        let executable = env::current_exe()?;
        let directory = desktops.join(Uuid::new_v4().to_string());
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let identity = directory_identity(&directory)?;
        let socket = directory.join("cu.sock");
        let child = Command::new(executable)
            .args(["daemon", "--offscreen-worker", "--socket"])
            .arg(&socket)
            .arg("--frame-dir")
            .arg(directory.join("frames"))
            .env("XAUTHORITY", directory.join("Xauthority"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env("XDG_SESSION_TYPE", "x11")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        match child {
            Ok(child) => {
                control.worker = Some(Worker {
                    child,
                    directory,
                    identity,
                });
                Ok((socket, true))
            }
            Err(error) => {
                let _ = remove_directory(&directory, identity);
                Err(error).context("failed to start private desktop worker")
            }
        }
    }

    pub fn running(&self) -> Result<bool> {
        let mut control = self.0.lock().expect("desktop mutex poisoned");
        if control.closed {
            return Ok(false);
        }
        control.worker.as_mut().map_or(Ok(false), |worker| {
            worker
                .child
                .try_wait()
                .map(|status| status.is_none())
                .map_err(Into::into)
        })
    }

    pub fn stop(&self) {
        let mut control = self.0.lock().expect("desktop mutex poisoned");
        control.closed = true;
        if let Some(worker) = &mut control.worker {
            // The worker observes EOF even if the MCP is killed or a handler is stuck.
            worker.child.stdin.take();
        }
    }

    pub async fn reset(&self) {
        let worker = self.0.lock().expect("desktop mutex poisoned").worker.take();
        if let Some(mut worker) = worker {
            worker.child.stdin.take();
            // Allow the worker to reap both children before escalating.
            if tokio::time::timeout(STOP_TIMEOUT * 3, worker.child.wait())
                .await
                .is_err()
            {
                let _ = worker.child.kill().await;
            }
            let _ = remove_directory(&worker.directory, worker.identity);
        }
    }

    pub async fn shutdown(&self) {
        self.stop();
        self.reset().await;
    }
}

fn private_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o777 != 0o700
            {
                bail!(
                    "private desktop directory must be an owner-only real directory: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn write_auth(path: &Path) -> Result<()> {
    // FamilyWild and an empty display number allow atomic Xvfb display allocation.
    let mut record = Vec::from(u16::MAX.to_be_bytes());
    let cookie = Uuid::new_v4();
    for field in [
        b"".as_slice(),
        b"".as_slice(),
        b"MIT-MAGIC-COOKIE-1".as_slice(),
        cookie.as_bytes(),
    ] {
        record.extend_from_slice(&u16::try_from(field.len())?.to_be_bytes());
        record.extend_from_slice(field);
    }
    write_private(path, &record)
}

/// Run before creating a Tokio runtime, then replace this process with the child.
pub fn exec_child(parent: u32, command: &[OsString]) -> Result<()> {
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
    if u32::try_from(Pid::as_raw(rustix::process::getppid()))? != parent {
        bail!("desktop owner exited before child startup");
    }
    let (program, arguments) = command
        .split_first()
        .context("missing desktop child program")?;
    Err(std::process::Command::new(program).args(arguments).exec()).with_context(|| {
        format!(
            "failed to launch {}; install Xvfb and Openbox",
            program.display()
        )
    })
}

pub fn notify_ready() -> Result<()> {
    let path = env::var_os("CU_DESKTOP_READY").context("missing desktop ready path")?;
    write_private(Path::new(&path), b"ready\n")
}

fn child_command(program: &str, directory: &Path) -> Result<Command> {
    let mut command = Command::new(env::current_exe()?);
    command
        .args([
            "__desktop-child",
            "--parent",
            &std::process::id().to_string(),
            "--",
            program,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .env("XAUTHORITY", directory.join("Xauthority"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("XDG_SESSION_TYPE", "x11")
        .process_group(0)
        .kill_on_drop(true);
    Ok(command)
}

#[derive(Default)]
struct DesktopProcesses {
    xvfb: Option<Child>,
    openbox: Option<Child>,
}

impl DesktopProcesses {
    async fn start(&mut self, directory: &Path) -> Result<String> {
        let auth = directory.join("Xauthority");
        write_auth(&auth)?;
        write_private(&directory.join("openbox.xml"), OPENBOX_CONFIG.as_bytes())?;
        let mut command = child_command("Xvfb", directory)?;
        command
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
                "-noreset",
                "-auth",
            ])
            .arg(&auth)
            .stdout(Stdio::piped());
        self.xvfb = Some(command.spawn().context("failed to start Xvfb")?);
        let stdout = self
            .xvfb
            .as_mut()
            .expect("just started Xvfb")
            .stdout
            .take()
            .context("Xvfb stdout missing")?;
        let mut reader = BufReader::new(stdout.take(32));
        let mut number = String::new();
        reader
            .read_line(&mut number)
            .await
            .context("failed to read Xvfb display")?;
        let number: u16 = number
            .trim()
            .parse()
            .context("Xvfb did not report a display; check that Xvfb is installed")?;
        let display = format!(":{number}");
        let executable = env::current_exe()?.to_string_lossy().into_owned();
        let startup = format!("'{}' __desktop-ready", executable.replace('\'', "'\\''"));
        let mut command = child_command("openbox", directory)?;
        command
            .args(["--sm-disable", "--config-file"])
            .arg(directory.join("openbox.xml"))
            .args(["--startup", &startup])
            .env("DISPLAY", &display)
            .env("CU_DESKTOP_READY", directory.join("ready"));
        self.openbox = Some(command.spawn().context("failed to start Openbox")?);
        loop {
            self.check()?;
            if directory.join("ready").is_file() {
                return Ok(display);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn check(&mut self) -> Result<()> {
        for (name, child) in [("Xvfb", &mut self.xvfb), ("Openbox", &mut self.openbox)] {
            if let Some(child) = child
                && let Some(status) = child.try_wait()?
            {
                bail!("{name} exited ({status}); install Xvfb and Openbox if unavailable");
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        for child in [&mut self.openbox, &mut self.xvfb].into_iter().flatten() {
            if let Some(pid) = child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, Signal::TERM);
            }
            if tokio::time::timeout(STOP_TIMEOUT, child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
}

pub async fn serve(paths: DaemonPaths, limits: CaptureLimits, max_frames: usize) -> Result<()> {
    let directory = paths
        .socket
        .parent()
        .context("private socket has no directory")?
        .to_owned();
    validate_worker_directory(&directory)?;
    if paths.socket != directory.join("cu.sock") || paths.frame_dir != directory.join("frames") {
        bail!("offscreen worker resources must belong to its private directory");
    }
    private_directory(&directory)?;
    let identity = directory_identity(&directory)?;
    if env::var_os("XAUTHORITY").as_deref() != Some(directory.join("Xauthority").as_os_str()) {
        bail!("offscreen worker requires its own XAUTHORITY environment");
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut stdin = tokio::io::stdin();
    let mut byte = [0_u8; 1];
    let mut children = DesktopProcesses::default();
    let work = async {
        let display = tokio::time::timeout(START_TIMEOUT, children.start(&directory))
            .await
            .context("private desktop startup timed out")??;
        let bound = daemon::bind(paths.socket).await?;
        let started = backend::start(&BackendOptions {
            choice: BackendChoice::X11,
            display: Some(display),
            output: None,
            capture_limits: limits,
        })?;
        let profile = format!(
            "Desktop connection: {}\nXAUTHORITY={}\n\n{}",
            started.target,
            serde_json::to_string(&directory.join("Xauthority"))?,
            OPENBOX_PROFILE
        );
        let engine =
            Engine::new(started.desktop, paths.frame_dir, max_frames)?.with_profile(Some(profile));
        let serving = daemon::serve(bound, engine);
        tokio::pin!(serving);
        loop {
            tokio::select! {
                result = &mut serving => return result,
                () = tokio::time::sleep(Duration::from_millis(100)) => children.check()?,
            }
        }
    };
    let result = tokio::select! {
        result = work => result,
        result = stdin.read(&mut byte) => result.map(|_| ()).context("private owner pipe failed"),
        _ = terminate.recv() => Ok(()),
        result = tokio::signal::ctrl_c() => result.context("private signal handler failed"),
    };
    children.shutdown().await;
    // Also clean after owner death, when there is no parent left to do this.
    let _ = remove_directory(&directory, identity);
    result
}

fn directory_identity(path: &Path) -> std::io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn remove_directory(path: &Path, identity: (u64, u64)) -> Result<()> {
    validate_worker_directory(path)?;
    match directory_identity(path) {
        Ok(actual) if actual != identity => bail!("private desktop directory identity changed"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to clean private desktop"),
    }
}

fn validate_worker_directory(path: &Path) -> Result<()> {
    if path.parent() != Some(crate::default_runtime_dir().join("mcp-desktops").as_path())
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| Uuid::parse_str(name).ok())
            .is_none()
    {
        bail!("offscreen worker requires a managed MCP desktop directory");
    }
    Ok(())
}

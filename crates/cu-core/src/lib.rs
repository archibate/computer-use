use std::{
    collections::{HashMap, VecDeque},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use cu_protocol::{
    ActOutcome, ActStatus, CoordinateSpace, CuError, DaemonRequest, DaemonResponse, ErrorCode,
    Observation, RequestEnvelope, ResponseEnvelope, ResponseResult, SettlePolicy, Viewport,
    validate_act_request, validate_settle_policy,
};
use uuid::Uuid;

pub const MIN_RETAINED_FRAMES: usize = 2;
const MAX_CACHED_ACTIONS: usize = 64;
const FRAME_STORE_MARKER: &str = ".cu-frames";
const FRAME_STORE_VERSION: &str = "cu-frames 1\n";

/// Kernel-released exclusive ownership of one filesystem resource path.
pub struct ResourceLease {
    _file: File,
}

impl ResourceLease {
    /// Acquire a non-blocking exclusive lease stored beside `resource`.
    ///
    /// Lock files are persistent inodes and must never be unlinked. The kernel
    /// releases the lease when this value or its process exits.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] when the resource parent cannot be resolved, the
    /// lock file cannot be opened, or another process already owns the lease.
    pub fn acquire(resource: &Path, kind: &str) -> Result<Self, CuError> {
        let lock_path = resource_lock_path(resource)?;
        let owned = rustix::fs::openat(
            rustix::fs::CWD,
            &lock_path,
            rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to open {kind} lease: {error}"),
            )
        })?;
        let mut file = File::from(owned);
        let metadata = file.metadata().map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to inspect {kind} lease: {error}"),
            )
        })?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.nlink() != 1
        {
            return Err(CuError::new(
                ErrorCode::Internal,
                format!("{kind} lease must be a singly linked regular file owned by this user"),
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to protect {kind} lease: {error}"),
                )
            })?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                if error == rustix::io::Errno::WOULDBLOCK {
                    CuError::new(
                        ErrorCode::LeaseConflict,
                        format!("another computer-use process owns the {kind}"),
                    )
                } else {
                    CuError::new(
                        ErrorCode::Internal,
                        format!("failed to acquire {kind} lease: {error}"),
                    )
                }
            },
        )?;
        let path_metadata = fs::symlink_metadata(&lock_path).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to verify {kind} lease path: {error}"),
            )
        })?;
        if path_metadata.file_type().is_symlink()
            || path_metadata.dev() != metadata.dev()
            || path_metadata.ino() != metadata.ino()
        {
            return Err(CuError::new(
                ErrorCode::Internal,
                format!("{kind} lease path changed during acquisition"),
            ));
        }
        file.set_len(0).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to reset {kind} lease metadata: {error}"),
            )
        })?;
        file.seek(SeekFrom::Start(0)).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to seek {kind} lease metadata: {error}"),
            )
        })?;
        writeln!(file, "pid={}", std::process::id()).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to write {kind} lease metadata: {error}"),
            )
        })?;
        Ok(Self { _file: file })
    }
}

fn resource_lock_path(resource: &Path) -> Result<PathBuf, CuError> {
    let parent = resource.parent().ok_or_else(|| {
        CuError::new(
            ErrorCode::Internal,
            "leased resource path has no parent directory",
        )
    })?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let canonical_parent = fs::canonicalize(parent).map_err(|error| {
        CuError::new(
            ErrorCode::Internal,
            format!("failed to resolve leased resource parent: {error}"),
        )
    })?;
    let name = resource.file_name().ok_or_else(|| {
        CuError::new(ErrorCode::Internal, "leased resource path has no file name")
    })?;
    let mut lock_name = OsString::from(".");
    lock_name.push(name);
    lock_name.push(".lock");
    Ok(canonical_parent.join(lock_name))
}

pub struct CapturedFrame {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub target: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureLimits {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

impl CaptureLimits {
    /// Reject zero-sized capture limits.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] when either configured limit is zero.
    pub fn validate(self) -> Result<(), CuError> {
        if self.max_width == Some(0) || self.max_height == Some(0) {
            return Err(CuError::new(
                ErrorCode::InvalidAction,
                "capture limits must be greater than zero",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn fit(self, width: u32, height: u32) -> Viewport {
        let mut fitted = Viewport { width, height };
        if width == 0 || height == 0 {
            return fitted;
        }
        if let Some(max_width) = self.max_width
            && fitted.width > max_width
        {
            fitted.height = scaled_dimension(fitted.height, max_width, fitted.width);
            fitted.width = max_width;
        }
        if let Some(max_height) = self.max_height
            && fitted.height > max_height
        {
            fitted.width = scaled_dimension(fitted.width, max_height, fitted.height);
            fitted.height = max_height;
        }
        fitted
    }
}

fn scaled_dimension(value: u32, numerator: u32, denominator: u32) -> u32 {
    let scaled = u64::from(value) * u64::from(numerator) / u64::from(denominator);
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

pub trait Desktop: Send {
    /// Capture the target as a PNG frame.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] when the target cannot be located or captured.
    fn capture(&mut self) -> Result<CapturedFrame, CuError>;

    /// Validate backend-specific action support before a batch starts.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] when the backend cannot execute the action.
    fn validate(&self, action: &cu_protocol::Action) -> Result<(), CuError>;

    /// Inject one validated input action into the target.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] when the input is unsupported or injection fails.
    fn execute(&mut self, action: &cu_protocol::Action, viewport: Viewport) -> Result<(), CuError>;
}

pub struct Engine {
    desktop: Box<dyn Desktop>,
    frames: FrameStore,
    session_id: Uuid,
    revision: u64,
    latest: Option<Observation>,
    completed_actions: HashMap<String, CachedAction>,
    cache_order: VecDeque<String>,
    max_cached_actions: usize,
}

#[derive(Clone)]
struct CachedAction {
    request: DaemonRequest,
    response: ResponseEnvelope,
}

impl Engine {
    /// Create a single-owner computer-use session engine.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] if the private frame store cannot be initialized or
    /// `max_frames` cannot preserve both the grounding and result frames.
    pub fn new(
        desktop: Box<dyn Desktop>,
        frame_dir: impl Into<PathBuf>,
        max_frames: usize,
    ) -> Result<Self, CuError> {
        Ok(Self {
            desktop,
            frames: FrameStore::new(frame_dir.into(), max_frames)?,
            session_id: Uuid::new_v4(),
            revision: 0,
            latest: None,
            completed_actions: HashMap::new(),
            cache_order: VecDeque::new(),
            max_cached_actions: MAX_CACHED_ACTIONS,
        })
    }

    #[must_use]
    pub fn latest(&self) -> Option<&Observation> {
        self.latest.as_ref()
    }

    pub fn handle(&mut self, envelope: RequestEnvelope) -> ResponseEnvelope {
        if let Some(cached) = self.completed_actions.get(&envelope.request_id) {
            if cached.request == envelope.request {
                let mut response = cached.response.clone();
                if let ResponseResult::Ok(DaemonResponse::Act(outcome)) = &mut response.result
                    && outcome
                        .observation
                        .as_ref()
                        .is_some_and(|observation| !self.frames.contains(&observation.image_path))
                {
                    outcome.observation = None;
                    outcome.image_expired = true;
                }
                return response;
            }
            return error_response(
                envelope.request_id,
                CuError::new(
                    ErrorCode::ProtocolError,
                    "request_id was reused with a different request",
                ),
            );
        }

        match &envelope.request {
            DaemonRequest::Observe(request) => {
                let result = validate_settle_policy(request.settle)
                    .and_then(|()| self.observe(request.settle))
                    .map(DaemonResponse::Observe);
                response_from_result(envelope.request_id.clone(), result)
            }
            DaemonRequest::Act(request) => {
                let response = response_from_result(
                    envelope.request_id.clone(),
                    self.act(request).map(DaemonResponse::Act),
                );
                self.cache_action(envelope.clone(), response.clone());
                response
            }
        }
    }

    fn observe(&mut self, settle: SettlePolicy) -> Result<Observation, CuError> {
        let (frame, settled) = self.capture_settled(settle)?;
        self.frames.reserve_one()?;
        self.persist_frame(frame, settled)
    }

    fn act(&mut self, request: &cu_protocol::ActRequest) -> Result<ActOutcome, CuError> {
        let latest = self.latest.as_ref().ok_or_else(|| {
            CuError::new(
                ErrorCode::StaleFrame,
                "observe the target before the first action",
            )
        })?;
        if request.expected_frame_id != latest.frame_id {
            return Err(CuError::new(
                ErrorCode::StaleFrame,
                format!(
                    "expected {}, but the latest frame is {}",
                    request.expected_frame_id, latest.frame_id
                ),
            ));
        }

        let viewport = latest.viewport();
        validate_act_request(request, viewport)?;
        for action in &request.actions {
            self.desktop.validate(action)?;
        }
        self.frames.reserve_one()?;

        let mut executed = 0;
        for action in &request.actions {
            if let Err(error) = self.desktop.execute(action, viewport) {
                if executed == 0 {
                    return Err(error);
                }
                let (frame, settled) = self.capture_settled(request.settle).map_err(|capture| {
                    CuError::new(
                        ErrorCode::Indeterminate,
                        format!(
                            "{executed} actions executed; action failed with {error}; post-failure capture also failed with {capture}"
                        ),
                    )
                    .with_executed(executed)
                })?;
                let observation = self.persist_frame(frame, settled).map_err(|publish| {
                    CuError::new(
                        ErrorCode::Indeterminate,
                        format!(
                            "{executed} actions executed; action failed with {error}; post-failure frame could not be published: {publish}"
                        ),
                    )
                    .with_executed(executed)
                })?;
                return Ok(ActOutcome {
                    status: ActStatus::Partial,
                    executed,
                    action_error: Some(error.with_executed(executed)),
                    image_expired: false,
                    observation: Some(observation),
                });
            }
            executed += 1;
        }

        let (frame, settled) = self.capture_settled(request.settle).map_err(|capture| {
            CuError::new(
                ErrorCode::Indeterminate,
                format!(
                    "all {executed} actions executed, but the resulting frame could not be captured: {capture}"
                ),
            )
            .with_executed(executed)
        })?;
        let observation = self.persist_frame(frame, settled).map_err(|publish| {
            CuError::new(
                ErrorCode::Indeterminate,
                format!(
                    "all {executed} actions executed, but the resulting frame could not be published: {publish}"
                ),
            )
            .with_executed(executed)
        })?;
        Ok(ActOutcome {
            status: ActStatus::Ok,
            executed,
            action_error: None,
            image_expired: false,
            observation: Some(observation),
        })
    }

    fn capture_settled(&mut self, policy: SettlePolicy) -> Result<(CapturedFrame, bool), CuError> {
        let deadline = Instant::now() + Duration::from_millis(policy.timeout_ms);
        let mut previous = self.desktop.capture()?;

        loop {
            thread::sleep(Duration::from_millis(policy.quiet_ms));
            let current = self.desktop.capture()?;
            if current.png == previous.png {
                return Ok((current, true));
            }
            previous = current;
            if Instant::now() >= deadline {
                return Ok((previous, false));
            }
        }
    }

    fn persist_frame(
        &mut self,
        frame: CapturedFrame,
        settled: bool,
    ) -> Result<Observation, CuError> {
        self.revision = self.revision.wrapping_add(1);
        let frame_id = format!("f_{}_{:016x}", self.session_id.simple(), self.revision);
        let image_path = self.frames.persist(&frame_id, &frame.png)?;
        let observation = Observation {
            frame_id,
            target: frame.target,
            width: frame.width,
            height: frame.height,
            coordinate_space: CoordinateSpace::FramePixels,
            settled,
            image_path: image_path.to_string_lossy().into_owned(),
        };
        self.latest = Some(observation.clone());
        Ok(observation)
    }

    fn cache_action(&mut self, envelope: RequestEnvelope, response: ResponseEnvelope) {
        self.cache_order.push_back(envelope.request_id.clone());
        self.completed_actions.insert(
            envelope.request_id,
            CachedAction {
                request: envelope.request,
                response,
            },
        );
        while self.cache_order.len() > self.max_cached_actions {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.completed_actions.remove(&oldest);
            }
        }
    }
}

struct FrameStore {
    directory: PathBuf,
    paths: VecDeque<PathBuf>,
    max_frames: usize,
    _lease: ResourceLease,
}

impl FrameStore {
    fn new(directory: PathBuf, max_frames: usize) -> Result<Self, CuError> {
        if max_frames < MIN_RETAINED_FRAMES {
            return Err(CuError::new(
                ErrorCode::Internal,
                format!("max_frames must be at least {MIN_RETAINED_FRAMES}"),
            ));
        }
        ensure_resource_parent(&directory)?;
        let lease = ResourceLease::acquire(&directory, "frame store")?;
        prepare_private_store_directory(&directory)?;
        prepare_store_marker(&directory)?;
        recover_managed_frames(&directory)?;
        Ok(Self {
            directory,
            paths: VecDeque::new(),
            max_frames,
            _lease: lease,
        })
    }

    fn reserve_one(&mut self) -> Result<(), CuError> {
        while self.paths.len() >= self.max_frames {
            let oldest = self.paths.front().expect("non-empty frame queue");
            match fs::remove_file(oldest) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CuError::new(
                        ErrorCode::Internal,
                        format!("failed to evict retained frame: {error}"),
                    ));
                }
            }
            self.paths.pop_front();
        }
        Ok(())
    }

    fn persist(&mut self, frame_id: &str, png: &[u8]) -> Result<PathBuf, CuError> {
        if self.paths.len() >= self.max_frames {
            return Err(CuError::new(
                ErrorCode::Internal,
                "frame capacity was not reserved before publication",
            ));
        }
        let final_path = self.directory.join(format!("{frame_id}.png"));
        let temporary_path = self.directory.join(format!(".{frame_id}.tmp"));
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)
            .map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to create captured PNG: {error}"),
                )
            })?;
        if let Err(error) = temporary.write_all(png) {
            drop(temporary);
            let _ = fs::remove_file(&temporary_path);
            return Err(CuError::new(
                ErrorCode::Internal,
                format!("failed to write captured PNG: {error}"),
            ));
        }
        drop(temporary);
        if let Err(error) = fs::rename(&temporary_path, &final_path) {
            let _ = fs::remove_file(&temporary_path);
            return Err(CuError::new(
                ErrorCode::Internal,
                format!("failed to publish captured PNG: {error}"),
            ));
        }

        self.paths.push_back(final_path.clone());
        Ok(final_path)
    }

    fn contains(&self, image_path: &str) -> bool {
        let path = Path::new(image_path);
        self.paths.iter().any(|retained| retained == path) && path.is_file()
    }
}

fn ensure_resource_parent(resource: &Path) -> Result<(), CuError> {
    let parent = resource.parent().ok_or_else(|| {
        CuError::new(
            ErrorCode::Internal,
            "frame store path has no parent directory",
        )
    })?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    if parent.exists() {
        return Ok(());
    }
    fs::create_dir_all(parent).map_err(|error| {
        CuError::new(
            ErrorCode::Internal,
            format!("failed to create frame-store parent: {error}"),
        )
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|error| {
        CuError::new(
            ErrorCode::Internal,
            format!("failed to protect frame-store parent: {error}"),
        )
    })
}

fn prepare_private_store_directory(directory: &Path) -> Result<(), CuError> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(CuError::new(
                    ErrorCode::Internal,
                    "frame store must be a real directory, not a file or symlink",
                ));
            }
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(CuError::new(
                    ErrorCode::Internal,
                    "frame store must be owned by the effective user",
                ));
            }
            if metadata.mode() & 0o077 != 0 {
                return Err(CuError::new(
                    ErrorCode::Internal,
                    "frame store must not be accessible by group or other users",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(directory).map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to create frame store: {error}"),
                )
            })?;
            set_private_directory_permissions(directory)
        }
        Err(error) => Err(CuError::new(
            ErrorCode::Internal,
            format!("failed to inspect frame store: {error}"),
        )),
    }
}

fn prepare_store_marker(directory: &Path) -> Result<(), CuError> {
    let marker = directory.join(FRAME_STORE_MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(CuError::new(
                    ErrorCode::Internal,
                    "frame-store marker is not a regular file",
                ));
            }
            let version = fs::read_to_string(&marker).map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to read frame-store marker: {error}"),
                )
            })?;
            if version != FRAME_STORE_VERSION {
                return Err(CuError::new(
                    ErrorCode::Internal,
                    "frame-store marker has an unsupported version",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for entry in fs::read_dir(directory).map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to inspect legacy frame store: {error}"),
                )
            })? {
                let entry = entry.map_err(|error| {
                    CuError::new(
                        ErrorCode::Internal,
                        format!("failed to inspect legacy frame entry: {error}"),
                    )
                })?;
                if !is_managed_frame_name(&entry.file_name()) {
                    return Err(CuError::new(
                        ErrorCode::Internal,
                        format!(
                            "refusing to adopt non-empty unmarked frame directory {}",
                            directory.display()
                        ),
                    ));
                }
                let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                    CuError::new(
                        ErrorCode::Internal,
                        format!("failed to inspect legacy frame: {error}"),
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(CuError::new(
                        ErrorCode::Internal,
                        "legacy frame entry must be a regular file",
                    ));
                }
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&marker)
                .map_err(|error| {
                    CuError::new(
                        ErrorCode::Internal,
                        format!("failed to create frame-store marker: {error}"),
                    )
                })?;
            file.write_all(FRAME_STORE_VERSION.as_bytes())
                .map_err(|error| {
                    CuError::new(
                        ErrorCode::Internal,
                        format!("failed to write frame-store marker: {error}"),
                    )
                })
        }
        Err(error) => Err(CuError::new(
            ErrorCode::Internal,
            format!("failed to inspect frame-store marker: {error}"),
        )),
    }
}

fn recover_managed_frames(directory: &Path) -> Result<(), CuError> {
    for entry in fs::read_dir(directory).map_err(|error| {
        CuError::new(
            ErrorCode::Internal,
            format!("failed to scan retained frames: {error}"),
        )
    })? {
        let entry = entry.map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to inspect retained frame: {error}"),
            )
        })?;
        let name = entry.file_name();
        if name == OsStr::new(FRAME_STORE_MARKER) {
            continue;
        }
        if is_managed_frame_name(&name) {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to inspect managed frame: {error}"),
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(CuError::new(
                    ErrorCode::Internal,
                    "managed frame entry must be a regular file",
                ));
            }
            fs::remove_file(entry.path()).map_err(|error| {
                CuError::new(
                    ErrorCode::Internal,
                    format!("failed to clean retained frame: {error}"),
                )
            })?;
        } else {
            eprintln!(
                "computer-use frame store ignored foreign entry {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn is_managed_frame_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let body = if let Some(body) = name
        .strip_prefix("f_")
        .and_then(|value| value.strip_suffix(".png"))
    {
        body
    } else if let Some(body) = name
        .strip_prefix(".f_")
        .and_then(|value| value.strip_suffix(".tmp"))
    {
        body
    } else {
        return false;
    };
    let Some((session, revision)) = body.split_once('_') else {
        return false;
    };
    is_hex_width(session, 32) && is_hex_width(revision, 16)
}

fn is_hex_width(value: &str, width: usize) -> bool {
    value.len() == width && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), CuError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        CuError::new(
            ErrorCode::Internal,
            format!("failed to protect directory {}: {error}", path.display()),
        )
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), CuError> {
    Ok(())
}

fn response_from_result(
    request_id: String,
    result: Result<DaemonResponse, CuError>,
) -> ResponseEnvelope {
    ResponseEnvelope {
        request_id,
        result: match result {
            Ok(response) => ResponseResult::Ok(response),
            Err(error) => ResponseResult::Error(error),
        },
    }
}

fn error_response(request_id: String, error: CuError) -> ResponseEnvelope {
    ResponseEnvelope {
        request_id,
        result: ResponseResult::Error(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::symlink,
        sync::{Arc, Mutex},
    };

    use cu_protocol::{ActRequest, Action, DaemonRequest, MouseButton, ObserveRequest};
    use tempfile::TempDir;

    use super::*;

    struct MockDesktop {
        captures: VecDeque<CapturedFrame>,
        executed: Arc<Mutex<Vec<Action>>>,
    }

    impl Desktop for MockDesktop {
        fn capture(&mut self) -> Result<CapturedFrame, CuError> {
            self.captures.pop_front().ok_or_else(|| {
                CuError::new(ErrorCode::CaptureFailed, "mock capture queue is empty")
            })
        }

        fn validate(&self, action: &Action) -> Result<(), CuError> {
            if matches!(
                action,
                Action::Keypress { keys } if keys.iter().any(|key| key == "UNSUPPORTED")
            ) {
                return Err(CuError::new(
                    ErrorCode::UnsupportedInput,
                    "mock unsupported key",
                ));
            }
            Ok(())
        }

        fn execute(&mut self, action: &Action, _viewport: Viewport) -> Result<(), CuError> {
            self.executed.lock().unwrap().push(action.clone());
            Ok(())
        }
    }

    fn frame(value: u8) -> CapturedFrame {
        CapturedFrame {
            png: vec![value],
            width: 100,
            height: 80,
            target: "mock:screen".to_owned(),
        }
    }

    fn engine(
        directory: &TempDir,
        captures: Vec<CapturedFrame>,
        executed: Arc<Mutex<Vec<Action>>>,
    ) -> Engine {
        engine_with_max(directory, captures, executed, 8)
    }

    fn engine_with_max(
        directory: &TempDir,
        captures: Vec<CapturedFrame>,
        executed: Arc<Mutex<Vec<Action>>>,
        max_frames: usize,
    ) -> Engine {
        Engine::new(
            Box::new(MockDesktop {
                captures: captures.into(),
                executed,
            }),
            directory.path().join("frames"),
            max_frames,
        )
        .unwrap()
    }

    fn observe(engine: &mut Engine) -> Observation {
        let response = engine.handle(RequestEnvelope {
            request_id: "observe-1".to_owned(),
            request: DaemonRequest::Observe(ObserveRequest::default()),
        });
        let ResponseResult::Ok(DaemonResponse::Observe(observation)) = response.result else {
            panic!("expected observation");
        };
        observation
    }

    #[test]
    fn action_is_grounded_in_latest_frame_and_returns_a_new_frame() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            &directory,
            vec![frame(1), frame(1), frame(2), frame(2)],
            Arc::clone(&executed),
        );
        let observation = observe(&mut engine);

        let response = engine.handle(RequestEnvelope {
            request_id: "act-1".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: observation.frame_id.clone(),
                actions: vec![Action::Click {
                    x: 20,
                    y: 30,
                    button: MouseButton::Left,
                    keys: Vec::new(),
                }],
                settle: SettlePolicy::default(),
            }),
        });

        let ResponseResult::Ok(DaemonResponse::Act(outcome)) = response.result else {
            panic!("expected action outcome");
        };
        assert_eq!(outcome.status, ActStatus::Ok);
        assert_eq!(outcome.executed, 1);
        assert_ne!(outcome.observation.unwrap().frame_id, observation.frame_id);
        assert_eq!(executed.lock().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_action_request_is_not_executed_twice() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(
            &directory,
            vec![frame(1), frame(1), frame(2), frame(2)],
            Arc::clone(&executed),
        );
        let observation = observe(&mut engine);
        let request = RequestEnvelope {
            request_id: "same-id".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: observation.frame_id,
                actions: vec![Action::Type {
                    text: "hello".to_owned(),
                }],
                settle: SettlePolicy::default(),
            }),
        };

        let first = engine.handle(request.clone());
        let second = engine.handle(request);

        assert_eq!(first, second);
        assert_eq!(executed.lock().unwrap().len(), 1);
    }

    #[test]
    fn stale_frame_is_rejected_before_input() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(&directory, vec![frame(1), frame(1)], Arc::clone(&executed));
        let _ = observe(&mut engine);

        let response = engine.handle(RequestEnvelope {
            request_id: "act-stale".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: "f_old".to_owned(),
                actions: vec![Action::Type {
                    text: "must not run".to_owned(),
                }],
                settle: SettlePolicy::default(),
            }),
        });

        let ResponseResult::Error(error) = response.result else {
            panic!("expected error");
        };
        assert_eq!(error.code, ErrorCode::StaleFrame);
        assert!(executed.lock().unwrap().is_empty());
    }

    #[test]
    fn backend_validates_the_entire_batch_before_input() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine(&directory, vec![frame(1), frame(1)], Arc::clone(&executed));
        let observation = observe(&mut engine);

        let response = engine.handle(RequestEnvelope {
            request_id: "act-unsupported".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: observation.frame_id,
                actions: vec![
                    Action::Click {
                        x: 20,
                        y: 30,
                        button: MouseButton::Left,
                        keys: Vec::new(),
                    },
                    Action::Keypress {
                        keys: vec!["UNSUPPORTED".to_owned()],
                    },
                ],
                settle: SettlePolicy::default(),
            }),
        });

        let ResponseResult::Error(error) = response.result else {
            panic!("expected unsupported input error");
        };
        assert_eq!(error.code, ErrorCode::UnsupportedInput);
        assert!(executed.lock().unwrap().is_empty());
    }

    #[test]
    fn frame_ids_are_unique_across_engine_sessions() {
        let first_directory = TempDir::new().unwrap();
        let second_directory = TempDir::new().unwrap();
        let mut first = engine(
            &first_directory,
            vec![frame(1), frame(1)],
            Arc::new(Mutex::new(Vec::new())),
        );
        let mut second = engine(
            &second_directory,
            vec![frame(1), frame(1)],
            Arc::new(Mutex::new(Vec::new())),
        );

        assert_ne!(observe(&mut first).frame_id, observe(&mut second).frame_id);
    }

    #[test]
    fn distinct_frame_stores_can_be_owned_concurrently() {
        let first_directory = TempDir::new().unwrap();
        let second_directory = TempDir::new().unwrap();
        let first = engine(
            &first_directory,
            Vec::new(),
            Arc::new(Mutex::new(Vec::new())),
        );
        let second = engine(
            &second_directory,
            Vec::new(),
            Arc::new(Mutex::new(Vec::new())),
        );

        drop(first);
        drop(second);
    }

    #[test]
    fn resource_lease_refuses_a_preplanted_symlink() {
        let directory = TempDir::new().unwrap();
        let resource = directory.path().join("frames");
        let lock_path = directory.path().join(".frames.lock");
        let outside = directory.path().join("outside");
        fs::write(&outside, "do not truncate").unwrap();
        symlink(&outside, &lock_path).unwrap();

        let result = ResourceLease::acquire(&resource, "frame store");

        let Err(error) = result else {
            panic!("symlinked lease unexpectedly succeeded");
        };
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(fs::read_to_string(outside).unwrap(), "do not truncate");
    }

    #[test]
    fn one_frame_store_rejects_a_second_live_owner() {
        let directory = TempDir::new().unwrap();
        let frame_dir = directory.path().join("frames");
        let first = Engine::new(
            Box::new(MockDesktop {
                captures: VecDeque::new(),
                executed: Arc::new(Mutex::new(Vec::new())),
            }),
            &frame_dir,
            8,
        )
        .unwrap();

        let second = Engine::new(
            Box::new(MockDesktop {
                captures: VecDeque::new(),
                executed: Arc::new(Mutex::new(Vec::new())),
            }),
            &frame_dir,
            8,
        );

        let Err(error) = second else {
            panic!("second frame-store owner unexpectedly succeeded");
        };
        assert_eq!(error.code, ErrorCode::LeaseConflict);
        drop(first);
    }

    #[test]
    fn restart_cleans_legacy_frames_and_temporary_files() {
        let directory = TempDir::new().unwrap();
        let frame_dir = directory.path().join("frames");
        fs::create_dir(&frame_dir).unwrap();
        fs::set_permissions(&frame_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let legacy = frame_dir.join("f_0123456789abcdef0123456789abcdef_0000000000000001.png");
        let temporary = frame_dir.join(".f_0123456789abcdef0123456789abcdef_0000000000000002.tmp");
        fs::write(&legacy, [1]).unwrap();
        fs::write(&temporary, [2]).unwrap();

        let engine = Engine::new(
            Box::new(MockDesktop {
                captures: VecDeque::new(),
                executed: Arc::new(Mutex::new(Vec::new())),
            }),
            &frame_dir,
            8,
        )
        .unwrap();

        assert!(!legacy.exists());
        assert!(!temporary.exists());
        assert_eq!(
            fs::read_to_string(frame_dir.join(FRAME_STORE_MARKER)).unwrap(),
            FRAME_STORE_VERSION
        );
        drop(engine);
    }

    #[test]
    fn legacy_recovery_never_follows_a_frame_shaped_symlink() {
        let directory = TempDir::new().unwrap();
        let frame_dir = directory.path().join("frames");
        let outside = directory.path().join("outside.png");
        fs::create_dir(&frame_dir).unwrap();
        fs::set_permissions(&frame_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&outside, [1]).unwrap();
        let disguised = frame_dir.join("f_0123456789abcdef0123456789abcdef_0000000000000001.png");
        symlink(&outside, &disguised).unwrap();

        let result = Engine::new(
            Box::new(MockDesktop {
                captures: VecDeque::new(),
                executed: Arc::new(Mutex::new(Vec::new())),
            }),
            &frame_dir,
            8,
        );

        let Err(error) = result else {
            panic!("symlinked legacy frame was unexpectedly adopted");
        };
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(outside.exists());
        assert!(disguised.is_symlink());
        assert!(!frame_dir.join(FRAME_STORE_MARKER).exists());
    }

    #[test]
    fn retention_keeps_only_the_newest_frames() {
        let directory = TempDir::new().unwrap();
        let mut engine = engine_with_max(
            &directory,
            vec![frame(1), frame(1), frame(2), frame(2), frame(3), frame(3)],
            Arc::new(Mutex::new(Vec::new())),
            2,
        );

        let first = observe(&mut engine);
        let second = observe(&mut engine);
        let third = observe(&mut engine);

        assert!(!Path::new(&first.image_path).exists());
        assert!(Path::new(&second.image_path).exists());
        assert!(Path::new(&third.image_path).exists());
    }

    #[test]
    fn externally_removed_frame_does_not_stall_retention() {
        let directory = TempDir::new().unwrap();
        let mut engine = engine_with_max(
            &directory,
            vec![
                frame(1),
                frame(1),
                frame(2),
                frame(2),
                frame(3),
                frame(3),
                frame(4),
                frame(4),
            ],
            Arc::new(Mutex::new(Vec::new())),
            2,
        );
        let first = observe(&mut engine);
        let _ = observe(&mut engine);
        fs::remove_file(first.image_path).unwrap();

        let third = observe(&mut engine);
        let fourth = observe(&mut engine);

        assert!(Path::new(&third.image_path).exists());
        assert!(Path::new(&fourth.image_path).exists());
    }

    #[test]
    fn retention_failure_prevents_input() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine_with_max(
            &directory,
            vec![frame(1), frame(1), frame(2), frame(2)],
            Arc::clone(&executed),
            2,
        );
        let oldest = observe(&mut engine);
        let latest = observe(&mut engine);
        fs::remove_file(&oldest.image_path).unwrap();
        fs::create_dir(&oldest.image_path).unwrap();

        let response = engine.handle(RequestEnvelope {
            request_id: "act-no-capacity".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: latest.frame_id,
                actions: vec![Action::Type {
                    text: "must not run".to_owned(),
                }],
                settle: SettlePolicy::default(),
            }),
        });

        let ResponseResult::Error(error) = response.result else {
            panic!("expected retention error");
        };
        assert_eq!(error.code, ErrorCode::Internal);
        assert!(executed.lock().unwrap().is_empty());
    }

    #[test]
    fn post_action_publish_failure_reports_executed_count_without_a_path() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine_with_max(
            &directory,
            vec![frame(1), frame(1), frame(2), frame(2)],
            Arc::clone(&executed),
            2,
        );
        let observation = observe(&mut engine);
        let next_frame_id = observation
            .frame_id
            .replace("_0000000000000001", "_0000000000000002");
        let blocked_temporary = directory
            .path()
            .join("frames")
            .join(format!(".{next_frame_id}.tmp"));
        fs::create_dir(&blocked_temporary).unwrap();

        let response = engine.handle(RequestEnvelope {
            request_id: "act-publish-fails".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: observation.frame_id,
                actions: vec![Action::Type {
                    text: "already ran".to_owned(),
                }],
                settle: SettlePolicy::default(),
            }),
        });

        let ResponseResult::Error(error) = response.result else {
            panic!("expected indeterminate result");
        };
        assert_eq!(error.code, ErrorCode::Indeterminate);
        assert_eq!(error.executed, Some(1));
        assert!(
            !error
                .message
                .contains(directory.path().to_string_lossy().as_ref())
        );
        assert_eq!(executed.lock().unwrap().len(), 1);
    }

    #[test]
    fn cached_action_becomes_a_non_repeating_tombstone_after_image_eviction() {
        let directory = TempDir::new().unwrap();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = engine_with_max(
            &directory,
            vec![
                frame(1),
                frame(1),
                frame(2),
                frame(2),
                frame(3),
                frame(3),
                frame(4),
                frame(4),
            ],
            Arc::clone(&executed),
            2,
        );
        let observation = observe(&mut engine);
        let request = RequestEnvelope {
            request_id: "cached-act".to_owned(),
            request: DaemonRequest::Act(ActRequest {
                expected_frame_id: observation.frame_id,
                actions: vec![Action::Type {
                    text: "once".to_owned(),
                }],
                settle: SettlePolicy::default(),
            }),
        };
        let first = engine.handle(request.clone());
        let _ = observe(&mut engine);
        let _ = observe(&mut engine);

        let replay = engine.handle(request);

        let ResponseResult::Ok(DaemonResponse::Act(first)) = first.result else {
            panic!("expected first action result");
        };
        assert!(first.observation.is_some());
        let ResponseResult::Ok(DaemonResponse::Act(replay)) = replay.result else {
            panic!("expected cached action replay");
        };
        assert_eq!(replay.status, ActStatus::Ok);
        assert_eq!(replay.executed, 1);
        assert!(replay.image_expired);
        assert!(replay.observation.is_none());
        assert_eq!(executed.lock().unwrap().len(), 1);
    }
}

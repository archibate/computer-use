use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use cu_protocol::{
    ActOutcome, ActStatus, CuError, DaemonRequest, DaemonResponse, ErrorCode, Observation,
    RequestEnvelope, ResponseEnvelope, ResponseResult, SettlePolicy, Viewport,
    validate_act_request, validate_settle_policy,
};
use uuid::Uuid;

pub struct CapturedFrame {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub target: String,
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
    /// `max_frames` is zero.
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
            max_cached_actions: max_frames.max(1),
        })
    }

    #[must_use]
    pub fn latest(&self) -> Option<&Observation> {
        self.latest.as_ref()
    }

    pub fn handle(&mut self, envelope: RequestEnvelope) -> ResponseEnvelope {
        if let Some(cached) = self.completed_actions.get(&envelope.request_id) {
            if cached.request == envelope.request {
                return cached.response.clone();
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
                let observation = self.persist_frame(frame, settled)?;
                return Ok(ActOutcome {
                    status: ActStatus::Partial,
                    executed,
                    action_error: Some(error.with_executed(executed)),
                    observation,
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
        let observation = self.persist_frame(frame, settled)?;
        Ok(ActOutcome {
            status: ActStatus::Ok,
            executed,
            action_error: None,
            observation,
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
            coordinate_space: "frame_pixels".to_owned(),
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
}

impl FrameStore {
    fn new(directory: PathBuf, max_frames: usize) -> Result<Self, CuError> {
        if max_frames == 0 {
            return Err(CuError::new(
                ErrorCode::Internal,
                "max_frames must be greater than zero",
            ));
        }
        fs::create_dir_all(&directory).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!(
                    "failed to create frame directory {}: {error}",
                    directory.display()
                ),
            )
        })?;
        set_private_directory_permissions(&directory)?;
        Ok(Self {
            directory,
            paths: VecDeque::new(),
            max_frames,
        })
    }

    fn persist(&mut self, frame_id: &str, png: &[u8]) -> Result<PathBuf, CuError> {
        let final_path = self.directory.join(format!("{frame_id}.png"));
        let temporary_path = self.directory.join(format!(".{frame_id}.tmp"));
        fs::write(&temporary_path, png).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to write {}: {error}", temporary_path.display()),
            )
        })?;
        set_private_file_permissions(&temporary_path)?;
        fs::rename(&temporary_path, &final_path).map_err(|error| {
            CuError::new(
                ErrorCode::Internal,
                format!("failed to publish frame {}: {error}", final_path.display()),
            )
        })?;

        self.paths.push_back(final_path.clone());
        while self.paths.len() > self.max_frames {
            if let Some(oldest) = self.paths.pop_front() {
                let _ = fs::remove_file(oldest);
            }
        }
        Ok(final_path)
    }
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

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), CuError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        CuError::new(
            ErrorCode::Internal,
            format!("failed to protect frame {}: {error}", path.display()),
        )
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), CuError> {
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
    use std::sync::{Arc, Mutex};

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
        Engine::new(
            Box::new(MockDesktop {
                captures: captures.into(),
                executed,
            }),
            directory.path(),
            8,
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
        assert_ne!(outcome.observation.frame_id, observation.frame_id);
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
}

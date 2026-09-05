use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail};
use cu_core::{Engine, ResourceLease, WaitStep};
use cu_protocol::{
    CuError, DaemonRequest, DaemonResponse, ErrorCode, RequestEnvelope, ResponseEnvelope,
    ResponseResult, WaitOutcome, WaitRequest,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};

pub struct BoundSocket {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
    _lease: ResourceLease,
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.dev() == self.device && metadata.ino() == self.inode {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub async fn bind(socket: PathBuf) -> Result<BoundSocket> {
    prepare_socket_parent(&socket)?;
    let lease = ResourceLease::acquire(&socket, "socket")?;
    prepare_socket(&socket).await?;
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", socket.display()))?;
    let metadata = fs::symlink_metadata(&socket)
        .with_context(|| format!("failed to inspect bound socket {}", socket.display()))?;
    Ok(BoundSocket {
        listener,
        path: socket,
        device: metadata.dev(),
        inode: metadata.ino(),
        _lease: lease,
    })
}

pub async fn serve(bound: BoundSocket, engine: Engine) -> Result<()> {
    let engine = Arc::new(Mutex::new(engine));
    let wait_slots = Arc::new(Semaphore::new(1));
    loop {
        let (stream, _) = bound
            .listener
            .accept()
            .await
            .context("failed to accept client")?;
        let engine = Arc::clone(&engine);
        let wait_slots = Arc::clone(&wait_slots);
        tokio::spawn(async move {
            if let Err(error) = handle(stream, engine, wait_slots).await {
                eprintln!("client error: {error:#}");
            }
        });
    }
}

fn prepare_socket_parent(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .context("socket path has no parent directory")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", parent.display()))?;
    }
    Ok(())
}

async fn prepare_socket(socket: &Path) -> Result<()> {
    if !socket.exists() {
        return Ok(());
    }
    if UnixStream::connect(socket).await.is_ok() {
        bail!(
            "another daemon is already listening at {}",
            socket.display()
        );
    }
    fs::remove_file(socket)
        .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    Ok(())
}

async fn handle(
    stream: UnixStream,
    engine: Arc<Mutex<Engine>>,
    wait_slots: Arc<Semaphore>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("failed to read request")?;
    let request: RequestEnvelope =
        serde_json::from_str(&line).context("failed to decode request")?;

    let RequestEnvelope {
        request_id,
        request,
    } = request;
    let request = match request {
        DaemonRequest::Wait(wait) => {
            return handle_wait(reader, &mut writer, engine, wait_slots, request_id, wait).await;
        }
        request => RequestEnvelope {
            request_id,
            request,
        },
    };

    let response = tokio::task::spawn_blocking(move || {
        engine
            .lock()
            .expect("engine mutex poisoned")
            .handle(request)
    })
    .await
    .context("engine task panicked")?;
    write_response(&mut writer, &response).await
}

async fn handle_wait(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    engine: Arc<Mutex<Engine>>,
    wait_slots: Arc<Semaphore>,
    request_id: String,
    request: WaitRequest,
) -> Result<()> {
    let Ok(_permit) = wait_slots.try_acquire_owned() else {
        return write_response(
            writer,
            &error_response(
                request_id,
                CuError::new(
                    ErrorCode::Busy,
                    "another screen-change wait is already active",
                ),
            ),
        )
        .await;
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let wait = run_wait(engine, request, Arc::clone(&cancelled));
    tokio::pin!(wait);
    let mut extra = [0_u8; 1];
    tokio::select! {
        biased;
        read = reader.read(&mut extra) => {
            cancelled.store(true, Ordering::Release);
            match read {
                Ok(0) => Ok(()),
                Ok(_) => bail!("client sent data after its wait request"),
                Err(error) => Err(error).context("failed while watching wait client"),
            }
        }
        result = &mut wait => {
            let response = match result {
                Ok(outcome) => ResponseEnvelope {
                    request_id,
                    result: ResponseResult::Ok(DaemonResponse::Wait(outcome)),
                },
                Err(error) => error_response(request_id, error),
            };
            write_response(writer, &response).await
        }
    }
}

async fn run_wait(
    engine: Arc<Mutex<Engine>>,
    request: WaitRequest,
    cancelled: Arc<AtomicBool>,
) -> Result<WaitOutcome, CuError> {
    let started_at = Instant::now();
    let begin_engine = Arc::clone(&engine);
    let mut tracker = tokio::task::spawn_blocking(move || {
        lock_engine(&begin_engine)?.begin_wait(&request, started_at)
    })
    .await
    .map_err(|error| join_error(&error))??;

    loop {
        let now = Instant::now();
        tokio::time::sleep(tracker.next_delay(now)).await;
        if cancelled.load(Ordering::Acquire) {
            return Err(CuError::new(
                ErrorCode::Cancelled,
                "screen-change wait was cancelled",
            ));
        }

        let capture_engine = Arc::clone(&engine);
        let capture_cancelled = Arc::clone(&cancelled);
        let (returned, sample) = tokio::task::spawn_blocking(move || {
            let sample =
                lock_engine(&capture_engine)?.capture_wait_sample(&tracker, &capture_cancelled);
            Ok::<_, CuError>((tracker, sample))
        })
        .await
        .map_err(|error| join_error(&error))??;
        tracker = returned;
        let sample = sample?;

        match tracker.observe_sample(sample, Instant::now())? {
            WaitStep::Pending => {}
            WaitStep::TimedOut(outcome) => return Ok(outcome),
            WaitStep::Publish(publication) => {
                let publish_engine = Arc::clone(&engine);
                let publish_cancelled = Arc::clone(&cancelled);
                return tokio::task::spawn_blocking(move || {
                    lock_engine(&publish_engine)?.publish_wait(
                        &tracker,
                        publication,
                        &publish_cancelled,
                    )
                })
                .await
                .map_err(|error| join_error(&error))?;
            }
        }
    }
}

fn lock_engine(engine: &Arc<Mutex<Engine>>) -> Result<std::sync::MutexGuard<'_, Engine>, CuError> {
    engine
        .lock()
        .map_err(|_| CuError::new(ErrorCode::Internal, "engine mutex was poisoned"))
}

fn join_error(error: &tokio::task::JoinError) -> CuError {
    CuError::new(
        ErrorCode::Internal,
        format!("engine task panicked: {error}"),
    )
}

fn error_response(request_id: String, error: CuError) -> ResponseEnvelope {
    ResponseEnvelope {
        request_id,
        result: ResponseResult::Error(error),
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ResponseEnvelope,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(response).context("failed to encode response")?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .context("failed to send response")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use cu_core::{CapturedFrame, Desktop};
    use cu_protocol::{
        CuError, DaemonRequest, DaemonResponse, ErrorCode, ObserveRequest, RequestEnvelope,
        ResponseResult, SettlePolicy, Viewport, WaitRequest,
    };
    use tempfile::TempDir;

    use super::*;

    struct MockDesktop {
        captures: VecDeque<CapturedFrame>,
    }

    struct CountingDesktop {
        frame: CapturedFrame,
        captures: Arc<AtomicUsize>,
    }

    impl Desktop for MockDesktop {
        fn capture(&mut self) -> Result<CapturedFrame, CuError> {
            Ok(self.captures.pop_front().expect("mock capture available"))
        }

        fn validate(&self, _action: &cu_protocol::Action) -> Result<(), CuError> {
            Ok(())
        }

        fn execute(
            &mut self,
            _action: &cu_protocol::Action,
            _viewport: Viewport,
        ) -> Result<(), CuError> {
            Ok(())
        }
    }

    impl Desktop for CountingDesktop {
        fn capture(&mut self) -> Result<CapturedFrame, CuError> {
            self.captures.fetch_add(1, Ordering::Relaxed);
            Ok(self.frame.clone())
        }

        fn validate(&self, _action: &cu_protocol::Action) -> Result<(), CuError> {
            Ok(())
        }

        fn execute(
            &mut self,
            _action: &cu_protocol::Action,
            _viewport: Viewport,
        ) -> Result<(), CuError> {
            Ok(())
        }
    }

    fn frame() -> CapturedFrame {
        CapturedFrame {
            pixels: Arc::from(vec![1; 100 * 80 * 3]),
            width: 100,
            height: 80,
            target: "mock:screen".to_owned(),
        }
    }

    fn wait_request(frame_id: String, timeout_ms: u64) -> WaitRequest {
        WaitRequest {
            last_frame_id: frame_id,
            include_rects: vec![cu_protocol::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 80,
            }],
            exclude_rects: Vec::new(),
            timeout_ms,
            quiet_ms: 100,
        }
    }

    async fn establish_baseline(socket: &Path) -> cu_protocol::Observation {
        let response = crate::client::request(
            socket,
            &RequestEnvelope {
                request_id: "baseline".to_owned(),
                request: DaemonRequest::Observe(ObserveRequest {
                    settle: SettlePolicy {
                        quiet_ms: 1,
                        timeout_ms: 1,
                    },
                }),
            },
        )
        .await
        .unwrap();
        let ResponseResult::Ok(DaemonResponse::Observe(observation)) = response.result else {
            panic!("expected baseline observation");
        };
        observation
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn unix_transport_returns_an_observation() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("cu.sock");
        let engine = Engine::new(
            Box::new(MockDesktop {
                captures: vec![frame(), frame()].into(),
            }),
            directory.path().join("frames"),
            4,
        )
        .unwrap();
        let bound = bind(socket.clone()).await.unwrap();
        let server = tokio::spawn(serve(bound, engine));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        let response = crate::client::request(
            &socket,
            &RequestEnvelope {
                request_id: "transport-observe".to_owned(),
                request: DaemonRequest::Observe(ObserveRequest {
                    settle: cu_protocol::SettlePolicy {
                        quiet_ms: 1,
                        timeout_ms: 1,
                    },
                }),
            },
        )
        .await
        .unwrap();
        server.abort();

        assert!(matches!(
            response.result,
            ResponseResult::Ok(DaemonResponse::Observe(_))
        ));
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn concurrent_observe_supersedes_a_wait_without_being_blocked() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("cu.sock");
        let engine = Engine::new(
            Box::new(CountingDesktop {
                frame: frame(),
                captures: Arc::new(AtomicUsize::new(0)),
            }),
            directory.path().join("frames"),
            8,
        )
        .unwrap();
        let bound = bind(socket.clone()).await.unwrap();
        let server = tokio::spawn(serve(bound, engine));
        let baseline = establish_baseline(&socket).await;

        let wait_socket = socket.clone();
        let wait = tokio::spawn(async move {
            crate::client::request(
                &wait_socket,
                &RequestEnvelope {
                    request_id: "wait".to_owned(),
                    request: DaemonRequest::Wait(wait_request(baseline.frame_id, 2_000)),
                },
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = crate::client::request(
            &socket,
            &RequestEnvelope {
                request_id: "superseding-observe".to_owned(),
                request: DaemonRequest::Observe(ObserveRequest {
                    settle: SettlePolicy {
                        quiet_ms: 1,
                        timeout_ms: 1,
                    },
                }),
            },
        )
        .await
        .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap()
            .unwrap();
        let ResponseResult::Error(error) = response.result else {
            panic!("expected stale wait");
        };
        assert_eq!(error.code, ErrorCode::StaleFrame);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn disconnect_cancels_a_wait_and_stops_capture_polling() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("cu.sock");
        let captures = Arc::new(AtomicUsize::new(0));
        let engine = Engine::new(
            Box::new(CountingDesktop {
                frame: frame(),
                captures: Arc::clone(&captures),
            }),
            directory.path().join("frames"),
            8,
        )
        .unwrap();
        let bound = bind(socket.clone()).await.unwrap();
        let server = tokio::spawn(serve(bound, engine));
        let baseline = establish_baseline(&socket).await;

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let envelope = RequestEnvelope {
            request_id: "disconnecting-wait".to_owned(),
            request: DaemonRequest::Wait(wait_request(baseline.frame_id, 10_000)),
        };
        let mut encoded = serde_json::to_vec(&envelope).unwrap();
        encoded.push(b'\n');
        stream.write_all(&encoded).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(stream);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let after_cancel = captures.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(captures.load(Ordering::Relaxed), after_cancel);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn second_concurrent_wait_returns_busy() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("cu.sock");
        let engine = Engine::new(
            Box::new(CountingDesktop {
                frame: frame(),
                captures: Arc::new(AtomicUsize::new(0)),
            }),
            directory.path().join("frames"),
            8,
        )
        .unwrap();
        let bound = bind(socket.clone()).await.unwrap();
        let server = tokio::spawn(serve(bound, engine));
        let baseline = establish_baseline(&socket).await;
        let mut held = UnixStream::connect(&socket).await.unwrap();
        let envelope = RequestEnvelope {
            request_id: "held-wait".to_owned(),
            request: DaemonRequest::Wait(wait_request(baseline.frame_id.clone(), 10_000)),
        };
        let mut encoded = serde_json::to_vec(&envelope).unwrap();
        encoded.push(b'\n');
        held.write_all(&encoded).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let response = tokio::time::timeout(
            Duration::from_secs(1),
            crate::client::request(
                &socket,
                &RequestEnvelope {
                    request_id: "excess-wait".to_owned(),
                    request: DaemonRequest::Wait(wait_request(baseline.frame_id, 10_000)),
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let ResponseResult::Error(error) = response.result else {
            panic!("expected busy response");
        };
        assert_eq!(error.code, ErrorCode::Busy);

        drop(held);
        server.abort();
    }

    #[tokio::test]
    async fn existing_socket_parent_permissions_are_unchanged() {
        let directory = TempDir::new().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

        prepare_socket_parent(&directory.path().join("cu.sock")).unwrap();

        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn distinct_socket_paths_can_be_bound_concurrently() {
        let directory = TempDir::new().unwrap();
        let first_path = directory.path().join("first.sock");
        let second_path = directory.path().join("second.sock");

        let first = bind(first_path.clone()).await.unwrap();
        let second = bind(second_path.clone()).await.unwrap();

        assert!(first_path.exists());
        assert!(second_path.exists());
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn two_daemons_serve_distinct_sockets_and_frame_stores() {
        let directory = TempDir::new().unwrap();
        let first_socket = directory.path().join("first.sock");
        let second_socket = directory.path().join("second.sock");
        let first_frames = directory.path().join("first-frames");
        let second_frames = directory.path().join("second-frames");
        let first_bound = bind(first_socket.clone()).await.unwrap();
        let second_bound = bind(second_socket.clone()).await.unwrap();
        let first_engine = Engine::new(
            Box::new(MockDesktop {
                captures: vec![frame(), frame()].into(),
            }),
            &first_frames,
            4,
        )
        .unwrap();
        let second_engine = Engine::new(
            Box::new(MockDesktop {
                captures: vec![frame(), frame()].into(),
            }),
            &second_frames,
            4,
        )
        .unwrap();
        let first_server = tokio::spawn(serve(first_bound, first_engine));
        let second_server = tokio::spawn(serve(second_bound, second_engine));

        let first_response = crate::client::request(
            &first_socket,
            &RequestEnvelope {
                request_id: "first-observe".to_owned(),
                request: DaemonRequest::Observe(ObserveRequest {
                    settle: cu_protocol::SettlePolicy {
                        quiet_ms: 1,
                        timeout_ms: 1,
                    },
                }),
            },
        )
        .await
        .unwrap();
        let second_response = crate::client::request(
            &second_socket,
            &RequestEnvelope {
                request_id: "second-observe".to_owned(),
                request: DaemonRequest::Observe(ObserveRequest {
                    settle: cu_protocol::SettlePolicy {
                        quiet_ms: 1,
                        timeout_ms: 1,
                    },
                }),
            },
        )
        .await
        .unwrap();

        let ResponseResult::Ok(DaemonResponse::Observe(first)) = first_response.result else {
            panic!("first daemon did not return an observation");
        };
        let ResponseResult::Ok(DaemonResponse::Observe(second)) = second_response.result else {
            panic!("second daemon did not return an observation");
        };
        assert!(Path::new(&first.image_path).starts_with(&first_frames));
        assert!(Path::new(&second.image_path).starts_with(&second_frames));
        assert_ne!(first.frame_id, second.frame_id);

        first_server.abort();
        second_server.abort();
        let _ = first_server.await;
        let _ = second_server.await;
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn a_second_owner_cannot_unlink_a_live_socket() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("cu.sock");
        let first = bind(socket.clone()).await.unwrap();

        let Err(error) = bind(socket.clone()).await else {
            panic!("second socket owner unexpectedly succeeded");
        };

        assert!(error.to_string().contains("lease_conflict"));
        assert!(socket.exists());
        drop(first);
        assert!(!socket.exists());
    }

    #[tokio::test]
    #[ignore = "requires local Unix socket access"]
    async fn socket_owner_does_not_unlink_a_replacement_path() {
        let directory = TempDir::new().unwrap();
        let socket = directory.path().join("cu.sock");
        let bound = bind(socket.clone()).await.unwrap();
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, "replacement").unwrap();

        drop(bound);

        assert_eq!(fs::read_to_string(socket).unwrap(), "replacement");
    }
}

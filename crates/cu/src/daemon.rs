use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use cu_core::{Engine, ResourceLease};
use cu_protocol::{RequestEnvelope, ResponseEnvelope};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
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
    loop {
        let (stream, _) = bound
            .listener
            .accept()
            .await
            .context("failed to accept client")?;
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            if let Err(error) = handle(stream, engine).await {
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

async fn handle(stream: UnixStream, engine: Arc<Mutex<Engine>>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader)
        .read_line(&mut line)
        .await
        .context("failed to read request")?;
    let request: RequestEnvelope =
        serde_json::from_str(&line).context("failed to decode request")?;

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
    use std::{collections::VecDeque, time::Duration};

    use cu_core::{CapturedFrame, Desktop};
    use cu_protocol::{
        CuError, DaemonRequest, DaemonResponse, ObserveRequest, RequestEnvelope, ResponseResult,
        Viewport,
    };
    use tempfile::TempDir;

    use super::*;

    struct MockDesktop {
        captures: VecDeque<CapturedFrame>,
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

    fn frame() -> CapturedFrame {
        CapturedFrame {
            png: vec![1, 2, 3],
            width: 100,
            height: 80,
            target: "mock:screen".to_owned(),
        }
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

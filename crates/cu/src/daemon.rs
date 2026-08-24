use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use cu_core::Engine;
use cu_protocol::{RequestEnvelope, ResponseEnvelope};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

pub async fn serve(socket: PathBuf, engine: Engine) -> Result<()> {
    prepare_socket(&socket).await?;
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", socket.display()))?;

    let engine = Arc::new(Mutex::new(engine));
    loop {
        let (stream, _) = listener.accept().await.context("failed to accept client")?;
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            if let Err(error) = handle(stream, engine).await {
                eprintln!("client error: {error:#}");
            }
        });
    }
}

async fn prepare_socket(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .context("socket path has no parent directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", parent.display()))?;
    }

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
        let server = tokio::spawn(serve(socket.clone(), engine));
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

        prepare_socket(&directory.path().join("cu.sock"))
            .await
            .unwrap();

        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

use std::{io, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use cu_protocol::{RequestEnvelope, ResponseEnvelope};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

pub async fn request(socket: &Path, request: &RequestEnvelope) -> Result<ResponseEnvelope> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|error| connection_error(socket, &error))?;
    let mut encoded = serde_json::to_vec(request).context("failed to encode request")?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .await
        .context("failed to send request")?;
    stream
        .shutdown()
        .await
        .context("failed to finish request")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .context("failed to read response")?;
    if line.is_empty() {
        bail!("daemon closed the connection without a response");
    }
    serde_json::from_str(&line).map_err(|error| anyhow!("invalid daemon response: {error}"))
}

fn connection_error(socket: &Path, error: &io::Error) -> anyhow::Error {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    ) {
        anyhow!(
            "computer-use daemon is not running at {}\nstart it with: cu daemon",
            socket.display()
        )
    } else {
        anyhow!("failed to connect to {}: {error}", socket.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_socket_error_explains_how_to_start_the_daemon() {
        let error = connection_error(
            Path::new("/run/user/1000/computer-use/cu.sock"),
            &io::Error::from(io::ErrorKind::NotFound),
        );

        assert!(error.to_string().contains("start it with: cu daemon"));
    }
}

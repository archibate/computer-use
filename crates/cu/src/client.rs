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
    let summary = if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    ) {
        format!("computer-use daemon is unavailable at {}", socket.display())
    } else {
        format!("failed to connect to {}: {error}", socket.display())
    };
    let start_command = named_instance_from_socket(socket).map_or_else(
        || "`cu daemon`".to_owned(),
        |instance| format!("`cu daemon --instance {instance}`"),
    );
    anyhow!(
        "{summary}\nstart it separately, then retry: {start_command} (auto-detects the desktop); use `cu daemon --help` for an explicit backend override"
    )
}

fn named_instance_from_socket(socket: &Path) -> Option<&str> {
    if socket.file_name()? != "cu.sock" {
        return None;
    }
    let instance_dir = socket.parent()?;
    if instance_dir.parent()?.file_name()? != "instances" {
        return None;
    }
    instance_dir.file_name()?.to_str()
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

        let message = error.to_string();
        assert!(message.contains("start it separately, then retry"));
        assert!(message.contains("`cu daemon`"));
        assert!(message.contains("`cu daemon --help`"));
    }

    #[test]
    fn missing_named_instance_recommends_the_matching_daemon() {
        let error = connection_error(
            Path::new("/run/user/1000/computer-use/instances/x11-99/cu.sock"),
            &io::Error::from(io::ErrorKind::NotFound),
        );

        assert!(error.to_string().contains("`cu daemon --instance x11-99`"));
    }
}

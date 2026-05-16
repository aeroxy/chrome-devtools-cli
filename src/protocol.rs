use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use anyhow::Context;

/// Request from CLI client to daemon.
#[derive(Serialize, Deserialize, Debug)]
pub struct DaemonRequest {
    pub command: String,
    pub args: Value,
    pub page: Option<usize>,
    pub target: Option<String>,
    pub json_output: bool,
}

/// Response from daemon to CLI client.
#[derive(Serialize, Deserialize, Debug)]
pub struct DaemonResponse {
    pub success: bool,
    pub output: String,
    pub error: String,
    pub navigated_to: Option<String>,
    pub error_code: Option<u32>,
}

#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.sock")
}

#[cfg(windows)]
pub fn addr_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.addr")
}

pub fn pid_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.pid")
}

/// Write a length-prefixed message to a stream.
pub async fn write_msg<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) -> anyhow::Result<()> {
    let len = (data.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}

/// Read a length-prefixed message from a stream.
pub async fn read_msg<R: AsyncReadExt + Unpin>(r: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = usize::try_from(u32::from_be_bytes(len_buf))
        .context("Message length overflows usize")?;
    if len > 64 * 1024 * 1024 {
        anyhow::bail!("Message too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

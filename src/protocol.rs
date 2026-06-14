use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use anyhow::Context;

use crate::format::OutputFormat;

/// Request from CLI client to daemon.
#[derive(Serialize, Deserialize, Debug)]
pub struct DaemonRequest {
    pub command: String,
    pub args: Value,
    pub page: Option<usize>,
    pub target: Option<String>,
    #[serde(default)]
    pub json_output: bool,
    #[serde(default)]
    pub output_format: Option<OutputFormat>,
    /// URL patterns to add to the daemon's network blocklist (from global CLI flags).
    #[serde(default)]
    pub block_url: Vec<String>,
    /// URL patterns to remove from the daemon's network blocklist (from global CLI flags).
    #[serde(default)]
    pub allow_url: Vec<String>,
}

impl DaemonRequest {
    /// Resolve the output format, preferring the new `output_format` field
    /// and falling back to the legacy `json_output` bool.
    pub fn format(&self) -> OutputFormat {
        self.output_format
            .unwrap_or(if self.json_output {
                OutputFormat::Json
            } else {
                OutputFormat::Text
            })
    }
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

/// Path to the Unix domain socket for daemon communication.
#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.sock")
}

/// Path to the named-pipe address file for daemon communication (Windows).
#[cfg(windows)]
pub fn addr_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.addr")
}

/// Path to the daemon PID file.
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

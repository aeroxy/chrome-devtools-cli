use anyhow::{bail, Result};
use std::time::{Duration, SystemTime};
#[cfg(windows)]
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::protocol::*;

#[cfg(unix)]
async fn connect_daemon() -> Result<UnixStream> {
    Ok(UnixStream::connect(socket_path()).await?)
}

#[cfg(windows)]
async fn connect_daemon() -> Result<TcpStream> {
    let addr = std::fs::read_to_string(addr_path())?;
    Ok(TcpStream::connect(addr.trim()).await?)
}

/// Read the daemon wait timeout from `DAEMON_WAIT_TIMEOUT_SECS`, defaulting to 5.
fn daemon_wait_timeout() -> Duration {
    std::env::var("DAEMON_WAIT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
}

/// Try to send a request to the daemon. Returns error if daemon is not running.
pub async fn send_to_daemon(request: &DaemonRequest) -> Result<DaemonResponse> {
    let mut stream = connect_daemon().await?;

    let req_bytes = serde_json::to_vec(request)?;
    write_msg(&mut stream, &req_bytes).await?;

    let resp_bytes = read_msg(&mut stream).await?;
    let response: DaemonResponse = serde_json::from_slice(&resp_bytes)?;
    Ok(response)
}

/// Spawn the daemon process in the background.
pub fn spawn_daemon(ws_url: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["__daemon__", ws_url])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    cmd.spawn()?;
    Ok(())
}

/// Wait for the daemon socket to become available, with exponential backoff.
pub async fn wait_for_daemon() -> Result<()> {
     let deadline = tokio::time::Instant::now() + daemon_wait_timeout();
    let mut delay = Duration::from_millis(50);
    loop {
        if tokio::time::Instant::now() > deadline {
            bail!("Daemon failed to start within {} seconds", daemon_wait_timeout().as_secs());
        }
        if connect_daemon().await.is_ok() {
            return Ok(());
        }
        // Simple jitter based on current time subseconds
        let jitter = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| Duration::from_millis(d.subsec_nanos() as u64 % (delay.as_millis() as u64 + 1)))
            .unwrap_or_default();
        tokio::time::sleep(delay + jitter).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

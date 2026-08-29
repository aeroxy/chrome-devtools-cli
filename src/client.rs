use anyhow::{bail, Context, Result};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::time::{Duration, SystemTime};
#[cfg(windows)]
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::protocol::*;

#[cfg(unix)]
async fn connect_daemon(key: &str) -> Result<UnixStream> {
    let path = socket_path(key);
    UnixStream::connect(&path)
        .await
        .with_context(|| format!("Failed to connect to daemon {key} at {}", path.display()))
}

#[cfg(windows)]
async fn connect_daemon(key: &str) -> Result<TcpStream> {
    let path = addr_path(key);
    let addr = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "Failed to read daemon {key} address file {}",
            path.display()
        )
    })?;
    let addr = addr.trim();
    TcpStream::connect(addr)
        .await
        .with_context(|| format!("Failed to connect to daemon {key} at {addr}"))
}

/// Read the daemon wait timeout from `DAEMON_WAIT_TIMEOUT_SECS`, defaulting to
/// 5. This is the whole budget a spawned daemon has to become reachable, so the
/// daemon derives its own startup lock wait from it (`daemon::lock_wait_timeout`)
/// — the spawned process inherits this environment.
pub(crate) fn daemon_wait_timeout() -> Duration {
    std::env::var("DAEMON_WAIT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
}

/// Try to send a request to the daemon for `key`. Returns an error if no
/// daemon is attached to that endpoint.
///
/// The key is what makes that scoping real: it selects one daemon's socket, so
/// a request can never be answered by a daemon attached to a different browser.
/// A single per-user socket would route this to whichever browser the first
/// command happened to resolve, silently ignoring the caller's `--browser` and
/// `--user-data-dir`.
pub async fn send_to_daemon(key: &str, request: &DaemonRequest) -> Result<DaemonResponse> {
    let mut stream = connect_daemon(key).await?;

    let req_bytes = serde_json::to_vec(request)?;
    write_msg(&mut stream, &req_bytes).await?;

    let resp_bytes = read_msg(&mut stream).await?;
    let response: DaemonResponse = serde_json::from_slice(&resp_bytes)?;
    Ok(response)
}

/// Spawn the daemon process in the background.
///
/// `browser` is descriptive only — it is recorded in the daemon's info file so
/// `list-daemons` can name the browser. The daemon's identity comes from
/// `ws_url` alone.
pub fn spawn_daemon(ws_url: &str, browser: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["__daemon__", ws_url, browser])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    cmd.spawn()?;
    Ok(())
}

/// Wait for the daemon socket for `key` to become available, with exponential
/// backoff.
///
/// Keyed for the same reason as [`send_to_daemon`]: the loop must wait for the
/// daemon this command just spawned for this endpoint, not settle for any
/// daemon that happens to be running.
pub async fn wait_for_daemon(key: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + daemon_wait_timeout();
    let mut delay = Duration::from_millis(50);
    loop {
        if tokio::time::Instant::now() > deadline {
            bail!(
                "Daemon failed to start within {} seconds",
                daemon_wait_timeout().as_secs()
            );
        }
        if connect_daemon(key).await.is_ok() {
            return Ok(());
        }
        // Simple jitter based on current time subseconds
        let jitter = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| {
                Duration::from_millis(d.subsec_nanos() as u64 % (delay.as_millis() as u64 + 1))
            })
            .unwrap_or_default();
        tokio::time::sleep(delay + jitter).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

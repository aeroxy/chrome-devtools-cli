use anyhow::{Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::cdp::CdpClient;
use crate::commands::executor;
use crate::error::ErrorCode;
use crate::protocol::*;
use crate::telemetry;

/// Read the daemon idle timeout from `DAEMON_IDLE_TIMEOUT_SECS`, defaulting to 300 (5 minutes).
fn idle_timeout() -> Duration {
    std::env::var("DAEMON_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300))
}

/// Result of handling a single daemon connection.
enum ConnectionOutcome {
    /// Continue accepting new connections.
    Continue,
    /// Fatal error occurred; daemon should exit.
    Fatal,
}

fn open_lock_file() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path())
}

/// Acquire the cross-process lock serializing the daemon-file critical
/// sections: startup's pid-write/rebind and cleanup's check-then-remove.
/// Blocks until the lock is free; the OS releases it when the handle drops.
///
/// Errors instead of degrading: the lock is what makes ownership handoff
/// correct, and a daemon that can't create a file in temp_dir couldn't write
/// its PID file either — failing here just surfaces the cause sooner.
fn lock_daemon_files() -> Result<std::fs::File> {
    let f = open_lock_file()
        .with_context(|| format!("Failed to open daemon lock file {}", lock_path().display()))?;
    f.lock()
        .with_context(|| format!("Failed to lock daemon lock file {}", lock_path().display()))?;
    Ok(f)
}

/// Best-effort removal of the daemon's socket/address and PID files.
///
/// Only cleans up when the PID file still names this process: the paths are
/// shared, so a stale-but-alive old daemon exiting must not delete the files
/// of a newer daemon that has since rebound them. The ownership check and the
/// removals happen under the daemon-file lock so a replacement can't write
/// its pid and rebind in between (it would keep running but be unreachable,
/// and every later CLI call would spawn yet another daemon).
fn cleanup() {
    let lock_file = match open_lock_file() {
        Ok(f) => f,
        Err(e) => {
            // Without the lock, removal could race a replacement's startup —
            // leaving the files is the safe side (they self-heal on the next
            // daemon start), but say why so the cause isn't swallowed.
            eprintln!(
                "daemon: leaving socket/PID files in place: cannot open lock file {}: {e}",
                lock_path().display()
            );
            return;
        }
    };
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            // Contended: a replacement is mid-startup. Its rebind supersedes
            // our files, and any leftovers self-heal on the next start.
            return;
        }
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!(
                "daemon: leaving socket/PID files in place: cannot lock {}: {e}",
                lock_path().display()
            );
            return;
        }
    }
    let _lock = lock_file;
    let owns_files = std::fs::read_to_string(pid_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id());
    if !owns_files {
        return;
    }
    #[cfg(unix)]
    let _ = std::fs::remove_file(socket_path());
    #[cfg(windows)]
    let _ = std::fs::remove_file(addr_path());
    let _ = std::fs::remove_file(pid_path());
}

/// Removes the daemon's on-disk files when `run_daemon`'s frame is left for
/// any reason: normal return, early `?` error, or an unwinding panic.
/// (SIGKILL and other non-catchable terminations are inherently not covered.)
struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup();
    }
}

macro_rules! run_accept_loop_body {
    ($accept:expr, $client:expr, $ws_url:expr, $shutdown:expr) => {
        loop {
            tokio::select! {
                _ = &mut $shutdown => {
                    // SIGTERM/SIGINT (or Ctrl-C on Windows) — exit cleanly.
                    // Only observed between requests: an in-flight command
                    // finishes and its response is written before shutdown.
                    break;
                }
                accept = tokio::time::timeout(idle_timeout(), $accept) => match accept {
                    Ok(Ok((stream, _))) => match handle_connection(stream, $client, $ws_url).await {
                        ConnectionOutcome::Continue => {}
                        ConnectionOutcome::Fatal => break,
                    },
                    Ok(Err(e)) => {
                        eprintln!("daemon: accept error: {e}");
                    }
                    Err(_) => {
                        // Idle timeout — exit
                        break;
                    }
                }
            }
        }
    };
}

/// SIGTERM/SIGINT must be turned into a normal accept-loop exit: their
/// default disposition kills the process without unwinding, so `CleanupGuard`
/// would never run and the socket/PID files would go stale. SIGHUP is
/// deliberately left unhandled — convention reserves it for config reload,
/// not shutdown.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    // If a signal stream can't be registered, fall back to never resolving —
    // the daemon then behaves as before this handler existed (default
    // disposition still terminates the process; only file cleanup is lost).
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}

/// Best-effort on Windows: the daemon is spawned with CREATE_NO_WINDOW (no
/// console), so SetConsoleCtrlHandler-based Ctrl-C delivery typically never
/// fires for a backgrounded daemon. It does work when `__daemon__` is run
/// manually in a foreground console for debugging.
#[cfg(windows)]
async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

pub async fn run_daemon(ws_url: &str) -> Result<()> {
    // Write PID
    // Armed before anything is written: cleanup() verifies pid-file ownership
    // first, so firing "too early" is a no-op, and this declaration order
    // means the startup lock below is released (locals drop in reverse order)
    // before the guard's cleanup() tries to take it — no self-deadlock on an
    // early `?` return or panic. Covers every way this frame is left,
    // including unwinding panics, which previously leaked the files.
    let _guard = CleanupGuard;

    // Startup critical section: the pid write and endpoint (re)bind must not
    // interleave with a predecessor's cleanup() check-then-remove, or the
    // predecessor can delete files this daemon just claimed.
    let startup_lock = lock_daemon_files()?;

    std::fs::write(pid_path(), std::process::id().to_string())?;

    #[cfg(unix)]
    let listener = {
        // Clean up stale socket
        let sock = socket_path();
        let _ = std::fs::remove_file(&sock);

        // Bind socket FIRST so the CLI knows the daemon is alive and can connect.
        // If we wait for CdpClient::connect first, a macOS network permission prompt
        // can block the daemon and cause the CLI's 5-second wait_for_daemon timeout to expire.
        UnixListener::bind(&sock)?
    };

    #[cfg(windows)]
    let listener = {
        // Clean up stale address file
        let _ = std::fs::remove_file(addr_path());

        // Bind listener FIRST so the CLI knows the daemon is alive and can connect.
        // If we wait for CdpClient::connect first, a Chrome/network permission prompt
        // can block the daemon and cause the CLI's 5-second wait_for_daemon timeout to expire.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        std::fs::write(addr_path(), listener.local_addr()?.to_string())?;
        listener
    };

    drop(startup_lock);

    // We don't connect immediately. We wait for the first connection from the CLI.
    // This ensures the CLI wait_for_daemon() succeeds, and the CLI blocks on read_msg()
    // while the daemon handles the potentially slow macOS/Chrome network permission prompt.
    let mut client: Option<CdpClient> = None;

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // Signal readiness by socket/address existence (it's already bound)
    run_accept_loop_body!(listener.accept(), &mut client, ws_url, shutdown);

    // File cleanup is handled by `_guard` (also covers signal/panic exits).

    // Shut down telemetry before exiting so the background thread
    // flushes pending entries and exits cleanly.
    telemetry::shutdown_logger();

    Ok(())
}

async fn handle_connection<S>(
    mut stream: S,
    client: &mut Option<CdpClient>,
    ws_url: &str,
) -> ConnectionOutcome
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let req_bytes = match read_msg(&mut stream).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("daemon: read error: {e}");
            return ConnectionOutcome::Continue;
        }
    };

    let request: DaemonRequest = match serde_json::from_slice(&req_bytes) {
        Ok(r) => r,
        Err(e) => {
            let resp = DaemonResponse {
                success: false,
                output: String::new(),
                error: format!("Invalid request: {e}"),
                navigated_to: None,
                error_code: Some(ErrorCode::InvalidInput as u32),
            };
            if let Ok(resp_bytes) = serde_json::to_vec(&resp) {
                let _ = write_msg(&mut stream, &resp_bytes).await;
            }
            return ConnectionOutcome::Continue;
        }
    };

    // Connect lazily
    if client.is_none() {
        match CdpClient::connect(ws_url).await {
            Ok(c) => *client = Some(c),
            Err(e) => {
                let resp = DaemonResponse {
                    success: false,
                    output: String::new(),
                    error: format!("Failed to connect to Chrome: {e:#}"),
                    navigated_to: None,
                    error_code: Some(ErrorCode::ChromeConnection as u32),
                };
                if let Ok(resp_bytes) = serde_json::to_vec(&resp) {
                    let _ = write_msg(&mut stream, &resp_bytes).await;
                }
                // Exit daemon if we can't connect, so the next CLI call will spawn a fresh daemon
                return ConnectionOutcome::Fatal;
            }
        }
    }

    let response = match client.as_mut() {
        Some(client) => handle_request(client, &request).await,
        None => DaemonResponse {
            success: false,
            output: String::new(),
            error: String::from("Failed to connect to Chrome: client initialization failed"),
            navigated_to: None,
            error_code: Some(ErrorCode::ChromeConnection as u32),
        },
    };

    // Check if the error indicates a disconnected WebSocket.
    // If so, we should exit the daemon so it can be respawned cleanly next time.
    let is_fatal = !response.success
        && (response.error.contains("WebSocket closed")
            || response.error.contains("WebSocket connection closed")
            || response.error.contains("WebSocket error"));

    if let Ok(resp_bytes) = serde_json::to_vec(&response) {
        let _ = write_msg(&mut stream, &resp_bytes).await;
    }

    if is_fatal {
        ConnectionOutcome::Fatal
    } else {
        ConnectionOutcome::Continue
    }
}

async fn handle_request(client: &mut CdpClient, req: &DaemonRequest) -> DaemonResponse {
    let start = std::time::Instant::now();
    let cmd_name = req.command.as_str();
    match executor::execute_command(client, req).await {
        Ok(result) => {
            let duration = start.elapsed();
            telemetry::log_command(cmd_name, duration, true, result.error_code);
            DaemonResponse {
                success: true,
                output: result.output,
                error: String::new(),
                navigated_to: result.navigated_to,
                error_code: result.error_code,
            }
        }
        Err(e) => {
            let duration = start.elapsed();
            let error_code = e
                .downcast_ref::<crate::error::CliError>()
                .map(|ce| ce.code().code());
            telemetry::log_command(cmd_name, duration, false, error_code);
            DaemonResponse {
                success: false,
                output: String::new(),
                error: format!("{e:#}"),
                navigated_to: None,
                error_code,
            }
        }
    }
}

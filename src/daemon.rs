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

/// The lock path has a predictable name and, on Linux, lives in shared /tmp,
/// so treat a pre-existing file as potentially hostile: refuse to follow a
/// planted symlink (O_NOFOLLOW) and only lock something that is a regular
/// file owned by this uid. O_NONBLOCK keeps a planted FIFO from wedging the
/// open() itself (it has no effect on regular files); the `is_file` check
/// then rejects it. macOS $TMPDIR and Windows %TEMP% are per-user, so there
/// the checks are inert.
fn open_lock_file() -> Result<std::fs::File> {
    open_lock_file_at(&lock_path())
}

/// Path-parameterized body of [`open_lock_file`], so tests can exercise the
/// hostile-path checks against a scratch directory instead of the real
/// `temp_dir()`.
fn open_lock_file_at(path: &std::path::Path) -> Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        opts.mode(0o600);
    }
    let f = opts
        .open(path)
        .with_context(|| format!("Failed to open daemon lock file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let md = f.metadata().with_context(|| {
            format!(
                "Failed to read metadata of daemon lock file {}",
                path.display()
            )
        })?;
        // SAFETY: getuid() is a pure kernel query with no preconditions; it
        // is thread-safe and cannot fail.
        if !md.is_file() || md.uid() != unsafe { libc::getuid() } {
            anyhow::bail!(
                "Daemon lock path {} is not a regular file owned by the current user; refusing to lock it",
                path.display()
            );
        }
    }
    Ok(f)
}

/// Ceiling on the lock wait. Legitimate holders (a predecessor's cleanup,
/// another daemon's startup) finish in milliseconds; anything longer means the
/// lock is wedged or squatted, so there is no reason to wait past this even
/// when the client is willing to.
const LOCK_WAIT_CEILING: Duration = Duration::from_secs(2);

/// How long startup will wait for the daemon-file lock, given the CLI's own
/// wait budget. Half the budget, capped at [`LOCK_WAIT_CEILING`]: the lock
/// wait must stay strictly shorter than the client's deadline, or a daemon
/// that spent the whole budget waiting here would bind at the exact moment
/// `wait_for_daemon` gives up, and the CLI would fall back to direct execution
/// despite a usable daemon. Halving (rather than subtracting a fixed margin)
/// keeps that true for every value `DAEMON_WAIT_TIMEOUT_SECS` accepts, and
/// leaves the other half for the pid write and bind that follow.
///
/// Kept pure so the relationship is unit-testable without setting env vars.
fn derive_lock_wait_timeout(client_timeout: Duration) -> Duration {
    (client_timeout / 2).min(LOCK_WAIT_CEILING)
}

/// The lock wait for this process. The daemon is spawned by the CLI and
/// inherits its environment, so both sides read the same
/// `DAEMON_WAIT_TIMEOUT_SECS` (default 5s, see `client.rs`).
fn lock_wait_timeout() -> Duration {
    derive_lock_wait_timeout(crate::client::daemon_wait_timeout())
}

/// Acquire the cross-process lock serializing the daemon-file critical
/// sections: startup's pid-write/rebind and cleanup's check-then-remove.
/// The OS releases the lock when the handle drops.
///
/// Errors instead of degrading: the lock is what makes ownership handoff
/// correct, and a daemon that can't create a file in temp_dir couldn't write
/// its PID file either — failing here just surfaces the cause sooner. The
/// wait is bounded because an indefinite `lock()` would let any process that
/// pre-acquired the predictable lock path park daemon startup forever.
/// Async (poll + `tokio::time::sleep`) so a contended lock never blocks the
/// runtime's worker thread.
async fn lock_daemon_files() -> Result<std::fs::File> {
    let f = open_lock_file()?;
    let timeout = lock_wait_timeout();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "Timed out after {timeout:?} waiting for daemon lock file {} (held by another process)",
                        lock_path().display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| {
                    format!("Failed to lock daemon lock file {}", lock_path().display())
                });
            }
        }
    }
}

/// Best-effort removal of the daemon's socket/address and PID files.
///
/// Only cleans up when the PID file still names this process: the paths are
/// shared, so a stale-but-alive old daemon exiting must not delete the files
/// of a newer daemon that has since rebound them. The ownership check and the
/// removals happen under the daemon-file lock so a replacement can't write
/// its pid and rebind in between (it would keep running but be unreachable,
/// and every later CLI call would spawn yet another daemon).
///
/// The `eprintln!` diagnostics here are only visible when `__daemon__` is run
/// in a foreground terminal: `spawn_daemon` detaches with stderr to null.
/// That's acceptable — every branch below fails safe (files are left in
/// place and self-heal on the next daemon start), so the messages exist for
/// interactive debugging, not operational monitoring.
fn cleanup() {
    #[cfg(unix)]
    let endpoint = socket_path();
    #[cfg(windows)]
    let endpoint = addr_path();
    cleanup_at(&lock_path(), &pid_path(), &endpoint);
}

/// Path-parameterized body of [`cleanup`] (see its doc for the locking and
/// ownership rules), so tests can drive it against a scratch directory
/// instead of the real `temp_dir()` files.
fn cleanup_at(lock: &std::path::Path, pid: &std::path::Path, endpoint: &std::path::Path) {
    // `lock_file` holds the OS lock until it drops at the end of this scope,
    // covering the ownership check and removals below.
    let lock_file = match open_lock_file_at(lock) {
        Ok(f) => f,
        Err(e) => {
            // Without the lock, removal could race a replacement's startup —
            // leaving the files is the safe side (they self-heal on the next
            // daemon start), but say why so the cause isn't swallowed.
            // open_lock_file_at's error already names the operation and path.
            eprintln!("daemon: leaving socket/PID files in place: {e:#}");
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
                lock.display()
            );
            return;
        }
    }
    let owns_files = std::fs::read_to_string(pid)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(std::process::id());
    if !owns_files {
        return;
    }
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_file(pid);
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

/// A macro (not a generic fn) because the Unix `UnixListener` and Windows
/// `TcpListener` have no common accept trait; the cfg-gated call site passes
/// whichever exists. Contract for callers:
/// - `$accept` is re-evaluated every iteration (pass `listener.accept()`,
///   which creates a fresh accept future each time around the loop).
/// - `$shutdown` is polled by `&mut` reference, so the caller must pin it
///   first (`tokio::pin!`); passing an unpinned future fails to compile.
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
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    // A plain fn returning a future (not an async fn) so the signal streams
    // are registered right here, when the caller sets up shutdown handling —
    // an async fn would defer registration to the first poll, leaving a
    // window during startup where a signal still hits the default
    // disposition and skips CleanupGuard.
    let sigterm = signal(SignalKind::terminate()).ok();
    let sigint = signal(SignalKind::interrupt()).ok();
    async move {
        // A stream that failed to register keeps its default disposition
        // (terminate without cleanup — as before this handler existed). One
        // that registered MUST still be drained here: registration replaces
        // the default handler, so ignoring it would make that signal a no-op
        // and the daemon unkillable by it.
        match (sigterm, sigint) {
            (Some(mut term), Some(mut int)) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            }
            (Some(mut term), None) => {
                term.recv().await;
            }
            (None, Some(mut int)) => {
                int.recv().await;
            }
            (None, None) => std::future::pending().await,
        }
    }
}

/// Best-effort on Windows: the daemon is spawned with CREATE_NO_WINDOW (no
/// console), so SetConsoleCtrlHandler-based Ctrl-C delivery typically never
/// fires for a backgrounded daemon. It does work when `__daemon__` is run
/// manually in a foreground console for debugging.
#[cfg(windows)]
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    // Registered eagerly for the same reason as the Unix version.
    let ctrl_c = tokio::signal::windows::ctrl_c().ok();
    async move {
        match ctrl_c {
            Some(mut c) => {
                c.recv().await;
            }
            None => std::future::pending().await,
        }
    }
}

pub async fn run_daemon(ws_url: &str) -> Result<()> {
    // Armed before anything is written: cleanup() verifies pid-file ownership
    // first, so firing "too early" is a no-op, and this declaration order
    // means the startup lock below is released (locals drop in reverse order)
    // before the guard's cleanup() tries to take it — no self-deadlock on an
    // early `?` return or panic. Covers every way this frame is left,
    // including unwinding panics, which previously leaked the files.
    let _guard = CleanupGuard;

    // Signal streams are registered here — before the lock wait and bind —
    // so a SIGTERM during startup is buffered for the accept loop instead of
    // killing the process without cleanup.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // Startup critical section: the pid write and endpoint (re)bind must not
    // interleave with a predecessor's cleanup() check-then-remove, or the
    // predecessor can delete files this daemon just claimed.
    let startup_lock = lock_daemon_files().await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_wait_stays_shorter_than_every_client_deadline() {
        // Any value DAEMON_WAIT_TIMEOUT_SECS accepts, not just the default:
        // an equal deadline lets the daemon bind exactly as the CLI gives up.
        for secs in [1_u64, 2, 3, 5, 10, 60, 3600] {
            let client = Duration::from_secs(secs);
            let lock = derive_lock_wait_timeout(client);
            assert!(
                lock < client,
                "lock wait {lock:?} must be shorter than client deadline {client:?}"
            );
            assert!(lock <= LOCK_WAIT_CEILING);
        }
        // A zero budget can't be beaten, only matched: one try_lock, no wait.
        assert_eq!(derive_lock_wait_timeout(Duration::ZERO), Duration::ZERO);
    }

    #[cfg(unix)]
    #[test]
    fn test_open_lock_file_at_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("daemon.lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // O_NOFOLLOW must refuse a planted symlink even though its target is
        // a perfectly ordinary file owned by us.
        assert!(open_lock_file_at(&link).is_err());
        // And the symlink must not have been replaced or followed-through.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn test_open_lock_file_at_rejects_fifo() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("daemon.lock");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo only reads the NUL-terminated path we just built.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // O_NONBLOCK makes the write-only open of a reader-less FIFO fail
        // with ENXIO instead of blocking forever.
        assert!(open_lock_file_at(&fifo).is_err());
    }

    #[test]
    fn test_open_lock_file_at_accepts_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        assert!(open_lock_file_at(&lock).is_ok(), "creates when absent");
        assert!(open_lock_file_at(&lock).is_ok(), "reopens when present");
    }

    /// The invariant the whole ownership handoff rests on: a daemon whose PID
    /// file has been rebound to another process must not remove the files.
    #[test]
    fn test_cleanup_at_leaves_files_of_foreign_pid() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        let pid = dir.path().join("daemon.pid");
        let endpoint = dir.path().join("daemon.sock");
        std::fs::write(&pid, std::process::id().wrapping_add(1).to_string()).unwrap();
        std::fs::write(&endpoint, "").unwrap();
        cleanup_at(&lock, &pid, &endpoint);
        assert!(pid.exists(), "foreign PID file must survive cleanup");
        assert!(
            endpoint.exists(),
            "foreign endpoint file must survive cleanup"
        );
    }

    #[test]
    fn test_cleanup_at_removes_own_files_but_never_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        let pid = dir.path().join("daemon.pid");
        let endpoint = dir.path().join("daemon.sock");
        std::fs::write(&pid, std::process::id().to_string()).unwrap();
        std::fs::write(&endpoint, "").unwrap();
        cleanup_at(&lock, &pid, &endpoint);
        assert!(!pid.exists());
        assert!(!endpoint.exists());
        assert!(
            lock.exists(),
            "the lock file is intentionally never removed"
        );
    }

    /// flock is per open-file-description, so a second handle in this same
    /// process contends like another process would.
    #[test]
    fn test_cleanup_at_backs_off_when_lock_contended() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("daemon.lock");
        let pid = dir.path().join("daemon.pid");
        let endpoint = dir.path().join("daemon.sock");
        std::fs::write(&pid, std::process::id().to_string()).unwrap();
        std::fs::write(&endpoint, "").unwrap();
        let holder = open_lock_file_at(&lock).unwrap();
        holder.try_lock().unwrap();
        // A replacement holds the lock (mid-startup): even our own files must
        // be left alone, since the replacement may be about to rebind them.
        cleanup_at(&lock, &pid, &endpoint);
        assert!(pid.exists());
        assert!(endpoint.exists());
    }
}

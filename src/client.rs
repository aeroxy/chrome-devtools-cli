use anyhow::{Context, Result};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::time::{Duration, SystemTime};
#[cfg(windows)]
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::protocol::*;

/// Marker for a daemon socket that answers but belongs to another user. Kept
/// as a type so `wait_for_daemon` can tell it from a socket simply not yet
/// listening and carry the reason into its startup error.
#[derive(Debug)]
struct PeerRefused;

impl std::fmt::Display for PeerRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the daemon socket is served by another user's process")
    }
}

impl std::error::Error for PeerRefused {}

#[cfg(unix)]
async fn connect_daemon(key: &str) -> Result<UnixStream> {
    let path = socket_path(key);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("Failed to connect to daemon {key} at {}", path.display()))?;
    if !peer_is_this_user(&stream) {
        return Err(anyhow::Error::new(PeerRefused)).context(format!(
            "The daemon socket {} is served by another user's process, so this command was not \
             sent to it. Remove the socket if it is stale, or ask whoever owns it.",
            path.display()
        ));
    }
    Ok(stream)
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
    daemon_command(&std::env::current_exe()?, ws_url, browser).spawn()?;
    Ok(())
}

/// The endpoint goes in the environment, not the arguments (see
/// [`crate::daemon::WS_URL_ENV`]).
fn daemon_command(exe: &std::path::Path, ws_url: &str, browser: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["__daemon__", browser])
        .env(crate::daemon::WS_URL_ENV, ws_url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

    cmd
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
    // A socket answered but rejected us: that is not the "not listening yet"
    // case this loop waits out, so keep its reason for the timeout error.
    let mut refusal: Option<anyhow::Error> = None;
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(daemon_start_timeout(refusal.as_ref()));
        }
        match connect_daemon(key).await {
            Ok(_) => return Ok(()),
            Err(e) if e.downcast_ref::<PeerRefused>().is_some() => refusal = Some(e),
            Err(_) => {}
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

/// The startup error keeps a peer refusal's reason; a socket that was simply
/// not listening yet still gets the plain timeout.
fn daemon_start_timeout(refusal: Option<&anyhow::Error>) -> anyhow::Error {
    let base = format!(
        "Daemon failed to start within {} seconds",
        daemon_wait_timeout().as_secs()
    );
    match refusal {
        Some(e) => anyhow::anyhow!("{base}: {e:#}"),
        None => anyhow::anyhow!("{base}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_refusal_survives_the_startup_timeout() {
        let refusal = anyhow::Error::new(PeerRefused).context("socket owned by somebody else");
        let error = daemon_start_timeout(Some(&refusal));
        assert!(
            error.to_string().contains("socket owned by somebody else"),
            "{error:#}"
        );
        let plain = daemon_start_timeout(None);
        assert!(
            plain.to_string().starts_with("Daemon failed to start"),
            "{plain:#}"
        );
    }

    #[test]
    fn daemon_endpoint_stays_out_of_its_arguments() {
        let url = "wss://cdp.example/devtools/browser?token=secret";
        let cmd = daemon_command(std::path::Path::new("chrome-devtools"), url, "chrome");

        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["__daemon__", "chrome"]);
        let env = cmd
            .get_envs()
            .find(|(k, _)| *k == crate::daemon::WS_URL_ENV)
            .and_then(|(_, v)| v);
        assert_eq!(env, Some(std::ffi::OsStr::new(url)));
    }
}

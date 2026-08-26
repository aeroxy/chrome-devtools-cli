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
    pub unblock_url: Vec<String>,
}

impl DaemonRequest {
    /// Resolve the output format, preferring the new `output_format` field
    /// and falling back to the legacy `json_output` bool.
    pub fn format(&self) -> OutputFormat {
        self.output_format.unwrap_or(if self.json_output {
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

/// Per-user filename suffix. `temp_dir()` is per-user on macOS ($TMPDIR) and
/// Windows (%TEMP%), but shared /tmp on Linux — a fixed name there lets users
/// collide on (or squat) each other's daemon files.
///
/// Effective uid, not real: the kernel stamps a new file with the creating
/// process's *effective* uid, and that is what the daemon's ownership checks
/// compare against. Keying the name to the real uid instead would make the two
/// disagree wherever they differ (a setuid binary, a credential-transitioning
/// launcher): the daemon would create its files at a path derived from one
/// identity and then refuse them as belonging to another user.
#[cfg(unix)]
fn user_suffix() -> std::borrow::Cow<'static, str> {
    // SAFETY: geteuid() is a pure kernel query with no preconditions; it is
    // thread-safe and cannot fail.
    std::borrow::Cow::Owned(format!("-{}", unsafe { libc::geteuid() }))
}

#[cfg(windows)]
fn user_suffix() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("")
}

/// Identity of a single daemon instance: a short hash of the resolved
/// WebSocket endpoint.
///
/// A daemon owns exactly one live CDP connection, so its identity *is* that
/// connection. Keying the socket/PID/info filenames by endpoint gives Chrome,
/// Edge, every release channel and every headless instance a daemon of its
/// own, which is what stops a command aimed at one browser from being served
/// by a daemon attached to another — the failure mode when a single per-user
/// daemon bound to whichever browser happened to be resolved first, and every
/// later `--browser`/`--user-data-dir` flag was silently ignored.
///
/// The endpoint's browser GUID changes on every browser launch, so a restarted
/// browser deliberately gets a fresh daemon instead of inheriting a dead
/// connection. The orphan holds no lock and exits on its own idle timeout.
pub fn instance_key(ws_url: &str) -> String {
    // FNV-1a, 64-bit. Not cryptographic, and does not need to be: the input is
    // a loopback URL this process just derived, not attacker-chosen, and a
    // collision would only mean two browser sessions sharing one daemon —
    // exactly the behavior that predates keying.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ws_url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Shared leading part of every per-user daemon filename.
fn daemon_file_prefix() -> String {
    format!("chrome-devtools-daemon{}", user_suffix())
}

/// Path to the Unix domain socket for daemon communication.
#[cfg(unix)]
pub fn socket_path(key: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}-{key}.sock", daemon_file_prefix()))
}

/// Path to the named-pipe address file for daemon communication (Windows).
#[cfg(windows)]
pub fn addr_path(key: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}-{key}.addr", daemon_file_prefix()))
}

/// Path to the daemon PID file.
pub fn pid_path(key: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}-{key}.pid", daemon_file_prefix()))
}

/// Path to the daemon's metadata sidecar (see [`DaemonInfo`]).
pub fn info_path(key: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}-{key}.info", daemon_file_prefix()))
}

/// Path to the lock file serializing daemon startup and cleanup.
///
/// Deliberately *not* keyed per instance. It only serializes the brief
/// write-pid-then-bind critical section, so one lock for all instances costs
/// nothing measurable — while a per-instance lock would accumulate a file per
/// browser session forever, since the lock file is never removed once created:
/// deleting it while another process may be about to lock it would reintroduce
/// the race it prevents.
pub fn lock_path() -> PathBuf {
    std::env::temp_dir().join(format!("{}.lock", daemon_file_prefix()))
}

/// Every daemon instance key with a PID file belonging to this user.
///
/// Used by `list-daemons` and `kill-daemon --all`. Unreadable temp dirs and
/// non-UTF-8 names are skipped rather than reported: a name we cannot parse is
/// not a daemon of ours.
pub fn enumerate_instance_keys() -> Vec<String> {
    let prefix = format!("{}-", daemon_file_prefix());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let key = name.strip_prefix(&prefix)?.strip_suffix(".pid")?;
            (!key.is_empty()).then(|| key.to_string())
        })
        .collect();
    keys.sort();
    keys
}

/// Metadata a daemon publishes about itself, so `list-daemons` can name the
/// browser it is attached to without inspecting process arguments.
///
/// Best-effort and purely descriptive: the daemon works if this file is
/// missing, and readers must tolerate its absence.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DaemonInfo {
    /// Browser the daemon was spawned for, as passed to `--browser`.
    pub browser: String,
    /// The resolved endpoint this daemon is attached to.
    pub ws_url: String,
    pub pid: u32,
    /// Unix seconds at daemon start, for the uptime column.
    pub started_unix: u64,
}

/// Pre-uid-suffix PID file name, so `kill-daemon` can still stop a daemon
/// left running by an older binary after an upgrade.
#[cfg(unix)]
pub fn legacy_pid_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.pid")
}

/// Pre-instance-key PID file name: `chrome-devtools-daemon-<uid>.pid`, written
/// by versions that ran one daemon per user. Swept by `kill-daemon --all` so an
/// upgrade doesn't leave an unreachable daemon holding a CDP connection.
pub fn legacy_unkeyed_pid_path() -> PathBuf {
    std::env::temp_dir().join(format!("{}.pid", daemon_file_prefix()))
}

/// Pre-instance-key socket name (see [`legacy_unkeyed_pid_path`]).
#[cfg(unix)]
pub fn legacy_unkeyed_socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("{}.sock", daemon_file_prefix()))
}

/// Pre-uid-suffix socket name (see [`legacy_pid_path`]).
#[cfg(unix)]
pub fn legacy_socket_path() -> PathBuf {
    std::env::temp_dir().join("chrome-devtools-daemon.sock")
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
    let len =
        usize::try_from(u32::from_be_bytes(len_buf)).context("Message length overflows usize")?;
    if len > 64 * 1024 * 1024 {
        anyhow::bail!("Message too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_key_is_stable_and_endpoint_specific() {
        let chrome = "ws://127.0.0.1:9222/devtools/browser/aaaa";
        let edge = "ws://127.0.0.1:56912/devtools/browser/bbbb";
        assert_eq!(instance_key(chrome), instance_key(chrome), "deterministic");
        assert_ne!(
            instance_key(chrome),
            instance_key(edge),
            "different browsers must not share a daemon"
        );
        assert_eq!(instance_key(chrome).len(), 16);
        assert!(instance_key(chrome).chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// The GUID is what makes a restarted browser a new daemon rather than one
    /// inheriting a dead CDP connection.
    #[test]
    fn instance_key_distinguishes_browser_sessions_on_one_port() {
        let before = "ws://127.0.0.1:9222/devtools/browser/session-one";
        let after = "ws://127.0.0.1:9222/devtools/browser/session-two";
        assert_ne!(instance_key(before), instance_key(after));
    }

    #[test]
    fn keyed_paths_are_distinct_and_lock_is_shared() {
        let a = instance_key("ws://127.0.0.1:9222/devtools/browser/a");
        let b = instance_key("ws://127.0.0.1:9223/devtools/browser/b");
        assert_ne!(pid_path(&a), pid_path(&b));
        assert_ne!(info_path(&a), info_path(&b));
        // One lock for all instances: it only serializes the brief
        // write-pid-then-bind window, and a per-instance lock would accumulate
        // a never-removed file per browser session.
        assert_eq!(lock_path(), lock_path());
        assert!(!lock_path().to_string_lossy().contains(&a));
    }

    /// The pre-key name must not be mistaken for an instance: it has no key
    /// segment, and sweeping it as one would derive a bogus socket path.
    #[test]
    fn legacy_unkeyed_pid_file_is_not_parsed_as_an_instance() {
        let legacy = legacy_unkeyed_pid_path();
        let name = legacy.file_name().unwrap().to_string_lossy().to_string();
        let prefix = format!("{}-", daemon_file_prefix());
        assert!(
            name.strip_prefix(&prefix).is_none(),
            "legacy name {name} must not match the keyed prefix {prefix}"
        );
    }

    #[test]
    fn daemon_info_round_trips() {
        let info = DaemonInfo {
            browser: "edge".to_string(),
            ws_url: "ws://127.0.0.1:56912/devtools/browser/x".to_string(),
            pid: 4242,
            started_unix: 1_700_000_000,
        };
        let back: DaemonInfo = serde_json::from_slice(&serde_json::to_vec(&info).unwrap()).unwrap();
        assert_eq!(back.browser, "edge");
        assert_eq!(back.pid, 4242);
        assert_eq!(back.ws_url, info.ws_url);
    }
}

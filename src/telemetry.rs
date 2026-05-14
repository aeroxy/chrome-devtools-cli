use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

/// A single telemetry entry to be written to the log file.
#[derive(Debug)]
struct LogEntry {
    path: PathBuf,
    line: String,
}

/// Background worker that owns the log writer thread.
#[derive(Debug)]
struct TelemetryWorker {
    sender: Sender<LogEntry>,
}

impl TelemetryWorker {
    /// Spawn the background thread and return a handle to send it work.
    fn spawn() -> Self {
        let (sender, receiver) = channel::<LogEntry>();
        std::thread::spawn(move || {
            while let Ok(entry) = receiver.recv() {
                let _ = Self::write_entry(&entry.path, &entry.line);
            }
        });
        Self { sender }
    }

    fn send(&self, entry: LogEntry) {
        // Best-effort: drop the entry if the channel is full or closed
        let _ = self.sender.send(entry);
    }

    fn write_entry(path: &PathBuf, line: &str) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

/// Simple file-based telemetry logger.
///
/// Logs command invocations as JSON lines to a log file in the user's
/// config directory. Designed to be lightweight and non-blocking.
///
/// Uses a single background thread fed by a channel instead of spawning
/// a new thread for every log entry.
#[derive(Debug)]
pub struct TelemetryLogger {
    log_dir: PathBuf,
    worker: TelemetryWorker,
}

impl TelemetryLogger {
    /// Create a new logger that writes to the given directory.
    pub fn new(log_dir: PathBuf) -> Self {
        // Best-effort directory creation
        let _ = fs::create_dir_all(&log_dir);
        Self {
            log_dir,
            worker: TelemetryWorker::spawn(),
        }
    }

    /// Default log directory: `~/.chrome-devtools-cli/logs/`
    #[allow(dead_code)]
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".chrome-devtools-cli")
            .join("logs")
    }

    /// Get the current log file path (date-based rotation).
    fn log_path(&self) -> PathBuf {
        let date = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() / 86400)
            .unwrap_or(0);
        self.log_dir.join(format!("telemetry-{date}.log"))
    }

    /// Log a command invocation.
    ///
    /// Non-blocking and best-effort — failures are silently ignored
    /// to avoid disrupting the user's workflow.
    pub fn log_command(
        &self,
        command: &str,
        duration: Duration,
        success: bool,
        error_code: Option<u32>,
    ) {
        let entry = json!({
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            "command": command,
            "duration_ms": duration.as_millis() as u64,
            "success": success,
            "error_code": error_code,
        });
        self.worker.send(LogEntry {
            path: self.log_path(),
            line: entry.to_string(),
        });
    }
}

/// Global telemetry logger instance.
///
/// Initialized once in `main.rs` or `daemon.rs`.
static LOGGER: std::sync::OnceLock<TelemetryLogger> = std::sync::OnceLock::new();

/// Initialize the global telemetry logger.
///
/// Idempotent — repeated calls are silently ignored so tests and
/// downstream callers don't need coordination.
pub fn init_logger(logger: TelemetryLogger) {
    let _ = LOGGER.set(logger);
}

/// Get a reference to the global logger, if initialized.
fn logger() -> Option<&'static TelemetryLogger> {
    LOGGER.get()
}

/// Log a command invocation to the global logger.
///
/// No-op if the logger hasn't been initialized.
pub fn log_command(command: &str, duration: Duration, success: bool, error_code: Option<u32>) {
    if let Some(logger) = logger() {
        logger.log_command(command, duration, success, error_code);
    }
}

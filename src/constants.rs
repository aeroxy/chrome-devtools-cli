/// Default timeout for page navigation (30 seconds)
pub const NAVIGATION_TIMEOUT_MS: u64 = 30_000;

/// Polling interval for the injected `ctx` wait helpers (waitForText /
/// waitForSelector) in run-script and adapter execution.
pub const POLL_INTERVAL_MS: u64 = 100;

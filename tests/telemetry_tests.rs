/// Test that the telemetry module is accessible.
#[test]
fn test_telemetry_module_accessible() {
    use chrome_devtools_cli::telemetry;
    assert!(true);
}

/// Test that init_logger and log_command are callable.
#[test]
fn test_telemetry_log_command() {
    use chrome_devtools_cli::telemetry::{init_logger, log_command, TelemetryLogger};
    use std::time::Duration;

    // Create a logger and initialize
    let logger = TelemetryLogger::new(std::path::PathBuf::from("/tmp/test_telemetry_cmd"));
    init_logger(logger);

    // This should not panic — log is best-effort
    log_command("test-command", Duration::from_millis(42), true, None);

    // Give the background thread a moment to write
    std::thread::sleep(Duration::from_millis(200));
}

/// Test that default_dir produces a path under the home directory.
#[test]
fn test_telemetry_default_dir() {
    use chrome_devtools_cli::telemetry::TelemetryLogger;

    let dir = TelemetryLogger::default_dir();
    assert!(dir.ends_with(".chrome-devtools-cli/logs"));
}
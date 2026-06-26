/// Test that the telemetry module is accessible.
#[test]
fn test_telemetry_module_accessible() {
    use chrome_devtools_cli::telemetry;
    // Reference the module so the import isn't flagged unused; this test exists
    // to assert the module path compiles (i.e. is publicly exported).
    let _ = telemetry::init_logger_once;
}

/// Test that TelemetryLogger writes a valid JSON entry and cleans up.
#[test]
fn test_telemetry_log_command() {
    use chrome_devtools_cli::telemetry::{init_logger_once, log_command};
    use std::time::Duration;

    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    init_logger_once(tmp.path().to_path_buf());

    log_command("test-command", Duration::from_millis(42), true, None);

    // Give the background thread a moment to write
    std::thread::sleep(Duration::from_millis(200));

    // Find the log file — it's date-based so there should be exactly one
    let log_files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("temp dir should exist")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(log_files.len(), 1, "expected exactly one log file");

    let content =
        std::fs::read_to_string(log_files[0].path()).expect("should be able to read log file");

    // The entry should be valid JSON with the command we logged
    let parsed: serde_json::Value =
        serde_json::from_str(content.trim()).expect("log entry should be valid JSON");
    assert_eq!(parsed["command"], "test-command");
    assert_eq!(parsed["duration_ms"], 42);
    assert_eq!(parsed["success"], true);

    // tmp (TempDir) is dropped here, cleaning up the directory
}

/// Test that default_dir produces a path under the home directory.
#[test]
fn test_telemetry_default_dir() {
    use chrome_devtools_cli::telemetry::TelemetryLogger;

    let dir = TelemetryLogger::default_dir();
    assert!(dir.ends_with(".chrome-devtools-cli/logs"));
}

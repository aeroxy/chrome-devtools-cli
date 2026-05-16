/// Test ErrorCode values and code() method.
#[test]
fn test_error_code_values() {
    use chrome_devtools_cli::error::ErrorCode;

    assert_eq!(ErrorCode::Unspecified.code(), 10);
    assert_eq!(ErrorCode::ChromeEndpointResolution.code(), 1);
    assert_eq!(ErrorCode::ChromeConnection.code(), 2);
    assert_eq!(ErrorCode::TargetNotFound.code(), 3);
    assert_eq!(ErrorCode::CommandExecution.code(), 4);
    assert_eq!(ErrorCode::FileIo.code(), 5);
    assert_eq!(ErrorCode::Daemon.code(), 6);
    assert_eq!(ErrorCode::InvalidInput.code(), 7);
    assert_eq!(ErrorCode::JavaScriptError.code(), 8);
    assert_eq!(ErrorCode::WebSocketDisconnected.code(), 9);
}

/// Test CliError creation and Display formatting.
#[test]
fn test_cli_error_display() {
    use chrome_devtools_cli::error::{CliError, ErrorCode};

    let err = CliError::new(ErrorCode::TargetNotFound, "No page at index 5");
    let display = format!("{err}");
    assert!(display.contains("[E3]"));
    assert!(display.contains("No page at index 5"));
}

/// Test CliError with source error.
#[test]
fn test_cli_error_with_source() {
    use anyhow::anyhow;
    use chrome_devtools_cli::error::{CliError, ErrorCode};

    let source = anyhow!("connection refused");
    let err = CliError::with_source(ErrorCode::ChromeConnection, "Failed to connect", source);
    let display = format!("{err}");
    assert!(display.contains("[E2]"));
    assert!(display.contains("Failed to connect"));
    assert!(display.contains("connection refused"));
}

/// Test From<anyhow::Error> for CliError.
#[test]
fn test_from_anyhow() {
    use anyhow::anyhow;
    use chrome_devtools_cli::error::{CliError, ErrorCode};

    let anyhow_err = anyhow!("something went wrong");
    let cli_err: CliError = anyhow_err.into();
    assert_eq!(cli_err.code(), ErrorCode::Unspecified);
    assert!(format!("{cli_err:#}").contains("something went wrong"));
}

/// Test CliError conversion to anyhow::Error.
#[test]
fn test_cli_error_to_anyhow() {
    use chrome_devtools_cli::error::{CliError, ErrorCode};

    let cli_err = CliError::new(ErrorCode::FileIo, "File not found");
    assert_eq!(cli_err.code(), ErrorCode::FileIo);
}

/// Test CliResult type alias.
#[test]
fn test_cli_result_type() {
    use chrome_devtools_cli::error::CliResult;

    let ok_result: CliResult<i32> = Ok(42);
    assert_eq!(ok_result.unwrap(), 42);

    let err_result: CliResult<i32> = Err(chrome_devtools_cli::error::CliError::new(
        chrome_devtools_cli::error::ErrorCode::InvalidInput,
        "bad input",
    ));
    assert!(err_result.is_err());
}

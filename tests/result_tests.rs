/// Test that CommandResult correctly propagates navigation info.
#[test]
fn test_command_result_no_navigation() {
    use chrome_devtools_cli::result::CommandResult;

    let result = CommandResult::output("hello");
    assert_eq!(result.output, "hello");
    assert!(result.navigated_to.is_none());
    assert!(result.target_id.is_none());
    assert!(result.error_code.is_none());
}

/// Test that with_navigated_to_if_changed only sets navigated_to when URL changed.
#[test]
fn test_command_result_navigation_changed() {
    use chrome_devtools_cli::result::CommandResult;

    let result = CommandResult::output("hello").with_navigated_to_if_changed(
        "https://example.com/new".to_string(),
        "https://example.com/old".to_string(),
    );
    assert_eq!(
        result.navigated_to,
        Some("https://example.com/new".to_string())
    );
}

/// Test that with_navigated_to_if_changed does NOT set navigated_to when URL unchanged.
#[test]
fn test_command_result_navigation_unchanged() {
    use chrome_devtools_cli::result::CommandResult;

    let result = CommandResult::output("hello").with_navigated_to_if_changed(
        "https://example.com/same".to_string(),
        "https://example.com/same".to_string(),
    );
    assert!(result.navigated_to.is_none());
}

/// Test with_target_id builder.
#[test]
fn test_command_result_target_id() {
    use chrome_devtools_cli::result::CommandResult;

    let result = CommandResult::output("hello").with_target_id("abc123");
    assert_eq!(result.target_id, Some("abc123".to_string()));
}

/// Test with_error_code builder.
#[test]
fn test_command_result_error_code() {
    use chrome_devtools_cli::result::CommandResult;

    let result = CommandResult::output("hello").with_error_code(5);
    assert_eq!(result.error_code, Some(5));
}

/// Test builder chaining.
#[test]
fn test_command_result_chaining() {
    use chrome_devtools_cli::result::CommandResult;

    let result = CommandResult::output("hello")
        .with_navigated_to("https://example.com")
        .with_target_id("tab-1")
        .with_error_code(42);

    assert_eq!(result.output, "hello");
    assert_eq!(result.navigated_to, Some("https://example.com".to_string()));
    assert_eq!(result.target_id, Some("tab-1".to_string()));
    assert_eq!(result.error_code, Some(42));
}

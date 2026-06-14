/// Test executor dispatch for browser-level commands.
#[cfg(test)]
mod tests {
    use chrome_devtools_cli::commands::executor;
    use chrome_devtools_cli::protocol::DaemonRequest;
    use serde_json::json;

    // TODO: Add mock CdpClient for executor integration tests.
    // This requires the cdp module to be refactored to support dependency injection
    // or a trait-based interface, which is tracked for a future refactoring.

    /// Verify that DaemonRequest can be constructed for each command variant.
    #[test]
    fn test_daemon_request_construction() {
        let req = DaemonRequest {
            command: "list-pages".to_string(),
            args: json!({}),
            page: None,
            target: None,
            json_output: false,
            output_format: None,
            block_url: vec![],
            allow_url: vec![],
        };
        assert_eq!(req.command, "list-pages");

        let req = DaemonRequest {
            command: "navigate".to_string(),
            args: json!({"url": "https://example.com", "back": false, "forward": false, "reload": false}),
            page: None,
            target: None,
            json_output: false,
            output_format: None,
            block_url: vec![],
            allow_url: vec![],
        };
        assert_eq!(req.command, "navigate");

        let req = DaemonRequest {
            command: "click".to_string(),
            args: json!({"selector": "button.submit"}),
            page: None,
            target: None,
            json_output: false,
            output_format: None,
            block_url: vec![],
            allow_url: vec![],
        };
        assert_eq!(req.command, "click");
    }

    /// Verify that known_args stays in sync with the Clap Command definition.
    /// This test dynamically inspects the Cli struct's subcommands and asserts
    /// that known_args matches each subcommand's arguments, guaranteeing they
    /// never get out of sync when CLI flags are added or removed.
    #[test]
    fn test_known_args_sync_with_clap() {
        use clap::CommandFactory;
        use chrome_devtools_cli::Cli;

        let cmd = Cli::command();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            let mut expected_args: Vec<String> = sub
                .get_arguments()
                .filter(|a| !a.is_global_set() && a.get_id() != "help" && a.get_id() != "version")
                .map(|a| a.get_id().as_str().replace('-', "_"))
                .collect();
            let mut actual_args: Vec<String> = executor::known_args(name)
                .iter()
                .map(|s| s.to_string())
                .collect();
            expected_args.sort();
            actual_args.sort();
            assert_eq!(
                actual_args, expected_args,
                "known_args mismatch for command '{}'. Please update known_args in executor.rs.",
                name
            );
        }
    }
}

use chrome_devtools_cli::{daemon, error::ErrorCode, telemetry};

#[tokio::main]
async fn main() {
    // Initialize telemetry logger (non-blocking, best-effort)
    let log_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".chrome-devtools-cli")
        .join("logs");
    telemetry::init_logger_once(log_dir);

    // Internal daemon mode — invoked by spawn_daemon(), not by users
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("__daemon__") {
        let ws_url = args.get(2).expect("daemon requires ws_url argument");
        let result = daemon::run_daemon(ws_url).await;
        telemetry::shutdown_logger();
        if let Err(e) = result {
            eprintln!("daemon error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let result = chrome_devtools_cli::run().await;
    telemetry::shutdown_logger();
    if let Err(e) = result {
        let code = match e.downcast_ref::<chrome_devtools_cli::error::CliError>() {
            Some(ce) => ce.code().code(),
            None => ErrorCode::Unspecified as u32,
        };
        eprintln!("error: {e:#}");
        std::process::exit(code as i32);
    }
}

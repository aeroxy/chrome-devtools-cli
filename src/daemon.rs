use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::cdp::CdpClient;
use crate::commands::executor;
use crate::error::ErrorCode;
use crate::protocol::*;
use crate::telemetry;

/// Read the daemon idle timeout from `DAEMON_IDLE_TIMEOUT_SECS`, defaulting to 300 (5 minutes).
fn idle_timeout() -> Duration {
    std::env::var("DAEMON_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300))
}

/// Result of handling a single daemon connection.
enum ConnectionOutcome {
    /// Continue accepting new connections.
    Continue,
    /// Fatal error occurred; daemon should exit.
    Fatal,
}

macro_rules! run_accept_loop_body {
    ($accept:expr, $client:expr, $ws_url:expr) => {
        loop {
            let accept = tokio::time::timeout(idle_timeout(), $accept).await;

            match accept {
                Ok(Ok((stream, _))) => match handle_connection(stream, $client, $ws_url).await {
                    ConnectionOutcome::Continue => {}
                    ConnectionOutcome::Fatal => break,
                },
                Ok(Err(e)) => {
                    eprintln!("daemon: accept error: {e}");
                }
                Err(_) => {
                    // Idle timeout — exit
                    break;
                }
            }
        }
    };
}

pub async fn run_daemon(ws_url: &str) -> Result<()> {
    // Write PID
    std::fs::write(pid_path(), std::process::id().to_string())?;

    #[cfg(unix)]
    let listener = {
        // Clean up stale socket
        let sock = socket_path();
        let _ = std::fs::remove_file(&sock);

        // Bind socket FIRST so the CLI knows the daemon is alive and can connect.
        // If we wait for CdpClient::connect first, a macOS network permission prompt
        // can block the daemon and cause the CLI's 5-second wait_for_daemon timeout to expire.
        UnixListener::bind(&sock)?
    };

    #[cfg(windows)]
    let listener = {
        // Clean up stale address file
        let _ = std::fs::remove_file(addr_path());

        // Bind listener FIRST so the CLI knows the daemon is alive and can connect.
        // If we wait for CdpClient::connect first, a Chrome/network permission prompt
        // can block the daemon and cause the CLI's 5-second wait_for_daemon timeout to expire.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        std::fs::write(addr_path(), listener.local_addr()?.to_string())?;
        listener
    };

    // We don't connect immediately. We wait for the first connection from the CLI.
    // This ensures the CLI wait_for_daemon() succeeds, and the CLI blocks on read_msg()
    // while the daemon handles the potentially slow macOS/Chrome network permission prompt.
    let mut client: Option<CdpClient> = None;

    // Signal readiness by socket/address existence (it's already bound)
    run_accept_loop_body!(listener.accept(), &mut client, ws_url);

    #[cfg(unix)]
    let _ = std::fs::remove_file(socket_path());

    #[cfg(windows)]
    let _ = std::fs::remove_file(addr_path());

    let _ = std::fs::remove_file(pid_path());

    // Shut down telemetry before exiting so the background thread
    // flushes pending entries and exits cleanly.
    telemetry::shutdown_logger();

    Ok(())
}

async fn handle_connection<S>(
    mut stream: S,
    client: &mut Option<CdpClient>,
    ws_url: &str,
) -> ConnectionOutcome
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let req_bytes = match read_msg(&mut stream).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("daemon: read error: {e}");
            return ConnectionOutcome::Continue;
        }
    };

    let request: DaemonRequest = match serde_json::from_slice(&req_bytes) {
        Ok(r) => r,
        Err(e) => {
            let resp = DaemonResponse {
                success: false,
                output: String::new(),
                error: format!("Invalid request: {e}"),
                navigated_to: None,
                error_code: Some(ErrorCode::InvalidInput as u32),
            };
            if let Ok(resp_bytes) = serde_json::to_vec(&resp) {
                let _ = write_msg(&mut stream, &resp_bytes).await;
            }
            return ConnectionOutcome::Continue;
        }
    };

    // Connect lazily
    if client.is_none() {
        match CdpClient::connect(ws_url).await {
            Ok(c) => *client = Some(c),
            Err(e) => {
                let resp = DaemonResponse {
                    success: false,
                    output: String::new(),
                    error: format!("Failed to connect to Chrome: {e:#}"),
                    navigated_to: None,
                    error_code: Some(ErrorCode::ChromeConnection as u32),
                };
                if let Ok(resp_bytes) = serde_json::to_vec(&resp) {
                    let _ = write_msg(&mut stream, &resp_bytes).await;
                }
                // Exit daemon if we can't connect, so the next CLI call will spawn a fresh daemon
                return ConnectionOutcome::Fatal;
            }
        }
    }

    let response = match client.as_mut() {
        Some(client) => handle_request(client, &request).await,
        None => DaemonResponse {
            success: false,
            output: String::new(),
            error: String::from("Failed to connect to Chrome: client initialization failed"),
            navigated_to: None,
            error_code: Some(ErrorCode::ChromeConnection as u32),
        },
    };

    // Check if the error indicates a disconnected WebSocket.
    // If so, we should exit the daemon so it can be respawned cleanly next time.
    let is_fatal = !response.success
        && (response.error.contains("WebSocket closed")
            || response.error.contains("WebSocket connection closed")
            || response.error.contains("WebSocket error"));

    if let Ok(resp_bytes) = serde_json::to_vec(&response) {
        let _ = write_msg(&mut stream, &resp_bytes).await;
    }

    if is_fatal {
        ConnectionOutcome::Fatal
    } else {
        ConnectionOutcome::Continue
    }
}

async fn handle_request(client: &mut CdpClient, req: &DaemonRequest) -> DaemonResponse {
    let start = std::time::Instant::now();
    let cmd_name = req.command.as_str();
    match executor::execute_command(client, req).await {
        Ok(result) => {
            let duration = start.elapsed();
            telemetry::log_command(cmd_name, duration, true, result.error_code);
            DaemonResponse {
                success: true,
                output: result.output,
                error: String::new(),
                navigated_to: result.navigated_to,
                error_code: result.error_code,
            }
        }
        Err(e) => {
            let duration = start.elapsed();
            let error_code = e
                .downcast_ref::<crate::error::CliError>()
                .map(|ce| ce.code().code());
            telemetry::log_command(cmd_name, duration, false, error_code);
            DaemonResponse {
                success: false,
                output: String::new(),
                error: format!("{e:#}"),
                navigated_to: None,
                error_code,
            }
        }
    }
}

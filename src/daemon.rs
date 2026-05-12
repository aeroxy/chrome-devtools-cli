use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::cdp::CdpClient;
use crate::commands;
use crate::friendly;
use crate::protocol::*;
use serde_json::json;

const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

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
            let accept = tokio::time::timeout(IDLE_TIMEOUT, $accept).await;

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
    Ok(())
}

async fn handle_connection<S>(mut stream: S, client: &mut Option<CdpClient>, ws_url: &str) -> ConnectionOutcome
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
            };
            let _ = write_msg(&mut stream, &serde_json::to_vec(&resp).unwrap()).await;
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
                };
                let _ = write_msg(&mut stream, &serde_json::to_vec(&resp).unwrap()).await;
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
    match execute_command(client, req).await {
        Ok(output) => DaemonResponse {
            success: true,
            output,
            error: String::new(),
        },
        Err(e) => DaemonResponse {
            success: false,
            output: String::new(),
            error: format!("{e:#}"),
        },
    }
}

fn is_browser_level(cmd: &str) -> bool {
    matches!(
        cmd,
        "list-pages" | "new-page" | "close-page" | "select-page"
    )
}

async fn execute_command(client: &mut CdpClient, req: &DaemonRequest) -> Result<String> {
    let args = &req.args;
    let cmd = req.command.as_str();

    if is_browser_level(cmd) {
        return match cmd {
            "list-pages" => commands::pages::list_pages(client, req.json_output).await,
            "new-page" => {
                let url = args["url"].as_str().ok_or(anyhow!("url required"))?;
                commands::pages::new_page(client, url).await
            }
            "close-page" => {
                let index = args["index"].as_u64().ok_or(anyhow!("index required"))? as usize;
                commands::pages::close_page(client, index).await
            }
            "select-page" => {
                let index = args["index"].as_u64().ok_or(anyhow!("index required"))? as usize;
                commands::pages::select_page(client, index).await
            }
            _ => unreachable!(),
        };
    }

    // Page-level: resolve and attach to target
    let target = client.resolve_page(req.target.as_deref(), req.page).await?;
    let target_id = target.target_id.clone();
    let session_id = client.attach_to_target(&target_id).await?;

    // Enable Page domain to receive dialog events for proactive rejection
    let _ = client
        .send_to_target(&session_id, "Page.enable", json!({}))
        .await;

    client.dialog_action = args["dialog_action"].as_str().map(|s| s.to_string());

    let result = match cmd {
        "navigate" => {
            commands::navigate::navigate(
                client,
                &session_id,
                args["url"].as_str(),
                args["back"].as_bool().unwrap_or(false),
                args["forward"].as_bool().unwrap_or(false),
                args["reload"].as_bool().unwrap_or(false),
            )
            .await
        }
        "screenshot" => {
            commands::screenshot::take_screenshot(
                client,
                &session_id,
                args["output"].as_str(),
                args["format"].as_str().unwrap_or("png"),
                args["full_page"].as_bool().unwrap_or(false),
            )
            .await
        }
        "evaluate" => {
            let expr = args["expression"]
                .as_str()
                .ok_or(anyhow!("expression required"))?;
            commands::evaluate::evaluate(client, &session_id, expr, req.json_output).await
        }
        "click" => {
            let sel = args["selector"]
                .as_str()
                .ok_or(anyhow!("selector required"))?;
            commands::input::click(client, &session_id, sel).await
        }
        "click-at" => {
            let x = args["x"].as_f64().ok_or(anyhow!("x required"))?;
            let y = args["y"].as_f64().ok_or(anyhow!("y required"))?;
            commands::input::click_at(client, &session_id, x, y).await
        }
        "fill" => {
            let sel = args["selector"]
                .as_str()
                .ok_or(anyhow!("selector required"))?;
            let val = args["value"].as_str().ok_or(anyhow!("value required"))?;
            commands::input::fill(client, &session_id, sel, val).await
        }
        "type-text" => {
            let text = args["text"].as_str().ok_or(anyhow!("text required"))?;
            commands::input::type_text(client, &session_id, text, args["submit_key"].as_str()).await
        }
        "press-key" => {
            let key = args["key"].as_str().ok_or(anyhow!("key required"))?;
            commands::input::press_key(client, &session_id, key).await
        }
        "hover" => {
            let sel = args["selector"]
                .as_str()
                .ok_or(anyhow!("selector required"))?;
            commands::input::hover(client, &session_id, sel).await
        }
        "snapshot" => commands::snapshot::take_snapshot(client, &session_id, req.json_output).await,
        "resize" => {
            let w = args["width"].as_u64().ok_or(anyhow!("width required"))? as u32;
            let h = args["height"].as_u64().ok_or(anyhow!("height required"))? as u32;
            commands::pages::resize(client, &session_id, w, h).await
        }
        "wait-for" => {
            let text = args["text"].as_str().ok_or(anyhow!("text required"))?;
            let timeout = args["timeout"].as_u64().unwrap_or(30000);
            commands::pages::wait_for(client, &session_id, text, timeout).await
        }
        "list-3p-tools" => {
            commands::third_party::list_3p_tools(client, &session_id, req.json_output).await
        }
        "execute-3p-tool" => {
            let name = args["name"].as_str().ok_or(anyhow!("name required"))?;
            commands::third_party::execute_3p_tool(
                client,
                &session_id,
                name,
                args["params"].as_str(),
                req.json_output,
            )
            .await
        }
        _ => Err(anyhow!("Unknown command: {cmd}")),
    };

    let _ = client.detach_from_target(&session_id).await;
    client.dialog_action = None;

    // Append target ID so the caller can pin subsequent commands to this page
    let name = friendly::to_friendly(&target_id);
    result.map(|output| format!("{output}\n[target:{name}]"))
}

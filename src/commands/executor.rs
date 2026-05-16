use anyhow::{anyhow, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::commands;
use crate::friendly;
use crate::protocol::DaemonRequest;
use crate::result::CommandResult;

/// Whether a command operates at the browser level (no page session needed).
fn is_browser_level(cmd: &str) -> bool {
    matches!(
        cmd,
        "list-pages" | "new-page" | "close-page" | "select-page"
    )
}

/// Execute a single command from a [`DaemonRequest`].
///
/// Handles target resolution, session attachment, dialog configuration,
/// and target ID enrichment on success.
pub async fn execute_command(client: &mut CdpClient, req: &DaemonRequest) -> Result<CommandResult> {
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
    client
        .send_to_target(&session_id, "Page.enable", json!({}))
        .await?;


    client.dialog_action = args["dialog_action"].as_str().map(|s| s.to_string());

    let result = match cmd {
        "navigate" => {
            commands::navigate::navigate(
                client,
                &session_id,
                args.get("url").and_then(|v| v.as_str()),
                args.get("back").and_then(|v| v.as_bool()).unwrap_or(false),
                args.get("forward")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                args.get("reload")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                args.get("output").and_then(|v| v.as_str()),
            )
            .await
        }
        "screenshot" => {
            commands::screenshot::take_screenshot(
                client,
                &session_id,
                args.get("output").and_then(|v| v.as_str()),
                args.get("format").and_then(|v| v.as_str()).unwrap_or("png"),
                args.get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            )
            .await
        }
        "evaluate" => match args.get("expression").and_then(|v| v.as_str()) {
            Some(expr) => {
                commands::evaluate::evaluate(
                    client,
                    &session_id,
                    expr,
                    req.json_output,
                    args.get("output").and_then(|v| v.as_str()),
                    args.get("track_navigation").and_then(|v| v.as_bool()).unwrap_or(false),
                )
                .await
            }
            None => Err(anyhow!("expression required")),
        },
        "click" => match args.get("selector").and_then(|v| v.as_str()) {
            Some(sel) => commands::input::click(client, &session_id, sel).await,
            None => Err(anyhow!("selector required")),
        },
        "click-at" => match (
            args.get("x").and_then(|v| v.as_f64()),
            args.get("y").and_then(|v| v.as_f64()),
        ) {
            (Some(x), Some(y)) => commands::input::click_at(client, &session_id, x, y, None).await,
            _ => Err(anyhow!("x and y required")),
        },
        "fill" => match (
            args.get("selector").and_then(|v| v.as_str()),
            args.get("value").and_then(|v| v.as_str()),
        ) {
            (Some(sel), Some(val)) => commands::input::fill(client, &session_id, sel, val).await,
            _ => Err(anyhow!("selector and value required")),
        },
        "type-text" => match args.get("text").and_then(|v| v.as_str()) {
            Some(text) => {
                commands::input::type_text(
                    client,
                    &session_id,
                    text,
                    args.get("submit_key").and_then(|v| v.as_str()),
                )
                .await
            }
            None => Err(anyhow!("text required")),
        },
        "press-key" => match args.get("key").and_then(|v| v.as_str()) {
            Some(key) => commands::input::press_key(client, &session_id, key).await,
            None => Err(anyhow!("key required")),
        },
        "hover" => match args.get("selector").and_then(|v| v.as_str()) {
            Some(sel) => commands::input::hover(client, &session_id, sel).await,
            None => Err(anyhow!("selector required")),
        },
        "snapshot" => {
            commands::snapshot::take_snapshot(
                client,
                &session_id,
                req.json_output,
                args.get("output").and_then(|v| v.as_str()),
            )
            .await
        }
        "resize" => match (
            args.get("width").and_then(|v| v.as_u64()),
            args.get("height").and_then(|v| v.as_u64()),
        ) {
            (Some(w), Some(h)) => {
                let w: u32 = w.try_into().map_err(|_| anyhow!("width too large"))?;
                let h: u32 = h.try_into().map_err(|_| anyhow!("height too large"))?;
                commands::pages::resize(client, &session_id, w, h).await
            }
            _ => Err(anyhow!("width and height required")),
        },
        "wait-for" => match args.get("text").and_then(|v| v.as_str()) {
            Some(text) => {
                let timeout = args
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                commands::pages::wait_for(client, &session_id, text, timeout).await
            }
            None => Err(anyhow!("text required")),
        },
        "list-3p-tools" => {
            commands::third_party::list_3p_tools(client, &session_id, req.json_output).await
        }
        "execute-3p-tool" => match args.get("name").and_then(|v| v.as_str()) {
            Some(name) => {
                commands::third_party::execute_3p_tool(
                    client,
                    &session_id,
                    name,
                    args.get("params").and_then(|v| v.as_str()),
                    req.json_output,
                )
                .await
            }
            None => Err(anyhow!("name required")),
        },
        _ => Err(anyhow!("Unknown command: {cmd}")),
    };

    // Always run cleanup regardless of success/failure
    let _ = client.detach_from_target(&session_id).await;
    client.dialog_action = None;

    // Append target ID so the caller can pin subsequent commands to this page
    let name = friendly::to_friendly(&target_id);
    result.map(|mut r| {
        r.target_id = Some(name);
        r
    })
}

use anyhow::{anyhow, bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::commands;
use crate::friendly;
use crate::protocol::DaemonRequest;
use crate::result::CommandResult;

/// Known arguments for each command. Used to detect and report unknown arguments.
pub fn known_args(cmd: &str) -> &'static [&'static str] {
    match cmd {
        "list-pages" => &[],
        "new-page" => &["url", "viewport", "device_scale_factor", "mobile", "geolocation", "accuracy", "extra_headers"],
        "close-page" => &["id_or_index"],
        "select-page" => &["id_or_index"],
        "navigate" => &["url", "back", "forward", "reload", "extra_headers", "viewport", "device_scale_factor", "mobile", "geolocation", "accuracy", "clear_all", "output"],
        "screenshot" => &["output", "format", "full_page"],
        "evaluate" => &["expression", "dialog_action", "output", "track_navigation"],
        "click" => &["selector"],
        "click-at" => &["x", "y"],
        "fill" => &["selector", "value"],
        "type-text" => &["text", "submit_key"],
        "press-key" => &["key"],
        "hover" => &["selector"],
        "snapshot" => &["output"],
        "emulate" => &["viewport", "device_scale_factor", "mobile", "geolocation", "accuracy", "clear_viewport", "clear_geolocation", "clear_all"],
        "wait-for" => &["text", "timeout"],
        "list-3p-tools" => &[],
        "execute-3p-tool" => &["name", "params"],
        _ => &[],
    }
}

/// Check for unknown arguments and return an error message if any are found.
fn validate_args(cmd: &str, args: &serde_json::Value) -> Result<()> {
    let known = known_args(cmd);
    if let Some(obj) = args.as_object() {
        let unknown: Vec<&String> = obj.keys().filter(|k| {
            let key = k.as_str();
            // dialog_action is handled globally for all page-level commands
            if key == "dialog_action" && !is_browser_level(cmd) {
                return false;
            }
            !known.contains(&key)
        }).collect();
        if !unknown.is_empty() {
            let unknown_names: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
            bail!(
                "Unknown argument(s) for '{}': {}. Expected: {}",
                cmd,
                unknown_names.join(", "),
                known.join(", ")
            );
        }
    }
    Ok(())
}

/// Whether a command operates at the browser level (no page session needed).
fn is_browser_level(cmd: &str) -> bool {
    matches!(
        cmd,
        "list-pages" | "new-page"
    )
}

/// Execute a single command from a [`DaemonRequest`].
///
/// Handles target resolution, session attachment, dialog configuration,
/// and target ID enrichment on success.
pub async fn execute_command(client: &mut CdpClient, req: &DaemonRequest) -> Result<CommandResult> {
    // Clear stale events from previous commands to prevent memory leak
    // in long-running daemon mode and avoid stale events interfering
    // with new command execution.
    client.clear_events();

    let args = &req.args;
    let cmd = req.command.as_str();

    validate_args(cmd, args)?;

    if is_browser_level(cmd) {
        return match cmd {
            "list-pages" => commands::pages::list_pages(client, req.json_output).await,
            "new-page" => {
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or(anyhow!("url required"))?;

                let viewport = args.get("viewport").and_then(|v| v.as_str());
                let geolocation = args.get("geolocation").and_then(|v| v.as_str());

                let params = commands::emulation::EmulateParams {
                    viewport: viewport.map(|s| s.to_string()),
                    device_scale_factor: args.get("device_scale_factor").and_then(|v| v.as_f64()),
                    mobile: args.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false),
                    geolocation: geolocation.map(|s| s.to_string()),
                    accuracy: args.get("accuracy").and_then(|v| v.as_f64()),
                    clear_viewport: false,
                    clear_geolocation: false,
                    clear_all: false,
                };
                params.validate()?;

                let params = if params.has_emulation() {
                    Some(params)
                } else {
                    None
                };

                commands::pages::new_page(client, url, params, args.get("extra_headers").and_then(|v| v.as_str())).await
            }
            _ => unreachable!(),
        };
    }

    // Page-level: resolve and attach to target
    let (target_id_arg, page_idx_arg) = if cmd == "close-page" || cmd == "select-page" {
        match args.get("id_or_index") {
            Some(v) if v.is_string() => {
                let s = v.as_str().unwrap();
                if let Ok(idx) = s.parse::<usize>() {
                    (None, Some(idx))
                } else {
                    (Some(s), None)
                }
            }
            Some(v) if v.is_number() => {
                let idx = v.as_u64()
                    .ok_or_else(|| anyhow!("invalid numeric id_or_index: must be a non-negative integer"))?;
                let idx_usize = usize::try_from(idx)
                    .map_err(|_| anyhow!("index too large"))?;
                (None, Some(idx_usize))
            }
            _ => (req.target.as_deref(), req.page),
        }
    } else {
        (req.target.as_deref(), req.page)
    };

    let target = client.resolve_page(target_id_arg, page_idx_arg).await?;
    let target_id = target.target_id.clone();

    // Special case for commands that target a page but don't need a session
    if cmd == "close-page" || cmd == "select-page" {
        return match cmd {
            "close-page" => commands::pages::close_page(client, &target_id).await,
            "select-page" => commands::pages::select_page(client, &target_id).await,
            _ => unreachable!(),
        };
    }

    let session_id = client.attach_to_target(&target_id).await?;

    let result = inner_execute(client, &session_id, req).await;

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

/// Execute a page-level command within an active session.
async fn inner_execute(
    client: &mut CdpClient,
    session_id: &str,
    req: &DaemonRequest,
) -> Result<CommandResult> {
    let args = &req.args;
    let cmd = req.command.as_str();

    // Enable Page domain to receive dialog events for proactive rejection
    client
        .send_to_target(session_id, "Page.enable", json!({}))
        .await?;

    client.dialog_action = args
        .get("dialog_action")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    match cmd {
        "navigate" => {
            // Apply emulation before navigation if requested
            let viewport = args.get("viewport").and_then(|v| v.as_str());
            let geolocation = args.get("geolocation").and_then(|v| v.as_str());
            let clear_all = args.get("clear_all").and_then(|v| v.as_bool()).unwrap_or(false);

            let params = commands::emulation::EmulateParams {
                viewport: viewport.map(|s| s.to_string()),
                device_scale_factor: args.get("device_scale_factor").and_then(|v| v.as_f64()),
                mobile: args.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false),
                geolocation: geolocation.map(|s| s.to_string()),
                accuracy: args.get("accuracy").and_then(|v| v.as_f64()),
                clear_viewport: false,
                clear_geolocation: false,
                clear_all,
            };
            params.validate()?;

            if params.has_emulation() {
                commands::emulation::emulate(client, session_id, params).await?;
            }

            commands::navigate::navigate(
                client,
                session_id,
                args.get("url").and_then(|v| v.as_str()),
                args.get("back").and_then(|v| v.as_bool()).unwrap_or(false),
                args.get("forward")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                args.get("reload")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                args.get("extra_headers").and_then(|v| v.as_str()),
                args.get("output").and_then(|v| v.as_str()),
            )
            .await
        }
        "screenshot" => {
            commands::screenshot::take_screenshot(
                client,
                session_id,
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
                    session_id,
                    expr,
                    req.json_output,
                    args.get("output").and_then(|v| v.as_str()),
                    args.get("track_navigation")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                )
                .await
            }
            None => bail!("expression required"),
        },
        "click" => match args.get("selector").and_then(|v| v.as_str()) {
            Some(sel) => commands::input::click(client, session_id, sel).await,
            None => bail!("selector required"),
        },
        "click-at" => match (
            args.get("x").and_then(|v| v.as_f64()),
            args.get("y").and_then(|v| v.as_f64()),
        ) {
            (Some(x), Some(y)) => commands::input::click_at(client, session_id, x, y, None).await,
            _ => bail!("x and y required"),
        },
        "fill" => match (
            args.get("selector").and_then(|v| v.as_str()),
            args.get("value").and_then(|v| v.as_str()),
        ) {
            (Some(sel), Some(val)) => commands::input::fill(client, session_id, sel, val).await,
            _ => bail!("selector and value required"),
        },
        "type-text" => match args.get("text").and_then(|v| v.as_str()) {
            Some(text) => {
                commands::input::type_text(
                    client,
                    session_id,
                    text,
                    args.get("submit_key").and_then(|v| v.as_str()),
                )
                .await
            }
            None => bail!("text required"),
        },
        "press-key" => match args.get("key").and_then(|v| v.as_str()) {
            Some(key) => commands::input::press_key(client, session_id, key).await,
            None => bail!("key required"),
        },
        "hover" => match args.get("selector").and_then(|v| v.as_str()) {
            Some(sel) => commands::input::hover(client, session_id, sel).await,
            None => bail!("selector required"),
        },
        "snapshot" => {
            commands::snapshot::take_snapshot(
                client,
                session_id,
                req.json_output,
                args.get("output").and_then(|v| v.as_str()),
            )
            .await
        }
        "emulate" => {
            let params = commands::emulation::EmulateParams {
                viewport: args.get("viewport").and_then(|v| v.as_str()).map(|s| s.to_string()),
                device_scale_factor: args.get("device_scale_factor").and_then(|v| v.as_f64()),
                mobile: args.get("mobile").and_then(|v| v.as_bool()).unwrap_or(false),
                geolocation: args.get("geolocation").and_then(|v| v.as_str()).map(|s| s.to_string()),
                accuracy: args.get("accuracy").and_then(|v| v.as_f64()),
                clear_viewport: args.get("clear_viewport").and_then(|v| v.as_bool()).unwrap_or(false),
                clear_geolocation: args.get("clear_geolocation").and_then(|v| v.as_bool()).unwrap_or(false),
                clear_all: args.get("clear_all").and_then(|v| v.as_bool()).unwrap_or(false),
            };
            params.validate()?;
            commands::emulation::emulate(client, session_id, params).await
        }
        "wait-for" => match args.get("text").and_then(|v| v.as_str()) {
            Some(text) => {
                let timeout = args
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                commands::pages::wait_for(client, session_id, text, timeout).await
            }
            None => bail!("text required"),
        },
        "list-3p-tools" => {
            commands::third_party::list_3p_tools(client, session_id, req.json_output).await
        }
        "execute-3p-tool" => match args.get("name").and_then(|v| v.as_str()) {
            Some(name) => {
                commands::third_party::execute_3p_tool(
                    client,
                    session_id,
                    name,
                    args.get("params").and_then(|v| v.as_str()),
                    req.json_output,
                )
                .await
            }
            None => bail!("name required"),
        },
        _ => bail!("Unknown command: {cmd}"),
    }
}

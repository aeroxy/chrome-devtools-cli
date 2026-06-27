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
        "new-page" => &[
            "url",
            "viewport",
            "device_scale_factor",
            "mobile",
            "geolocation",
            "accuracy",
            "extra_headers",
        ],
        "close-page" => &["id_or_index"],
        "select-page" => &["id_or_index"],
        "navigate" => &[
            "url",
            "back",
            "forward",
            "reload",
            "extra_headers",
            "viewport",
            "device_scale_factor",
            "mobile",
            "geolocation",
            "accuracy",
            "clear_all",
            "output",
        ],
        "screenshot" => &[
            "output",
            "format",
            "full_page",
            "quality",
            "max_width",
            "max_height",
        ],
        "evaluate" => &["expression", "dialog_action", "output", "track_navigation"],
        "click" => &["selector"],
        "click-at" => &["x", "y"],
        "fill" => &["selector", "value"],
        "type-text" => &["text", "submit_key"],
        "press-key" => &["key"],
        "hover" => &["selector"],
        "snapshot" => &["output"],
        "read-page" => &["output"],
        "take-heapsnapshot" => &["output"],
        "inspect-heapsnapshot-node" => &["file_path", "node_id"],
        "emulate" => &[
            "viewport",
            "device_scale_factor",
            "mobile",
            "geolocation",
            "accuracy",
            "clear_viewport",
            "clear_geolocation",
            "clear_all",
            "clear_blocks",
        ],
        "wait-for" => &["text", "timeout"],
        "list-3p-tools" => &[],
        "execute-3p-tool" => &["name", "params"],
        "console" => &["duration", "type"],
        "network" => &["duration", "type"],
        "sw-logs" => &["duration", "extension_id"],
        "kill-daemon" => &[],
        _ => &[],
    }
}

/// Check for unknown arguments and return an error message if any are found.
fn validate_args(cmd: &str, args: &serde_json::Value) -> Result<()> {
    let known = known_args(cmd);
    if let Some(obj) = args.as_object() {
        let unknown: Vec<&String> = obj
            .keys()
            .filter(|k| {
                let key = k.as_str();
                // dialog_action is handled globally for all page-level commands
                if key == "dialog_action" && !is_browser_level(cmd) {
                    return false;
                }
                !known.contains(&key)
            })
            .collect();
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
///
/// `inspect-heapsnapshot-node` is intentionally excluded: it is intercepted
/// offline in the CLI before any daemon connection is established, so the
/// daemon should never receive it. If it ever does, omitting it here lets it
/// fall through to `inner_execute`'s catch-all `bail!("Unknown command")`
/// rather than hitting the `_ => unreachable!()` arm in the browser-level
/// dispatch and panicking.
fn is_browser_level(cmd: &str) -> bool {
    matches!(cmd, "list-pages" | "new-page" | "sw-logs" | "kill-daemon")
}

/// Execute a single command from a [`DaemonRequest`].
///
/// Handles target resolution, session attachment, dialog configuration,
/// and target ID enrichment on success.
pub async fn execute_command(client: &mut CdpClient, req: &DaemonRequest) -> Result<CommandResult> {
    let cmd = req.command.as_str();

    // Event-collecting commands drain the buffer via read_events_for,
    // so they can capture events that arrived between commands.
    // All other commands clear stale events to prevent memory buildup.
    if !matches!(cmd, "console" | "network" | "sw-logs") {
        client.clear_events();
    }

    let args = &req.args;

    validate_args(cmd, args)?;

    if is_browser_level(cmd) {
        return match cmd {
            "list-pages" => commands::pages::list_pages(client, req.format()).await,
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
                    mobile: args
                        .get("mobile")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    geolocation: geolocation.map(|s| s.to_string()),
                    accuracy: args.get("accuracy").and_then(|v| v.as_f64()),
                    clear_viewport: false,
                    clear_geolocation: false,
                    clear_all: false,
                    // Apply the global --block-url/--unblock-url flags to the new
                    // tab during its initial load, matching direct mode and the
                    // viewport/geolocation flags handled above.
                    block_url: req.block_url.clone(),
                    unblock_url: req.unblock_url.clone(),
                    clear_blocks: false,
                };
                params.validate()?;

                let params = if params.has_emulation() {
                    Some(params)
                } else {
                    None
                };

                commands::pages::new_page(
                    client,
                    url,
                    params,
                    args.get("extra_headers").and_then(|v| v.as_str()),
                )
                .await
            }
            "sw-logs" => {
                let duration = args
                    .get("duration")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3000);
                let extension_id = args.get("extension_id").and_then(|v| v.as_str());
                commands::sw_logs::collect_sw_logs(client, duration, extension_id, req.format())
                    .await
            }
            "kill-daemon" => Ok(CommandResult::output(
                "kill-daemon is handled directly by the CLI, not the daemon.",
            )),
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
                let idx = v.as_u64().ok_or_else(|| {
                    anyhow!("invalid numeric id_or_index: must be a non-negative integer")
                })?;
                let idx_usize = usize::try_from(idx).map_err(|_| anyhow!("index too large"))?;
                (None, Some(idx_usize))
            }
            _ => (req.target.as_deref(), req.page),
        }
    } else {
        (req.target.as_deref(), req.page)
    };

    let target = client.resolve_page(target_id_arg, page_idx_arg).await?;
    let target_id = target.target_id.clone();

    // Maintain a persistent CDP session for continuous event collection
    // (Network + Console) across commands. Skip for close/select which don't need it.
    if cmd != "close-page" && cmd != "select-page" {
        let _ = client.ensure_persistent_session(&target_id).await;
    }

    // Merge the global --block-url/--unblock-url flags (from DaemonRequest
    // top-level fields) into the blocklist and apply them. This MUST run after
    // ensure_persistent_session: that call swaps `blocklist` to the resolved
    // target's per-tab list, so merging here lands the flags on the intended
    // tab. Merging earlier would mutate (and leak into) whichever tab was
    // active before the switch, and the swap would then discard the change.
    //
    // Skip "emulate": its handler applies these fields itself, ordered with
    // --clear-blocks (clear first, then add). Skip close/select: no session.
    // Browser-level commands returned early above, so --block-url with them is
    // intentionally a no-op (a per-tab rule has no target tab there).
    if cmd != "emulate"
        && cmd != "navigate"
        && cmd != "close-page"
        && cmd != "select-page"
        && (!req.block_url.is_empty() || !req.unblock_url.is_empty())
    {
        for p in &req.block_url {
            if !client.blocklist.contains(p) {
                client.blocklist.push(p.clone());
            }
        }
        client.blocklist.retain(|b| !req.unblock_url.contains(b));
        client.apply_network_rules().await?;
    }

    // Special case for commands that target a page but don't need a session
    if cmd == "close-page" || cmd == "select-page" {
        return match cmd {
            "close-page" => commands::pages::close_page(client, &target_id).await,
            "select-page" => commands::pages::select_page(client, &target_id).await,
            _ => unreachable!(),
        };
    }

    // Prefer the persistent session so navigation, emulation, and event
    // collection all share one stable session per target. This is what makes
    // blocklist/emulation state actually govern the commands that run here
    // (e.g. `navigate` is subject to the blocklist; `emulate --clear-*` clears
    // overrides set by a previous `emulate`, since both run on the same session).
    // Fall back to a fresh per-command session only if the persistent session
    // isn't available for this target (e.g. its setup failed).
    let using_persistent = client.persistent_session.is_some()
        && client.persistent_target_id.as_deref() == Some(target_id.as_str());
    let session_id = if using_persistent {
        client.persistent_session.clone().unwrap()
    } else {
        // Degraded path: the persistent session is unavailable, so this fresh
        // session won't have the blocklist or emulation that
        // ensure_persistent_session normally applies. Re-apply them so
        // overrides still take effect.
        let sid = client.attach_to_target(&target_id).await?;
        if !client.blocklist.is_empty() {
            client.apply_network_rules_internal(&sid).await?;
        }
        client.apply_emulation_internal(&sid).await?;
        sid
    };

    let result = inner_execute(client, &session_id, req).await;

    // Clean up only sessions we created here — never detach the persistent one.
    if !using_persistent {
        let _ = client.detach_from_target(&session_id).await;
    }
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
            let clear_all = args
                .get("clear_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let params = commands::emulation::EmulateParams {
                viewport: viewport.map(|s| s.to_string()),
                device_scale_factor: args.get("device_scale_factor").and_then(|v| v.as_f64()),
                mobile: args
                    .get("mobile")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                geolocation: geolocation.map(|s| s.to_string()),
                accuracy: args.get("accuracy").and_then(|v| v.as_f64()),
                clear_viewport: false,
                clear_geolocation: false,
                clear_all,
                block_url: req.block_url.clone(),
                unblock_url: req.unblock_url.clone(),
                clear_blocks: false,
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
                commands::screenshot::ScreenshotOptions {
                    output: args.get("output").and_then(|v| v.as_str()).map(String::from),
                    format: args
                        .get("format")
                        .and_then(|v| v.as_str())
                        .unwrap_or("png")
                        .to_string(),
                    full_page: args
                        .get("full_page")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    quality: args.get("quality").and_then(|v| v.as_u64()),
                    max_width: args.get("max_width").and_then(|v| v.as_f64()),
                    max_height: args.get("max_height").and_then(|v| v.as_f64()),
                },
            )
            .await
        }
        "evaluate" => match args.get("expression").and_then(|v| v.as_str()) {
            Some(expr) => {
                commands::evaluate::evaluate(
                    client,
                    session_id,
                    expr,
                    req.format(),
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
                req.format(),
                args.get("output").and_then(|v| v.as_str()),
            )
            .await
        }
        "read-page" => {
            commands::read_page::read_page(
                client,
                session_id,
                req.format(),
                args.get("output").and_then(|v| v.as_str()),
            )
            .await
        }
        "take-heapsnapshot" => match args.get("output").and_then(|v| v.as_str()) {
            Some(output) => {
                commands::memory::take_heapsnapshot(client, session_id, output, req.format()).await
            }
            None => bail!("output required"),
        },
        "emulate" => {
            // block/unblock come from the global request fields (the single flag
            // definition); the emulate handler applies them itself — in the right
            // order relative to --clear-blocks — which is why the generic merge
            // above skips "emulate".
            let params = commands::emulation::EmulateParams {
                viewport: args
                    .get("viewport")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                device_scale_factor: args.get("device_scale_factor").and_then(|v| v.as_f64()),
                mobile: args
                    .get("mobile")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                geolocation: args
                    .get("geolocation")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                accuracy: args.get("accuracy").and_then(|v| v.as_f64()),
                clear_viewport: args
                    .get("clear_viewport")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                clear_geolocation: args
                    .get("clear_geolocation")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                clear_all: args
                    .get("clear_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                block_url: req.block_url.clone(),
                unblock_url: req.unblock_url.clone(),
                clear_blocks: args
                    .get("clear_blocks")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
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
            commands::third_party::list_3p_tools(client, session_id, req.format()).await
        }
        "execute-3p-tool" => match args.get("name").and_then(|v| v.as_str()) {
            Some(name) => {
                commands::third_party::execute_3p_tool(
                    client,
                    session_id,
                    name,
                    args.get("params").and_then(|v| v.as_str()),
                    req.format(),
                )
                .await
            }
            None => bail!("name required"),
        },
        "console" => {
            let duration = args.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
            let types: Vec<String> = args
                .get("type")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            commands::console::collect_console(client, session_id, duration, types, req.format())
                .await
        }
        "network" => {
            let duration = args.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
            let types: Vec<String> = args
                .get("type")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            commands::network::collect_network(client, session_id, duration, types, req.format())
                .await
        }
        _ => bail!("Unknown command: {cmd}"),
    }
}

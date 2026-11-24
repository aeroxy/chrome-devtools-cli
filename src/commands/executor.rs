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
            "background",
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
        "inspect-heapsnapshot-node" => {
            &["file_path", "node_id", "limit", "offset", "max_name_length"]
        }
        "compare-heapsnapshots" => &[
            "base",
            "current",
            "class_index",
            "limit",
            "offset",
            "max_name_length",
        ],
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
            "media",
            "media_feature",
            "clear_media",
        ],
        "css-styles" => &["selector", "computed", "ua_rules"],
        "mock-network" => &[
            "pattern",
            "status",
            "headers",
            "body",
            "body_file",
            "abort",
            "resource_type",
        ],
        "remove-network-mock" => &["pattern"],
        "configure-webauthn" => &[
            "protocol",
            "transport",
            "has_resident_key",
            "has_user_verification",
            "is_user_verified",
            "remove",
        ],
        "wait-for" => &["text", "timeout"],
        "list-3p-tools" => &[],
        "execute-3p-tool" => &["name", "params"],
        "console" => &["duration", "type"],
        "network" => &["duration", "type", "request_headers", "response_headers"],
        "cookies" => &["urls", "names", "header"],
        "sw-logs" => &["duration", "extension_id"],
        "run-script" => &[
            "file_path",
            "script_args",
            "raw_args",
            "output",
            "track_navigation",
        ],
        "adapter" => &[
            "file_path",
            "function_name",
            "script_args",
            "raw_args",
            "output",
            "track_navigation",
        ],
        "kill-daemon" => &["force", "all"],
        "list-daemons" => &[],
        "caffeinate" => &[],
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
    matches!(
        cmd,
        "list-pages" | "new-page" | "sw-logs" | "kill-daemon" | "list-daemons" | "caffeinate"
    )
}

/// Execute a single command from a [`DaemonRequest`].
///
/// Handles target resolution, session attachment, dialog configuration,
/// and target ID enrichment on success.
pub async fn execute_command(client: &mut CdpClient, req: &DaemonRequest) -> Result<CommandResult> {
    let cmd = req.command.as_str();

    // Event-collecting commands drain the buffer via read_events_for,
    // so they can capture events that arrived between commands, and a
    // caffeinate ping leaves it as the next command would find it.
    // All other commands clear stale events to prevent memory buildup.
    if !matches!(cmd, "console" | "network" | "sw-logs" | "caffeinate") {
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
                    media: None,
                    media_features: vec![],
                    clear_media: false,
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
                    args.get("background")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
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
            "list-daemons" => Ok(CommandResult::output(
                "list-daemons is handled directly by the CLI, not the daemon.",
            )),
            "kill-daemon" => Ok(CommandResult::output(
                "kill-daemon is handled directly by the CLI, not the daemon.",
            )),
            // One check per request: the CLI side loops, so the daemon serves
            // other commands in between.
            // Bounded like every other call, so a connection that died
            // without closing ends caffeinate instead of hanging the daemon.
            "caffeinate" => client
                .check_alive()
                .await
                .map(|()| CommandResult::output("")),
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

    // A dialog that opened while the daemon was idle is announced to whatever
    // reads next, often this lookup. It doesn't hold the lookup up, and it is
    // settled below, with this command's --dialog-action, only if it is on the
    // page this command resolves to.
    let target = client.resolve_page(target_id_arg, page_idx_arg).await?;
    let target_id = target.target_id.clone();

    // Maintain a persistent CDP session for continuous event collection
    // (Network + Console) across commands. Skip for close/select which don't need it.
    if cmd != "close-page" && cmd != "select-page" {
        if let Err(e) = client.ensure_persistent_session(&target_id).await {
            // A blocked page answers no session, so a second one would only
            // wait out the same timeout again; a lost connection must reach
            // the daemon, which exits on it.
            if crate::cdp::is_connection_or_page_failure(&e) {
                return Err(e);
            }
        }
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

    client.dialog_action = args
        .get("dialog_action")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let console_mark = client.console_pushed;
    client.input_sent = false;
    // A dialog left open by an earlier command blocks the page: handle it
    // first, or fail now rather than wait on a page that won't answer.
    let result = match client.settle_open_dialog(&session_id).await {
        Ok(()) => {
            let result = inner_execute(client, &session_id, &target_id, req).await;
            // Only the session that saw a dialog open can handle it, and a
            // one-off session is detached below.
            let next = if using_persistent {
                HANDLE_DIALOG_NEXT
            } else {
                ONE_OFF_DIALOG_NEXT
            };
            result.or_else(|e| dialog_opened_by_input(cmd, client.input_sent, e, next))
        }
        Err(e) => Err(e),
    };

    // Clean up only sessions we created here — never detach the persistent one.
    if !using_persistent {
        client.forget_dialog(Some(&session_id));
        let _ = client.detach_from_target(&session_id).await;
    }
    client.dialog_action = None;

    // Errors the page raised while this command ran, e.g. from a click
    // handler. `console` and `network` show messages themselves.
    let page_errors = if matches!(cmd, "console" | "network") {
        Vec::new()
    } else {
        client.page_errors_since(console_mark)
    };

    // Append target ID so the caller can pin subsequent commands to this page
    let name = friendly::to_friendly(&target_id);
    result.map(|mut r| {
        r.target_id = Some(name);
        r.notes.extend(page_errors);
        r
    })
}

/// What to do about a dialog an input opened, when the daemon ran it on its
/// own session for the tab: that session saw the dialog open, and only such a
/// session can handle it.
pub(crate) const HANDLE_DIALOG_NEXT: &str = "The page is paused until the dialog is handled: \
     run `evaluate --dialog-action accept|dismiss|<prompt text> true` on this page next.";

/// The same without the daemon: the session that saw the dialog open ends
/// with the command, and Chrome tells no later session about the dialog.
pub(crate) const DIRECT_DIALOG_NEXT: &str = "This ran without the daemon, so no later command \
     can handle the dialog: a person has to close it, or `close-page` closes the tab.";

/// The same when the daemon could not attach its own session to the tab: the
/// one-off session it ran the input on is detached when the command ends.
const ONE_OFF_DIALOG_NEXT: &str = "The daemon could not attach to this tab, so the command ran \
     on a session of its own that has ended, and no later command can handle the dialog: a \
     person has to close it, or `close-page` closes the tab.";

/// An input action that opens a dialog did what it was asked: the page reacted
/// with a dialog, and the action's reply can't come back until it closes.
/// Reporting that as a failure invites a retry, which would click twice. A
/// dialog that turned up before the action was sent is an ordinary failure.
/// `next` says what to do about the dialog, which depends on who ran it.
pub(crate) fn dialog_opened_by_input(
    cmd: &str,
    input_sent: bool,
    error: anyhow::Error,
    next: &str,
) -> Result<CommandResult> {
    if !input_sent {
        return Err(error);
    }
    let verb = match cmd {
        "click" | "click-at" => "Clicked",
        "hover" => "Hovered",
        "fill" => "Filled",
        "type-text" => "Typed",
        "press-key" => "Pressed the key",
        _ => return Err(error),
    };
    let Some(dialog) = error.downcast_ref::<crate::cdp::DialogOpen>() else {
        return Err(error);
    };
    let article = if dialog.dialog_type.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    Ok(CommandResult::output(format!(
        "{verb}, and the page opened {article} {} dialog: \"{}\". Steps after it opened did not run \
         (for type-text, a --submit-key). {next}",
        dialog.dialog_type, dialog.message
    )))
}

/// Arguments shared by the `run-script` and `adapter` commands.
struct ScriptExecArgs<'a> {
    file_path: &'a str,
    script_args: serde_json::Value,
    output: Option<&'a str>,
    track_navigation: bool,
}

/// Extract the arguments common to `run-script` and `adapter` from the raw
/// command args. `script_args` is conceptually optional; it defaults to an
/// empty object so clients can omit it when a script/adapter takes no
/// arguments.
fn script_exec_args(args: &serde_json::Value) -> Result<ScriptExecArgs<'_>> {
    let file_path = args
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("file_path required"))?;
    let script_args = args
        .get("script_args")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let output = args.get("output").and_then(|v| v.as_str());
    let track_navigation = args
        .get("track_navigation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Ok(ScriptExecArgs {
        file_path,
        script_args,
        output,
        track_navigation,
    })
}

/// A repeatable string flag's values (a JSON array), empty when absent.
fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Execute a page-level command within an active session on `target_id`.
async fn inner_execute(
    client: &mut CdpClient,
    session_id: &str,
    target_id: &str,
    req: &DaemonRequest,
) -> Result<CommandResult> {
    let args = &req.args;
    let cmd = req.command.as_str();

    // Enable Page domain to receive dialog events for proactive rejection.
    // `client.dialog_action` was set by `execute_command`.
    client
        .send_to_target_within(
            session_id,
            "Page.enable",
            json!({}),
            crate::cdp::SETUP_CALL_TIMEOUT,
        )
        .await?;

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
                media: None,
                media_features: vec![],
                clear_media: false,
            };
            params.validate()?;

            if params.has_emulation() {
                commands::emulation::emulate_tab(client, session_id, Some(target_id), params)
                    .await?;
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
                    output: args
                        .get("output")
                        .and_then(|v| v.as_str())
                        .map(String::from),
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
            (Some(sel), Some(val)) => {
                commands::input::fill(client, session_id, sel, val, req.cwd.as_deref()).await
            }
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
                media: args
                    .get("media")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                media_features: string_list(args.get("media_feature")),
                clear_media: args
                    .get("clear_media")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            };
            params.validate()?;
            commands::emulation::emulate_tab(client, session_id, Some(target_id), params).await
        }
        "mock-network" => match args.get("pattern").and_then(|v| v.as_str()) {
            None => commands::network_mock::list_network_mocks(client, req.format()),
            Some(pattern) => {
                let request = commands::network_mock::MockRequest {
                    pattern,
                    status: args
                        .get("status")
                        .and_then(|v| v.as_u64())
                        .map(|s| u16::try_from(s).unwrap_or(0)),
                    headers: string_list(args.get("headers")),
                    body: args.get("body").and_then(|v| v.as_str()),
                    body_file: args.get("body_file").and_then(|v| v.as_str()),
                    abort: args.get("abort").and_then(|v| v.as_bool()).unwrap_or(false),
                    resource_type: args.get("resource_type").and_then(|v| v.as_str()),
                };
                commands::network_mock::mock_network(client, session_id, request).await
            }
        },
        "remove-network-mock" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str());
            commands::network_mock::remove_network_mock(client, session_id, pattern).await
        }
        "css-styles" => match args.get("selector").and_then(|v| v.as_str()) {
            Some(selector) => {
                let opts = commands::css::CssOptions {
                    computed: string_list(args.get("computed")),
                    ua_rules: args
                        .get("ua_rules")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                };
                commands::css::css_styles(client, session_id, selector, opts, req.format()).await
            }
            None => bail!("selector required"),
        },
        "configure-webauthn" => {
            let text = |k: &str| args.get(k).and_then(|v| v.as_str()).map(String::from);
            let flag = |k: &str| args.get(k).and_then(|v| v.as_bool());
            let params = commands::webauthn::WebAuthnParams {
                protocol: text("protocol"),
                transport: text("transport"),
                has_resident_key: flag("has_resident_key"),
                has_user_verification: flag("has_user_verification"),
                is_user_verified: flag("is_user_verified"),
                remove: flag("remove").unwrap_or(false),
            };
            commands::webauthn::configure_webauthn(client, session_id, params, req.format()).await
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
        "cookies" => {
            commands::cookies::cookies(
                client,
                session_id,
                &string_list(args.get("urls")),
                &string_list(args.get("names")),
                args.get("header")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                req.format(),
            )
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
            let flag = |name: &str| args.get(name).and_then(|v| v.as_bool()).unwrap_or(false);
            let headers = commands::network::Headers {
                request: flag("request_headers"),
                response: flag("response_headers"),
            };
            commands::network::collect_network(
                client,
                session_id,
                duration,
                types,
                headers,
                req.format(),
            )
            .await
        }
        "run-script" => {
            let a = script_exec_args(args)?;
            commands::evaluate::run_script(
                client,
                session_id,
                a.file_path,
                &a.script_args,
                req.format(),
                a.output,
                a.track_navigation,
            )
            .await
        }
        "adapter" => {
            let a = script_exec_args(args)?;
            let function_name = args
                .get("function_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("function_name required"))?;

            commands::evaluate::run_adapter(
                client,
                session_id,
                a.file_path,
                function_name,
                &a.script_args,
                req.format(),
                a.output,
                a.track_navigation,
            )
            .await
        }
        _ => bail!("Unknown command: {cmd}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message;

    /// What the fake browser does with one call: send `events`, then the reply
    /// (its `result` or `error`), or no reply at all, as a page blocked by a
    /// dialog gives none.
    struct Answer {
        events: Vec<Value>,
        reply: Option<Value>,
    }

    fn reply(result: Value) -> Answer {
        Answer {
            events: Vec::new(),
            reply: Some(json!({"result": result})),
        }
    }

    /// The call refused, as Chrome refuses one with an ordinary CDP error.
    fn cdp_error(message: &str) -> Answer {
        Answer {
            events: Vec::new(),
            reply: Some(json!({"error": {"code": -32000, "message": message}})),
        }
    }

    fn value(v: &str) -> Answer {
        reply(json!({"result": {"type": "string", "value": v}}))
    }

    fn dialog_event(message: &str) -> Value {
        json!({
            "method": "Page.javascriptDialogOpening",
            "sessionId": "S1",
            "params": {"type": "alert", "message": message},
        })
    }

    /// The page opens a dialog while handling the call, so the call gets no reply.
    fn dialog(message: &str) -> Answer {
        Answer {
            events: vec![dialog_event(message)],
            reply: None,
        }
    }

    /// One tab, T1, which the daemon's persistent session S1 is attached to.
    fn usual(method: &str, params: &Value) -> Answer {
        match method {
            "Target.getTargets" => reply(json!({"targetInfos": [
                {"targetId": "T1", "type": "page", "title": "", "url": "https://a.test/"},
            ]})),
            "Target.attachToTarget" => reply(json!({"sessionId": "S2"})),
            "Runtime.evaluate" if params["expression"] == "window.location.href" => {
                value("https://a.test/")
            }
            _ => reply(json!({})),
        }
    }

    /// A client of a browser that answers each call with `answer(method,
    /// params)`. The task yields the calls it got, as `method@session`.
    async fn fake_browser(
        answer: impl Fn(&str, &Value) -> Answer + Send + 'static,
    ) -> (CdpClient, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let browser = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let method = call["method"].as_str().unwrap_or_default();
                let session = call["sessionId"].as_str().unwrap_or_default();
                calls.push(format!("{method}@{session}"));
                let answer = answer(method, &call["params"]);
                for event in answer.events {
                    ws.send(Message::Text(event.to_string())).await.unwrap();
                }
                if let Some(mut reply) = answer.reply {
                    reply["id"] = call["id"].clone();
                    reply["sessionId"] = json!(session);
                    ws.send(Message::Text(reply.to_string())).await.unwrap();
                }
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        client.persistent_session = Some("S1".into());
        client.persistent_target_id = Some("T1".into());
        (client, browser)
    }

    async fn calls_made(
        client: CdpClient,
        browser: tokio::task::JoinHandle<Vec<String>>,
    ) -> Vec<String> {
        drop(client);
        browser.await.unwrap()
    }

    fn request(command: &str, args: Value) -> DaemonRequest {
        serde_json::from_value(json!({"command": command, "args": args})).unwrap()
    }

    fn is_dialog(error: &anyhow::Error) -> bool {
        error.downcast_ref::<crate::cdp::DialogOpen>().is_some()
    }

    /// The error a command was expected to fail with.
    fn failure(result: Result<CommandResult>) -> anyhow::Error {
        match result {
            Ok(r) => panic!("expected a failure, got: {}", r.output),
            Err(e) => e,
        }
    }

    /// `fill`'s check of what the element is, which changes nothing.
    fn fill_check(expr: &str) -> bool {
        expr.contains("const kind") && !expr.contains("dispatchEvent")
    }

    /// `fill`'s evaluation that sets a select, checkbox or textarea.
    fn fill_set(expr: &str) -> bool {
        expr.contains("const kind") && expr.contains("dispatchEvent")
    }

    /// A mouseover handler's dialog stops the click before the button is
    /// pressed, so reporting a click would be false, and a retry is safe.
    #[tokio::test]
    async fn a_dialog_from_moving_the_pointer_fails_the_click() {
        let (mut client, browser) = fake_browser(|method, params| match method {
            "Input.dispatchMouseEvent" if params["type"] == "mouseMoved" => dialog("hovered"),
            _ => usual(method, params),
        })
        .await;
        let click = request("click-at", json!({"x": 5, "y": 5}));
        let error = failure(execute_command(&mut client, &click).await);
        assert!(is_dialog(&error), "{error:#}");
        assert!(
            format!("{error:#}").starts_with(
                "Moving the pointer there opened a dialog, so the click was not sent: A javascript \
                 dialog is open (alert: hovered)"
            ),
            "{error:#}"
        );
        let calls = calls_made(client, browser).await;
        let mouse_events = calls
            .iter()
            .filter(|c| c.starts_with("Input.dispatchMouseEvent"));
        assert_eq!(mouse_events.count(), 1, "{calls:?}");
    }

    /// The press starts the click, as it does a person's: a dialog from it, or
    /// from the click handler, is the click's outcome, and a retry would repeat it.
    #[tokio::test]
    async fn a_dialog_from_pressing_or_clicking_reports_the_click() {
        for step in ["mousePressed", "mouseReleased"] {
            let (mut client, browser) = fake_browser(move |method, params| match method {
                "Input.dispatchMouseEvent" if params["type"] == step => dialog("clicked"),
                _ => usual(method, params),
            })
            .await;
            let result =
                execute_command(&mut client, &request("click-at", json!({"x": 5, "y": 5})))
                    .await
                    .expect("the page got the click");
            assert!(
                result
                    .output
                    .starts_with("Clicked, and the page opened an alert dialog: \"clicked\"."),
                "{step}: {}",
                result.output
            );
            calls_made(client, browser).await;
        }
    }

    /// The page can disable an option between the check and the selection,
    /// so the evaluation that selects checks again.
    #[tokio::test]
    async fn an_option_refused_when_it_is_selected_fails_the_click() {
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            match method {
                "Runtime.evaluate" if expr.contains("return 'option'") => value("option"),
                "Runtime.evaluate" if expr.contains("return 'selected'") => {
                    value("select_disabled")
                }
                _ => usual(method, params),
            }
        })
        .await;
        let click = request("click", json!({"selector": "#b"}));
        let error = failure(execute_command(&mut client, &click).await);
        assert_eq!(
            error.to_string(),
            "Cannot click option in disabled select: #b"
        );
        calls_made(client, browser).await;
    }

    /// The check that comes first changes nothing, so a dialog that blocks it
    /// was not opened by the fill: the command fails before setting anything.
    #[tokio::test]
    async fn a_dialog_that_blocks_the_check_fails_the_fill() {
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            match method {
                "Runtime.evaluate" if fill_check(expr) => dialog("already open"),
                _ => usual(method, params),
            }
        })
        .await;
        let fill = request("fill", json!({"selector": "#q", "value": "hello"}));
        let error = failure(execute_command(&mut client, &fill).await);
        assert!(is_dialog(&error), "{error:#}");
        let calls = calls_made(client, browser).await;
        // The page's URL, then the check, and nothing after it.
        let evaluations = calls.iter().filter(|c| c.starts_with("Runtime.evaluate"));
        assert_eq!(evaluations.count(), 2, "{calls:?}");
    }

    /// The page can change the element between the check and the fill, so
    /// the evaluation that fills checks again.
    #[tokio::test]
    async fn an_option_gone_by_the_time_it_is_set_fails_the_fill() {
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            match method {
                "Runtime.evaluate" if fill_check(expr) => value("select"),
                "Runtime.evaluate" if fill_set(expr) => value("option_not_found"),
                _ => usual(method, params),
            }
        })
        .await;
        let fill = request("fill", json!({"selector": "#s", "value": "b"}));
        let error = failure(execute_command(&mut client, &fill).await);
        assert_eq!(
            error.to_string(),
            "Could not find option with text or value 'b' in select element: #s"
        );
        calls_made(client, browser).await;
    }

    /// Focusing a text field only gets it ready: a dialog from its focus
    /// handler means the value never went in, so `fill` must not report it.
    #[tokio::test]
    async fn a_dialog_from_focusing_the_field_fails_the_fill() {
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            match method {
                "Runtime.evaluate" if fill_check(expr) => value("text"),
                "Runtime.evaluate" if expr.contains("el.focus()") => dialog("focused"),
                _ => usual(method, params),
            }
        })
        .await;
        let fill = request("fill", json!({"selector": "#q", "value": "hello"}));
        let error = failure(execute_command(&mut client, &fill).await);
        assert!(is_dialog(&error), "{error:#}");
        assert!(
            format!("{error:#}").starts_with(
                "Focusing or clearing the field opened a dialog, so the value was not entered"
            ),
            "{error:#}"
        );
        let calls = calls_made(client, browser).await;
        assert!(
            !calls.iter().any(|c| c.starts_with("Input.insertText")),
            "{calls:?}"
        );
    }

    /// A textarea's value is set by the same evaluation that fires `change`,
    /// so a dialog from its handler comes after the fill.
    #[tokio::test]
    async fn a_dialog_after_the_value_is_set_reports_the_fill() {
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            match method {
                "Runtime.evaluate" if fill_check(expr) => value("textarea"),
                "Runtime.evaluate" if fill_set(expr) => dialog("changed"),
                _ => usual(method, params),
            }
        })
        .await;
        let result = execute_command(
            &mut client,
            &request("fill", json!({"selector": "textarea", "value": "hello"})),
        )
        .await
        .expect("the value is set");
        assert!(
            result
                .output
                .starts_with("Filled, and the page opened an alert dialog"),
            "{}",
            result.output
        );
        calls_made(client, browser).await;
    }

    /// Chrome sets the file before firing `change`, so a dialog from the
    /// handler means the file is set; an error would invite a second upload.
    #[tokio::test]
    async fn a_dialog_from_a_file_inputs_change_handler_reports_the_fill() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            match method {
                "Runtime.evaluate" if fill_check(expr) => value("file_input"),
                "Runtime.evaluate" if expr.starts_with("document.querySelector") => {
                    reply(json!({"result": {"type": "object", "objectId": "O1"}}))
                }
                "DOM.setFileInputFiles" => dialog("file changed"),
                _ => usual(method, params),
            }
        })
        .await;
        let args = json!({"selector": "#f", "value": file.to_str().unwrap()});
        let result = execute_command(&mut client, &request("fill", args))
            .await
            .expect("the file is set");
        assert!(
            result
                .output
                .starts_with("Filled, and the page opened an alert dialog: \"file changed\""),
            "{}",
            result.output
        );
        calls_made(client, browser).await;
    }

    /// A browser whose page T1 opened a dialog while nothing was reading: the
    /// event waits ahead of the next reply, the target lookup's.
    fn dialog_while_idle(targets: Value) -> impl Fn(&str, &Value) -> Answer + Send + 'static {
        let lookups = std::sync::atomic::AtomicUsize::new(0);
        move |method, params| match method {
            "Target.getTargets" => Answer {
                events: if lookups.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    vec![dialog_event("later")]
                } else {
                    Vec::new()
                },
                reply: Some(json!({"result": {"targetInfos": targets}})),
            },
            "Runtime.evaluate" if params["expression"] == "'done'" => value("done"),
            _ => usual(method, params),
        }
    }

    #[tokio::test]
    async fn the_first_recovery_request_handles_a_dialog_that_opened_while_idle() {
        let page = json!([{"targetId": "T1", "type": "page", "url": "https://a.test/"}]);
        let (mut client, browser) = fake_browser(dialog_while_idle(page.clone())).await;
        let recover = request(
            "evaluate",
            json!({"expression": "'done'", "dialog_action": "accept"}),
        );
        let result = execute_command(&mut client, &recover)
            .await
            .expect("--dialog-action handles the dialog");
        assert_eq!(result.output, "done");
        let calls = calls_made(client, browser).await;
        assert!(
            calls.contains(&"Page.handleJavaScriptDialog@S1".to_string()),
            "{calls:?}"
        );

        // Without an action, the dialog still fails a command on that page at once.
        let (mut client, browser) = fake_browser(dialog_while_idle(page)).await;
        let plain = request("evaluate", json!({"expression": "'done'"}));
        let error = failure(execute_command(&mut client, &plain).await);
        assert!(is_dialog(&error), "{error:#}");
        calls_made(client, browser).await;
    }

    /// The dialog is on T1 and the command is for T2: T2 isn't blocked, and
    /// the action meant for T2 must not answer T1's dialog.
    #[tokio::test]
    async fn a_dialog_on_another_page_neither_fails_the_command_nor_takes_its_action() {
        let pages = json!([
            {"targetId": "T1", "type": "page", "url": "https://a.test/"},
            {"targetId": "T2", "type": "page", "url": "https://b.test/"},
        ]);
        let (mut client, browser) = fake_browser(dialog_while_idle(pages)).await;
        let mut other = request(
            "evaluate",
            json!({"expression": "'done'", "dialog_action": "accept"}),
        );
        other.target = Some("T2".into());
        let result = execute_command(&mut client, &other)
            .await
            .expect("T2 answers");
        assert_eq!(result.output, "done");
        let calls = calls_made(client, browser).await;
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("Page.handleJavaScriptDialog")),
            "{calls:?}"
        );
    }

    /// The page turns the text field into a select once `fill` has looked at
    /// it: the step that types repeats the check before touching the field.
    #[tokio::test]
    async fn a_field_no_longer_text_is_not_typed_into() {
        let (mut client, browser) = fake_browser(|method, params| {
            let expr = params["expression"].as_str().unwrap_or_default();
            let rechecked = |e: &str| {
                let check = e.find("if (kind !== 'text') return kind;");
                check.is_some_and(|c| e.find("el.focus()").is_some_and(|f| c < f))
            };
            match method {
                "Runtime.evaluate" if fill_check(expr) => value("text"),
                "Runtime.evaluate" if rechecked(expr) => value("select"),
                "Runtime.evaluate" if expr.contains("el.focus()") => value("ready"),
                _ => usual(method, params),
            }
        })
        .await;
        let fill = request("fill", json!({"selector": "#q", "value": "hello"}));
        let error = failure(execute_command(&mut client, &fill).await);
        assert_eq!(
            error.to_string(),
            "The page changed the element before it could be filled: #q"
        );
        let calls = calls_made(client, browser).await;
        assert!(
            !calls.iter().any(|c| c.starts_with("Input.insertText")),
            "{calls:?}"
        );
    }

    /// A browser whose tab T1 refuses the daemon's own session (an ordinary
    /// CDP error while setting it up), so commands on T1 run on a one-off
    /// session, S2 again here, that is detached when they end.
    fn persistent_setup_refused(
        answer: impl Fn(&str, &Value) -> Option<Answer> + Send + 'static,
    ) -> impl Fn(&str, &Value) -> Answer + Send + 'static {
        move |method, params| match method {
            "Network.enable" => cdp_error("Not allowed"),
            _ => answer(method, params).unwrap_or_else(|| usual(method, params)),
        }
    }

    async fn without_the_daemons_session(
        answer: impl Fn(&str, &Value) -> Answer + Send + 'static,
    ) -> (CdpClient, tokio::task::JoinHandle<Vec<String>>) {
        let (mut client, browser) = fake_browser(answer).await;
        client.persistent_session = None;
        client.persistent_target_id = None;
        (client, browser)
    }

    /// Chrome tells only the session that saw a dialog open, and the one-off
    /// session is gone once the command ends, so the report must not send the
    /// caller to `--dialog-action`. It still reports the click.
    #[tokio::test]
    async fn a_dialog_from_an_input_on_a_one_off_session_is_left_to_a_person() {
        let (mut client, browser) =
            without_the_daemons_session(persistent_setup_refused(|method, params| {
                (method == "Input.dispatchMouseEvent" && params["type"] == "mousePressed")
                    .then(|| dialog("clicked"))
            }))
            .await;
        let click = request("click-at", json!({"x": 5, "y": 5}));
        let result = execute_command(&mut client, &click)
            .await
            .expect("the page got the click");
        assert!(
            result
                .output
                .starts_with("Clicked, and the page opened an alert dialog: \"clicked\"."),
            "{}",
            result.output
        );
        assert!(
            result.output.ends_with(
                "no later command can handle the dialog: a person has to close it, or \
                 `close-page` closes the tab."
            ),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("--dialog-action"),
            "{}",
            result.output
        );
        let calls = calls_made(client, browser).await;
        // The daemon's own session, refused, then the one-off one.
        assert!(
            calls.contains(&"Network.enable@S2".to_string()),
            "{calls:?}"
        );
        let attaches = calls.iter().filter(|c| *c == "Target.attachToTarget@");
        assert_eq!(attaches.count(), 2, "{calls:?}");
    }

    fn print_saved_for_t1(client: &mut CdpClient) {
        client.emulation_saved.insert(
            "T1".into(),
            crate::cdp::TabEmulation {
                media: Some(crate::cdp::MediaOverride {
                    media: Some("print".into()),
                    features: Vec::new(),
                }),
                ..Default::default()
            },
        );
    }

    /// T1's media override waits for the daemon's next session on the tab,
    /// which would put it back: clearing it on a one-off session clears that.
    #[tokio::test]
    async fn clearing_media_on_a_one_off_session_clears_what_the_tab_keeps() {
        let (mut client, browser) =
            without_the_daemons_session(persistent_setup_refused(|_, _| None)).await;
        print_saved_for_t1(&mut client);
        let clear = request("emulate", json!({"clear_media": true}));
        let result = execute_command(&mut client, &clear).await.expect("cleared");
        assert_eq!(result.output, "Media emulation cleared");
        let saved = client.emulation_saved.get("T1");
        assert!(saved.is_none_or(|s| s.media.is_none()), "{saved:?}");
        let calls = calls_made(client, browser).await;
        assert!(
            calls.contains(&"Emulation.setEmulatedMedia@S2".to_string()),
            "{calls:?}"
        );
    }

    /// Nothing applies T1's override on a one-off session, but the daemon
    /// still keeps it: the report must say so rather than "no overrides".
    #[tokio::test]
    async fn a_one_off_session_reports_the_tabs_saved_media_as_not_applied() {
        let (mut client, browser) =
            without_the_daemons_session(persistent_setup_refused(|_, _| None)).await;
        print_saved_for_t1(&mut client);
        let status = request("emulate", json!({}));
        let result = execute_command(&mut client, &status).await.expect("status");
        assert_eq!(
            result.output,
            "Media: print (saved for this tab, not applied: the daemon could not attach to it \
             this time)"
        );
        calls_made(client, browser).await;
    }

    /// Without the daemon, the session that saw the dialog open ends with the
    /// command, so the report can't send the caller to `--dialog-action`.
    #[test]
    fn the_report_on_a_dialog_says_who_can_handle_it() {
        let opened = || {
            anyhow::Error::new(crate::cdp::DialogOpen {
                dialog_type: "confirm".into(),
                message: "Delete?".into(),
            })
        };
        let daemon = dialog_opened_by_input("click", true, opened(), HANDLE_DIALOG_NEXT).unwrap();
        assert_eq!(
            daemon.output,
            "Clicked, and the page opened a confirm dialog: \"Delete?\". Steps after it opened did \
             not run (for type-text, a --submit-key). The page is paused until the dialog is \
             handled: run `evaluate --dialog-action accept|dismiss|<prompt text> true` on this \
             page next."
        );
        let direct = dialog_opened_by_input("fill", true, opened(), DIRECT_DIALOG_NEXT).unwrap();
        assert!(
            direct.output.starts_with("Filled, and the page opened"),
            "{}",
            direct.output
        );
        assert!(
            direct
                .output
                .ends_with("a person has to close it, or `close-page` closes the tab."),
            "{}",
            direct.output
        );
        assert!(
            !direct.output.contains("--dialog-action"),
            "{}",
            direct.output
        );

        // Before the action was sent, or for a command that isn't input, it
        // stays a failure.
        assert!(dialog_opened_by_input("click", false, opened(), DIRECT_DIALOG_NEXT).is_err());
        assert!(dialog_opened_by_input("evaluate", true, opened(), DIRECT_DIALOG_NEXT).is_err());
    }
}

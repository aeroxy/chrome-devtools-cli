pub mod browser;
pub mod cdp;
pub mod client;
pub mod commands;
pub mod constants;
pub mod daemon;
pub mod error;
pub mod format;
pub mod friendly;
pub mod protocol;
pub mod result;
pub mod telemetry;

use crate::protocol::DaemonRequest;
use anyhow::Result;
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "chrome-devtools",
    version,
    about = "Chrome DevTools Protocol CLI"
)]
pub struct Cli {
    /// Explicit WebSocket endpoint (skips auto-connect)
    #[arg(long, global = true, env = "CHROME_WS_ENDPOINT")]
    pub ws_endpoint: Option<String>,

    /// Chrome user data directory (for auto-connect)
    #[arg(long, global = true, env = "CHROME_USER_DATA_DIR")]
    pub user_data_dir: Option<String>,

    /// Chrome channel: stable, beta, canary, dev
    #[arg(long, global = true, default_value = "stable", env = "CHROME_CHANNEL")]
    pub channel: String,

    /// Page index for page-level commands (0-based, from list-pages)
    #[arg(long, short, global = true)]
    pub page: Option<usize>,

    /// Target ID for page-level commands (stable across calls, from command output)
    #[arg(long, short, global = true)]
    pub target: Option<String>,

    /// Output as JSON
    #[arg(long, global = true, conflicts_with = "toon")]
    pub json: bool,

    /// Output as TOON (Token-Oriented Object Notation — compact encoding for LLMs)
    #[arg(long, global = true, conflicts_with = "json")]
    pub toon: bool,

    /// Add URL pattern to the daemon's network block list (e.g. "*.png").
    /// Repeatable. Persisted in daemon memory until un-blocked or cleared.
    /// Blocks subresources (images, scripts, fetch/XHR, CDN, trackers); does
    /// NOT block top-level navigations (a Chrome setBlockedURLs limitation).
    #[arg(long, global = true)]
    pub block_url: Vec<String>,

    /// Un-block a previously blocked URL pattern. Repeatable.
    #[arg(long, global = true)]
    pub unblock_url: Vec<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all open pages/tabs
    ListPages,

    /// Navigate to a URL, or go back/forward/reload
    Navigate {
        /// URL to navigate to
        url: Option<String>,
        #[arg(long)]
        back: bool,
        #[arg(long)]
        forward: bool,
        #[arg(long)]
        reload: bool,
        /// Extra HTTP headers as a JSON object (e.g. '{"Authorization":"Bearer token"}')
        #[arg(long)]
        extra_headers: Option<String>,
        /// Set viewport size as WxH (e.g. 1280x720)
        #[arg(long)]
        viewport: Option<String>,
        /// Set device scale factor (default: 1.0)
        #[arg(long)]
        device_scale_factor: Option<f64>,
        /// Emulate mobile device (sets mobile=true in CDP)
        #[arg(long)]
        mobile: bool,
        /// Set geolocation as lat,lon (e.g. 37.77,-122.41)
        #[arg(long)]
        geolocation: Option<String>,
        /// Geolocation accuracy in meters (default: 100)
        #[arg(long)]
        accuracy: Option<f64>,
        /// Clear all emulation overrides
        #[arg(long)]
        clear_all: bool,
        /// Write output to a file instead of stdout
        #[arg(long, short)]
        output: Option<String>,
    },

    /// Open a new page/tab
    NewPage {
        /// URL to open
        url: String,
        /// Set viewport size as WxH (e.g. 1280x720)
        #[arg(long)]
        viewport: Option<String>,
        /// Set device scale factor (default: 1.0)
        #[arg(long)]
        device_scale_factor: Option<f64>,
        /// Emulate mobile device (sets mobile=true in CDP)
        #[arg(long)]
        mobile: bool,
        /// Set geolocation as lat,lon (e.g. 37.77,-122.41)
        #[arg(long)]
        geolocation: Option<String>,
        /// Geolocation accuracy in meters (default: 100)
        #[arg(long)]
        accuracy: Option<f64>,
        /// Extra HTTP headers as a JSON object (e.g. '{"Authorization":"Bearer token"}')
        #[arg(long)]
        extra_headers: Option<String>,
    },

    /// Close a page/tab
    ClosePage {
        /// Target ID, friendly name, or 0-based index
        id_or_index: Option<String>,
    },

    /// Bring a page to front
    SelectPage {
        /// Target ID, friendly name, or 0-based index
        id_or_index: Option<String>,
    },

    /// Take a screenshot
    Screenshot {
        /// Save to file path (default: print base64 to stdout)
        #[arg(long, short)]
        output: Option<String>,
        /// Image format: png, jpeg, webp
        #[arg(long, default_value = "png")]
        format: String,
        /// Capture full scrollable page
        #[arg(long)]
        full_page: bool,
    },

    /// Evaluate a JavaScript expression
    Evaluate {
        /// JavaScript expression
        expression: String,
        /// Handle dialogs while execution: accept, dismiss, or string for prompt
        #[arg(long)]
        dialog_action: Option<String>,
        /// Write output to a file instead of stdout
        #[arg(long, short)]
        output: Option<String>,
        /// Track URL changes caused by this evaluation (adds two extra CDP round-trips)
        #[arg(long)]
        track_navigation: bool,
    },

    /// Click an element by CSS selector
    Click { selector: String },

    /// Click at specific coordinates
    ClickAt { x: f64, y: f64 },

    /// Fill an input field by CSS selector
    Fill { selector: String, value: String },

    /// Type text using keyboard (into currently focused element)
    TypeText {
        text: String,
        /// Optional key to press after typing (e.g. Enter)
        #[arg(long)]
        submit_key: Option<String>,
    },

    /// Press a key or key combination (e.g. Enter, Control+A)
    PressKey { key: String },

    /// Hover over an element by CSS selector
    Hover { selector: String },

    /// Take an accessibility tree snapshot
    Snapshot {
        /// Write output to a file instead of stdout
        #[arg(long, short)]
        output: Option<String>,
    },

    /// Manage page emulation (viewport, geolocation, etc.)
    Emulate {
        /// Set viewport size as WxH (e.g. 1280x720)
        #[arg(long)]
        viewport: Option<String>,
        /// Set device scale factor (default: 1.0)
        #[arg(long)]
        device_scale_factor: Option<f64>,
        /// Emulate mobile device (sets mobile=true in CDP)
        #[arg(long)]
        mobile: bool,
        /// Set geolocation as lat,lon (e.g. 37.77,-122.41)
        #[arg(long)]
        geolocation: Option<String>,
        /// Geolocation accuracy in meters (default: 100)
        #[arg(long)]
        accuracy: Option<f64>,
        /// Clear viewport override
        #[arg(long)]
        clear_viewport: bool,
        /// Clear geolocation override
        #[arg(long)]
        clear_geolocation: bool,
        /// Clear all emulation overrides
        #[arg(long)]
        clear_all: bool,
        /// Clear network blocklist only
        #[arg(long)]
        clear_blocks: bool,
        // Note: --block-url / --unblock-url are global flags (see `Cli`); they
        // work on `emulate` via clap's global-arg inheritance.
    },

    /// Wait for text to appear on the page
    WaitFor {
        text: String,
        #[arg(long, default_value_t = 30000)]
        timeout: u64,
    },

    /// List third-party developer tools exposed by the page
    #[command(name = "list-3p-tools")]
    List3pTools,

    /// Execute a third-party developer tool exposed by the page
    #[command(name = "execute-3p-tool")]
    Execute3pTool {
        /// Name of the tool to execute
        name: String,
        /// JSON-stringified parameters for the tool
        params: Option<String>,
    },

    /// Collect console messages and exceptions from the page
    Console {
        /// Duration in ms for live collection (events remain available to a later drain).
        /// Omit or 0 to drain accumulated events instantly.
        #[arg(long, short, default_value_t = 0)]
        duration: u64,
        /// Filter by message type (e.g. error, warning, log, info). Repeatable.
        #[arg(long)]
        r#type: Vec<String>,
    },

    /// Collect network requests from the page
    Network {
        /// Duration in ms for live collection (events remain available to a later drain).
        /// Omit or 0 to drain accumulated events instantly.
        #[arg(long, short, default_value_t = 0)]
        duration: u64,
        /// Filter by resource type (e.g. document, xhr, fetch, script, stylesheet, image). Repeatable.
        #[arg(long)]
        r#type: Vec<String>,
    },

    /// Collect console logs from extension service workers
    #[command(name = "sw-logs")]
    SwLogs {
        /// Duration in milliseconds to collect logs (default: 3000)
        #[arg(long, short, default_value_t = 3000)]
        duration: u64,
        /// Filter by extension ID. If omitted, collects from all extensions.
        #[arg(long)]
        extension_id: Option<String>,
    },

    /// Stop the background daemon process
    #[command(name = "kill-daemon")]
    KillDaemon,
}

impl Cli {
    pub fn output_format(&self) -> format::OutputFormat {
        format::OutputFormat::from_flags(self.json, self.toon)
    }

    /// Whether this command operates at the browser level (no page session needed).
    fn is_browser_level(&self) -> bool {
        matches!(
            self.command,
            Commands::ListPages
                | Commands::NewPage { .. }
                | Commands::SwLogs { .. }
                | Commands::KillDaemon
        )
    }

    /// Get the name of the subcommand as a static string for telemetry logging.
    fn command_name(&self) -> &'static str {
        match &self.command {
            Commands::ListPages => "list-pages",
            Commands::Navigate { .. } => "navigate",
            Commands::NewPage { .. } => "new-page",
            Commands::ClosePage { .. } => "close-page",
            Commands::SelectPage { .. } => "select-page",
            Commands::Screenshot { .. } => "screenshot",
            Commands::Evaluate { .. } => "evaluate",
            Commands::Click { .. } => "click",
            Commands::ClickAt { .. } => "click-at",
            Commands::Fill { .. } => "fill",
            Commands::TypeText { .. } => "type-text",
            Commands::PressKey { .. } => "press-key",
            Commands::Hover { .. } => "hover",
            Commands::Snapshot { .. } => "snapshot",
            Commands::Emulate { .. } => "emulate",
            Commands::WaitFor { .. } => "wait-for",
            Commands::List3pTools => "list-3p-tools",
            Commands::Execute3pTool { .. } => "execute-3p-tool",
            Commands::Console { .. } => "console",
            Commands::Network { .. } => "network",
            Commands::SwLogs { .. } => "sw-logs",
            Commands::KillDaemon => "kill-daemon",
        }
    }
}

/// Build a DaemonRequest from parsed CLI args.
fn build_request(cli: &Cli) -> DaemonRequest {
    let (command, args) = match &cli.command {
        Commands::ListPages => ("list-pages", json!({})),
        Commands::Navigate {
            url,
            back,
            forward,
            reload,
            extra_headers,
            viewport,
            device_scale_factor,
            mobile,
            geolocation,
            accuracy,
            clear_all,
            output,
        } => (
            "navigate",
            json!({
                "url": url,
                "back": back,
                "forward": forward,
                "reload": reload,
                "extra_headers": extra_headers,
                "viewport": viewport,
                "device_scale_factor": device_scale_factor,
                "mobile": mobile,
                "geolocation": geolocation,
                "accuracy": accuracy,
                "clear_all": clear_all,
                "output": output
            }),
        ),
        Commands::NewPage {
            url,
            viewport,
            device_scale_factor,
            mobile,
            geolocation,
            accuracy,
            extra_headers,
        } => (
            "new-page",
            json!({
                "url": url,
                "viewport": viewport,
                "device_scale_factor": device_scale_factor,
                "mobile": mobile,
                "geolocation": geolocation,
                "accuracy": accuracy,
                "extra_headers": extra_headers
            }),
        ),
        Commands::ClosePage { id_or_index } => {
            ("close-page", json!({ "id_or_index": id_or_index }))
        }
        Commands::SelectPage { id_or_index } => {
            ("select-page", json!({ "id_or_index": id_or_index }))
        }
        Commands::Screenshot {
            output,
            format,
            full_page,
        } => (
            "screenshot",
            json!({"output": output, "format": format, "full_page": full_page}),
        ),
        Commands::Evaluate {
            expression,
            dialog_action,
            output,
            track_navigation,
        } => (
            "evaluate",
            json!({"expression": expression, "dialog_action": dialog_action, "output": output, "track_navigation": track_navigation}),
        ),
        Commands::Click { selector } => ("click", json!({"selector": selector})),
        Commands::ClickAt { x, y } => ("click-at", json!({"x": x, "y": y})),
        Commands::Fill { selector, value } => {
            ("fill", json!({"selector": selector, "value": value}))
        }
        Commands::TypeText { text, submit_key } => {
            ("type-text", json!({"text": text, "submit_key": submit_key}))
        }
        Commands::PressKey { key } => ("press-key", json!({"key": key})),
        Commands::Hover { selector } => ("hover", json!({"selector": selector})),
        Commands::Snapshot { output } => ("snapshot", json!({"output": output})),
        Commands::Emulate {
            viewport,
            device_scale_factor,
            mobile,
            geolocation,
            accuracy,
            clear_viewport,
            clear_geolocation,
            clear_all,
            clear_blocks,
        } => (
            "emulate",
            json!({"viewport": viewport, "device_scale_factor": device_scale_factor, "mobile": mobile, "geolocation": geolocation, "accuracy": accuracy, "clear_viewport": clear_viewport, "clear_geolocation": clear_geolocation, "clear_all": clear_all, "clear_blocks": clear_blocks}),
        ),
        Commands::WaitFor { text, timeout } => {
            ("wait-for", json!({"text": text, "timeout": timeout}))
        }
        Commands::List3pTools => ("list-3p-tools", json!({})),
        Commands::Execute3pTool { name, params } => {
            ("execute-3p-tool", json!({"name": name, "params": params}))
        }
        Commands::Console { duration, r#type } => {
            ("console", json!({"duration": duration, "type": r#type}))
        }
        Commands::Network { duration, r#type } => {
            ("network", json!({"duration": duration, "type": r#type}))
        }
        Commands::SwLogs {
            duration,
            extension_id,
        } => (
            "sw-logs",
            json!({"duration": duration, "extension_id": extension_id}),
        ),
        Commands::KillDaemon => ("kill-daemon", json!({})),
    };

    DaemonRequest {
        command: command.to_string(),
        args,
        page: cli.page,
        target: cli.target.clone(),
        json_output: cli.json,
        output_format: Some(cli.output_format()),
        block_url: cli.block_url.clone(),
        unblock_url: cli.unblock_url.clone(),
    }
}

fn print_output(output: &str, navigated_to: Option<&str>, target_id: Option<&str>) {
    if !output.is_empty() {
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
    }
    if let Some(navigated_to) = navigated_to {
        eprintln!("[navigated to: {navigated_to}]");
    }
    if let Some(target_id) = target_id {
        eprintln!("[target: {target_id}]");
    }
}

fn print_response(resp: &protocol::DaemonResponse) {
    if resp.success {
        print_output(&resp.output, resp.navigated_to.as_deref(), None);
    } else {
        eprintln!("error: {}", resp.error);
        telemetry::shutdown_logger();
        std::process::exit(resp.error_code.unwrap_or(1) as i32);
    }
}

fn print_result(result: &result::CommandResult) {
    print_output(
        &result.output,
        result.navigated_to.as_deref(),
        result.target_id.as_deref(),
    );
}

pub async fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                e.exit();
            }

            let err_str = e.render().to_string();
            let clean_err = err_str.replace("For more information, try '--help'.", "");
            eprintln!("{}", clean_err.trim_end());
            println!();

            // Show subcommand-specific help when the error is about a subcommand's args
            let mut cmd = Cli::command();
            let sub_name = std::env::args_os().skip(1).find_map(|arg| {
                let s = arg.to_string_lossy();
                if !s.starts_with('-') && cmd.get_subcommands().any(|c| c.get_name() == s) {
                    Some(s.into_owned())
                } else {
                    None
                }
            });
            match sub_name {
                Some(name) => {
                    if let Some(sub_cmd) = cmd.find_subcommand_mut(&name) {
                        let _ = sub_cmd.print_help();
                    } else {
                        let _ = cmd.print_help();
                    }
                }
                None => {
                    let _ = cmd.print_help();
                }
            }
            std::process::exit(1);
        }
    };

    // Handle kill-daemon without connecting to Chrome
    if matches!(cli.command, Commands::KillDaemon) {
        let pid_path = protocol::pid_path();
        let sock_path = protocol::socket_path();
        match std::fs::read_to_string(&pid_path) {
            Ok(pid_str) => {
                let pid: u32 = pid_str
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid PID in {}", pid_path.display()))?;
                #[cfg(unix)]
                {
                    // Refuse PID 0: kill(0, ...) signals every process in the
                    // caller's process group — it would take down this CLI and
                    // its siblings. A corrupted/truncated PID file could read "0".
                    if pid == 0 {
                        anyhow::bail!("PID in {} is 0; refusing to signal", pid_path.display());
                    }
                    // Guard against a PID that doesn't fit in libc::pid_t (a
                    // signed 32-bit integer on POSIX). The OS never produces such
                    // PIDs, but a corrupted PID file could, and the cast below
                    // would silently wrap to a negative number — which kill()
                    // interprets as "signal all processes in process group -pid",
                    // potentially killing unrelated processes.
                    let pid_i32: i32 = i32::try_from(pid).map_err(|_| {
                        anyhow::anyhow!(
                            "PID {} in {} exceeds libc::pid_t; refusing to signal",
                            pid,
                            pid_path.display()
                        )
                    })?;
                    // Signal the process directly via libc to avoid shelling out
                    // to /usr/bin/kill. A return of 0 means the signal was
                    // delivered; -1 with errno ESRCH means the process is gone
                    // (and the PID file was stale).
                    let ret = unsafe { libc::kill(pid_i32 as libc::pid_t, libc::SIGTERM) };
                    if ret == 0 {
                        // Signal delivered — daemon is shutting down; clean up.
                        let _ = std::fs::remove_file(&sock_path);
                        let _ = std::fs::remove_file(&pid_path);
                        println!("Daemon (PID {pid}) stopped.");
                    } else {
                        let err = std::io::Error::last_os_error();
                        if err.raw_os_error() == Some(libc::ESRCH) {
                            // Process is gone — the PID file was stale; clean up.
                            let _ = std::fs::remove_file(&sock_path);
                            let _ = std::fs::remove_file(&pid_path);
                            println!("Daemon (PID {pid}) was not running. Cleaned up stale files.");
                        } else {
                            // e.g. EPERM: the daemon may still be running. Leave the
                            // socket/PID files so it stays reachable.
                            return Err(anyhow::anyhow!(
                                "Failed to signal daemon (PID {pid}): {err}. Left socket/PID files in place."
                            ));
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = pid;
                    println!("kill-daemon is only supported on Unix systems.");
                }
            }
            Err(_) => {
                println!("No daemon running (PID file not found).");
            }
        }
        return Ok(());
    }

    let ws_url = browser::resolve_ws_url(
        cli.ws_endpoint.as_deref(),
        cli.user_data_dir.as_deref(),
        &cli.channel,
    )?;

    let request = build_request(&cli);

    // Try daemon first
    if let Ok(resp) = client::send_to_daemon(&request).await {
        print_response(&resp);
        return Ok(());
    }

    // Daemon not running — spawn it
    client::spawn_daemon(&ws_url)?;
    if let Err(e) = client::wait_for_daemon().await {
        return run_direct_fallback(&cli, &ws_url, &e).await;
    }

    // Retry via daemon
    match client::send_to_daemon(&request).await {
        Ok(resp) => {
            print_response(&resp);
            Ok(())
        }
        Err(e) => {
            // Daemon failed — fall back to direct execution
            run_direct_fallback(&cli, &ws_url, &e).await
        }
    }
}

/// Fall back to direct execution when the daemon is unavailable.
async fn run_direct_fallback(cli: &Cli, ws_url: &str, error: &anyhow::Error) -> Result<()> {
    eprintln!("Warning: daemon unavailable ({error}), running directly");
    let cmd_name = cli.command_name();
    let start = std::time::Instant::now();
    let result = run_direct(cli, ws_url).await;
    let duration = start.elapsed();
    match &result {
        Ok(r) => {
            telemetry::log_command(cmd_name, duration, true, r.error_code);
            print_result(r);
        }
        Err(e) => {
            let code = e
                .downcast_ref::<error::CliError>()
                .map(|ce| ce.code().code());
            telemetry::log_command(cmd_name, duration, false, code);
        }
    }
    result.map(|_| ())
}

/// Direct execution without daemon (fallback).
async fn run_direct(cli: &Cli, ws_url: &str) -> Result<result::CommandResult> {
    let mut client = cdp::CdpClient::connect(ws_url).await?;

    let is_browser = cli.is_browser_level();

    if is_browser {
        return match &cli.command {
            Commands::ListPages => {
                commands::pages::list_pages(&mut client, cli.output_format()).await
            }
            Commands::NewPage {
                url,
                viewport,
                device_scale_factor,
                mobile,
                geolocation,
                accuracy,
                extra_headers,
            } => {
                let params = commands::emulation::EmulateParams {
                    viewport: viewport.clone(),
                    device_scale_factor: *device_scale_factor,
                    mobile: *mobile,
                    geolocation: geolocation.clone(),
                    accuracy: *accuracy,
                    clear_viewport: false,
                    clear_geolocation: false,
                    clear_all: false,
                    // URL blocking is daemon-only state; new-page in direct mode
                    // has no persistent session to apply it to.
                    block_url: Vec::new(),
                    unblock_url: Vec::new(),
                    clear_blocks: false,
                };
                params.validate()?;

                let params = if params.has_emulation() {
                    Some(params)
                } else {
                    None
                };
                commands::pages::new_page(&mut client, url, params, extra_headers.as_deref()).await
            }
            Commands::SwLogs {
                duration,
                extension_id,
            } => {
                commands::sw_logs::collect_sw_logs(
                    &mut client,
                    *duration,
                    extension_id.as_deref(),
                    cli.output_format(),
                )
                .await
            }
            _ => unreachable!(),
        };
    }

    let (target_id_arg, page_idx_arg) = match &cli.command {
        Commands::ClosePage { id_or_index } | Commands::SelectPage { id_or_index } => {
            if let Some(s) = id_or_index {
                if let Ok(idx) = s.parse::<usize>() {
                    (None, Some(idx))
                } else {
                    (Some(s.as_str()), None)
                }
            } else {
                (cli.target.as_deref(), cli.page)
            }
        }
        _ => (cli.target.as_deref(), cli.page),
    };

    let target = client.resolve_page(target_id_arg, page_idx_arg).await?;
    let target_id = target.target_id.clone();

    // Special case for browser-level commands that target a specific page but don't need a session
    if matches!(
        cli.command,
        Commands::ClosePage { .. } | Commands::SelectPage { .. }
    ) {
        return match &cli.command {
            Commands::ClosePage { .. } => {
                commands::pages::close_page(&mut client, &target_id).await
            }
            Commands::SelectPage { .. } => {
                commands::pages::select_page(&mut client, &target_id).await
            }
            _ => unreachable!(),
        };
    }

    let session_id = client.attach_to_target(&target_id).await?;

    // Enable Page domain to receive dialog events for proactive rejection
    client
        .send_to_target(&session_id, "Page.enable", json!({}))
        .await?;

    // Extract dialog_action if set (only available on Evaluate command, but
    // apply it for all commands in direct mode to match daemon behavior)
    let dialog_action = match &cli.command {
        Commands::Evaluate { dialog_action, .. } => dialog_action.clone(),
        _ => None,
    };
    client.dialog_action = dialog_action;

    let result = match &cli.command {
        Commands::Navigate {
            url,
            back,
            forward,
            reload,
            extra_headers,
            viewport,
            device_scale_factor,
            mobile,
            geolocation,
            accuracy,
            clear_all,
            output,
        } => {
            let params = commands::emulation::EmulateParams {
                viewport: viewport.clone(),
                device_scale_factor: *device_scale_factor,
                mobile: *mobile,
                geolocation: geolocation.clone(),
                accuracy: *accuracy,
                clear_viewport: false,
                clear_geolocation: false,
                clear_all: *clear_all,
                // URL blocking is daemon-only state; navigate in direct mode has
                // no persistent session to apply it to.
                block_url: Vec::new(),
                unblock_url: Vec::new(),
                clear_blocks: false,
            };
            params.validate()?;

            // Apply emulation before navigation if requested
            if params.has_emulation() {
                commands::emulation::emulate(&mut client, &session_id, params).await?;
            }

            commands::navigate::navigate(
                &mut client,
                &session_id,
                url.as_deref(),
                *back,
                *forward,
                *reload,
                extra_headers.as_deref(),
                output.as_deref(),
            )
            .await
        }
        Commands::Screenshot {
            output,
            format,
            full_page,
        } => {
            commands::screenshot::take_screenshot(
                &mut client,
                &session_id,
                output.as_deref(),
                format,
                *full_page,
            )
            .await
        }
        Commands::Evaluate {
            expression,
            dialog_action: _,
            output,
            track_navigation,
        } => {
            commands::evaluate::evaluate(
                &mut client,
                &session_id,
                expression,
                cli.output_format(),
                output.as_deref(),
                *track_navigation,
            )
            .await
        }
        Commands::Click { selector } => {
            commands::input::click(&mut client, &session_id, selector).await
        }
        Commands::ClickAt { x, y } => {
            commands::input::click_at(&mut client, &session_id, *x, *y, None).await
        }
        Commands::Fill { selector, value } => {
            commands::input::fill(&mut client, &session_id, selector, value).await
        }
        Commands::TypeText { text, submit_key } => {
            commands::input::type_text(&mut client, &session_id, text, submit_key.as_deref()).await
        }
        Commands::PressKey { key } => {
            commands::input::press_key(&mut client, &session_id, key).await
        }
        Commands::Hover { selector } => {
            commands::input::hover(&mut client, &session_id, selector).await
        }
        Commands::Snapshot { output } => {
            commands::snapshot::take_snapshot(
                &mut client,
                &session_id,
                cli.output_format(),
                output.as_deref(),
            )
            .await
        }
        Commands::Emulate {
            viewport,
            device_scale_factor,
            mobile,
            geolocation,
            accuracy,
            clear_viewport,
            clear_geolocation,
            clear_all,
            clear_blocks,
        } => {
            let params = commands::emulation::EmulateParams {
                viewport: viewport.clone(),
                device_scale_factor: *device_scale_factor,
                mobile: *mobile,
                geolocation: geolocation.clone(),
                accuracy: *accuracy,
                clear_viewport: *clear_viewport,
                clear_geolocation: *clear_geolocation,
                clear_all: *clear_all,
                // block/unblock come from the global flags, not the subcommand.
                block_url: cli.block_url.clone(),
                unblock_url: cli.unblock_url.clone(),
                clear_blocks: *clear_blocks,
            };
            params.validate()?;
            commands::emulation::emulate(&mut client, &session_id, params).await
        }
        Commands::WaitFor { text, timeout } => {
            commands::pages::wait_for(&mut client, &session_id, text, *timeout).await
        }
        Commands::List3pTools => {
            commands::third_party::list_3p_tools(&mut client, &session_id, cli.output_format())
                .await
        }
        Commands::Execute3pTool { name, params } => {
            commands::third_party::execute_3p_tool(
                &mut client,
                &session_id,
                name,
                params.as_deref(),
                cli.output_format(),
            )
            .await
        }
        Commands::Console { duration, r#type } => {
            commands::console::collect_console(
                &mut client,
                &session_id,
                *duration,
                r#type.clone(),
                cli.output_format(),
            )
            .await
        }
        Commands::Network { duration, r#type } => {
            commands::network::collect_network(
                &mut client,
                &session_id,
                *duration,
                r#type.clone(),
                cli.output_format(),
            )
            .await
        }
        _ => unreachable!(),
    };

    let _ = client.detach_from_target(&session_id).await;
    let name = friendly::to_friendly(&target_id);
    result.map(|mut r| {
        r.target_id = Some(name);
        r
    })
}

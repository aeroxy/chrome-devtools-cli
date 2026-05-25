mod browser;
mod cdp;
mod client;
mod commands;
mod constants;
mod daemon;
mod error;
mod friendly;
mod protocol;
mod result;
mod telemetry;

use crate::error::ErrorCode;
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
struct Cli {
    /// Explicit WebSocket endpoint (skips auto-connect)
    #[arg(long, global = true, env = "CHROME_WS_ENDPOINT")]
    ws_endpoint: Option<String>,

    /// Chrome user data directory (for auto-connect)
    #[arg(long, global = true, env = "CHROME_USER_DATA_DIR")]
    user_data_dir: Option<String>,

    /// Chrome channel: stable, beta, canary, dev
    #[arg(long, global = true, default_value = "stable", env = "CHROME_CHANNEL")]
    channel: String,

    /// Page index for page-level commands (0-based, from list-pages)
    #[arg(long, short, global = true)]
    page: Option<usize>,

    /// Target ID for page-level commands (stable across calls, from command output)
    #[arg(long, short, global = true)]
    target: Option<String>,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
        #[arg(long, short = 't')]
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
    },

    /// Wait for text to appear on the page
    WaitFor {
        text: String,
        #[arg(long, default_value_t = 30000)]
        timeout: u64,
    },

    /// List third-party developer tools exposed by the page
    List3pTools,

    /// Execute a third-party developer tool exposed by the page
    Execute3pTool {
        /// Name of the tool to execute
        name: String,
        /// JSON-stringified parameters for the tool
        params: Option<String>,
    },
}

impl Cli {
    /// Whether this command operates at the browser level (no page session needed).
    fn is_browser_level(&self) -> bool {
        matches!(
            self.command,
            Commands::ListPages | Commands::NewPage { .. }
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
        }
    }
}

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

    let result = run().await;
    telemetry::shutdown_logger();
    if let Err(e) = result {
        let code = match e.downcast_ref::<error::CliError>() {
            Some(ce) => ce.code().code(),
            None => ErrorCode::Unspecified as u32,
        };
        eprintln!("error: {e:#}");
        std::process::exit(code as i32);
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
        } => (
            "new-page",
            json!({
                "url": url,
                "viewport": viewport,
                "device_scale_factor": device_scale_factor,
                "mobile": mobile,
                "geolocation": geolocation,
                "accuracy": accuracy
            }),
        ),
        Commands::ClosePage { id_or_index } => (
            "close-page",
            json!({ "id_or_index": id_or_index }),
        ),
        Commands::SelectPage { id_or_index } => (
            "select-page",
            json!({ "id_or_index": id_or_index }),
        ),
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
        } => (
            "emulate",
            json!({"viewport": viewport, "device_scale_factor": device_scale_factor, "mobile": mobile, "geolocation": geolocation, "accuracy": accuracy, "clear_viewport": clear_viewport, "clear_geolocation": clear_geolocation, "clear_all": clear_all}),
        ),
        Commands::WaitFor { text, timeout } => {
            ("wait-for", json!({"text": text, "timeout": timeout}))
        }
        Commands::List3pTools => ("list-3p-tools", json!({})),
        Commands::Execute3pTool { name, params } => {
            ("execute-3p-tool", json!({"name": name, "params": params}))
        }
    };

    DaemonRequest {
        command: command.to_string(),
        args,
        page: cli.page,
        target: cli.target.clone(),
        json_output: cli.json,
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

async fn run() -> Result<()> {
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
            let sub_name = std::env::args().skip(1).find(|name| {
                !name.starts_with('-') && cmd.get_subcommands().any(|c| c.get_name() == name)
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
            Commands::ListPages => commands::pages::list_pages(&mut client, cli.json).await,
            Commands::NewPage {
                url,
                viewport,
                device_scale_factor,
                mobile,
                geolocation,
                accuracy,
            } => {
                if accuracy.is_some() && geolocation.is_none() {
                    anyhow::bail!("--accuracy requires --geolocation");
                }

                let params = if viewport.is_some() || geolocation.is_some() {
                    Some(commands::emulation::EmulateParams {
                        viewport: viewport.clone(),
                        device_scale_factor: *device_scale_factor,
                        mobile: *mobile,
                        geolocation: geolocation.clone(),
                        accuracy: *accuracy,
                        clear_viewport: false,
                        clear_geolocation: false,
                        clear_all: false,
                    })
                } else {
                    None
                };
                commands::pages::new_page(&mut client, url, params).await
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
    if matches!(cli.command, Commands::ClosePage { .. } | Commands::SelectPage { .. }) {
        return match &cli.command {
            Commands::ClosePage { .. } => commands::pages::close_page(&mut client, &target_id).await,
            Commands::SelectPage { .. } => commands::pages::select_page(&mut client, &target_id).await,
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
            if accuracy.is_some() && geolocation.is_none() {
                anyhow::bail!("--accuracy requires --geolocation");
            }

            // Apply emulation before navigation if requested
            if viewport.is_some() || geolocation.is_some() || *clear_all {
                commands::emulation::emulate(
                    &mut client,
                    &session_id,
                    commands::emulation::EmulateParams {
                        viewport: viewport.clone(),
                        device_scale_factor: *device_scale_factor,
                        mobile: *mobile,
                        geolocation: geolocation.clone(),
                        accuracy: *accuracy,
                        clear_viewport: false,
                        clear_geolocation: false,
                        clear_all: *clear_all,
                    },
                )
                .await?;
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
                cli.json,
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
            commands::snapshot::take_snapshot(&mut client, &session_id, cli.json, output.as_deref())
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
        } => {
            commands::emulation::emulate(
                &mut client,
                &session_id,
                commands::emulation::EmulateParams {
                    viewport: viewport.clone(),
                    device_scale_factor: *device_scale_factor,
                    mobile: *mobile,
                    geolocation: geolocation.clone(),
                    accuracy: *accuracy,
                    clear_viewport: *clear_viewport,
                    clear_geolocation: *clear_geolocation,
                    clear_all: *clear_all,
                },
            )
            .await
        }
        Commands::WaitFor { text, timeout } => {
            commands::pages::wait_for(&mut client, &session_id, text, *timeout).await
        }
        Commands::List3pTools => {
            commands::third_party::list_3p_tools(&mut client, &session_id, cli.json).await
        }
        Commands::Execute3pTool { name, params } => {
            commands::third_party::execute_3p_tool(
                &mut client,
                &session_id,
                name,
                params.as_deref(),
                cli.json,
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

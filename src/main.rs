mod browser;
mod cdp;
mod client;
mod commands;
mod daemon;
mod friendly;
mod protocol;

use anyhow::Result;
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;

use protocol::DaemonRequest;

#[derive(Parser)]
#[command(name = "chrome-devtools", about = "Chrome DevTools Protocol CLI")]
struct Cli {
    /// Explicit WebSocket endpoint (skips auto-connect)
    #[arg(long, global = true)]
    ws_endpoint: Option<String>,

    /// Chrome user data directory (for auto-connect)
    #[arg(long, global = true)]
    user_data_dir: Option<String>,

    /// Chrome channel: stable, beta, canary, dev
    #[arg(long, global = true, default_value = "stable")]
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
    },

    /// Open a new page/tab
    NewPage {
        /// URL to open
        url: String,
    },

    /// Close a page/tab by index
    ClosePage {
        /// Page index (from list-pages)
        index: usize,
    },

    /// Bring a page to front
    SelectPage {
        /// Page index (from list-pages)
        index: usize,
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
    Snapshot,

    /// Resize the page viewport
    Resize { width: u32, height: u32 },

    /// Wait for text to appear on the page
    WaitFor {
        text: String,
        #[arg(long, default_value_t = 30000)]
        timeout: u64,
    },
}

#[tokio::main]
async fn main() {
    // Internal daemon mode — invoked by spawn_daemon(), not by users
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("__daemon__") {
        let ws_url = args.get(2).expect("daemon requires ws_url argument");
        if let Err(e) = daemon::run_daemon(ws_url).await {
            eprintln!("daemon error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
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
        } => (
            "navigate",
            json!({"url": url, "back": back, "forward": forward, "reload": reload}),
        ),
        Commands::NewPage { url } => ("new-page", json!({"url": url})),
        Commands::ClosePage { index } => ("close-page", json!({"index": index})),
        Commands::SelectPage { index } => ("select-page", json!({"index": index})),
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
        } => (
            "evaluate",
            json!({"expression": expression, "dialog_action": dialog_action}),
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
        Commands::Snapshot => ("snapshot", json!({})),
        Commands::Resize { width, height } => ("resize", json!({"width": width, "height": height})),
        Commands::WaitFor { text, timeout } => {
            ("wait-for", json!({"text": text, "timeout": timeout}))
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

fn print_response(resp: &protocol::DaemonResponse) {
    if resp.success {
        if !resp.output.is_empty() {
            print!("{}", resp.output);
            // Ensure trailing newline
            if !resp.output.ends_with('\n') {
                println!();
            }
        }
    } else {
        eprintln!("error: {}", resp.error);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let err_str = e.render().to_string();
            let clean_err = err_str.replace("For more information, try '--help'.", "");
            eprintln!("{}", clean_err.trim_end());
            if e.kind() != ErrorKind::DisplayHelp && e.kind() != ErrorKind::DisplayVersion {
                println!();
                let mut cmd = Cli::command();
                let _ = cmd.print_help();
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
    client::wait_for_daemon().await?;

    // Retry via daemon
    match client::send_to_daemon(&request).await {
        Ok(resp) => {
            print_response(&resp);
            Ok(())
        }
        Err(e) => {
            // Daemon failed — fall back to direct execution
            eprintln!("Warning: daemon unavailable ({e}), running directly");
            let output = run_direct(&cli, &ws_url).await?;
            if !output.is_empty() {
                print!("{}", output);
                if !output.ends_with('\n') {
                    println!();
                }
            }
            Ok(())
        }
    }
}

/// Direct execution without daemon (fallback).
async fn run_direct(cli: &Cli, ws_url: &str) -> Result<String> {
    let mut client = cdp::CdpClient::connect(ws_url).await?;

    let is_browser = matches!(
        cli.command,
        Commands::ListPages
            | Commands::NewPage { .. }
            | Commands::ClosePage { .. }
            | Commands::SelectPage { .. }
    );

    if is_browser {
        return match &cli.command {
            Commands::ListPages => commands::pages::list_pages(&mut client, cli.json).await,
            Commands::NewPage { url } => commands::pages::new_page(&mut client, url).await,
            Commands::ClosePage { index } => commands::pages::close_page(&mut client, *index).await,
            Commands::SelectPage { index } => {
                commands::pages::select_page(&mut client, *index).await
            }
            _ => unreachable!(),
        };
    }

    let target = client.resolve_page(cli.target.as_deref(), cli.page).await?;
    let target_id = target.target_id.clone();
    let session_id = client.attach_to_target(&target_id).await?;

    let result = match &cli.command {
        Commands::Navigate {
            url,
            back,
            forward,
            reload,
        } => {
            commands::navigate::navigate(
                &mut client,
                &session_id,
                url.as_deref(),
                *back,
                *forward,
                *reload,
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
            dialog_action,
        } => {
            commands::evaluate::evaluate(
                &mut client,
                &session_id,
                expression,
                cli.json,
                dialog_action.as_deref(),
            )
            .await
        }
        Commands::Click { selector } => {
            commands::input::click(&mut client, &session_id, selector).await
        }
        Commands::ClickAt { x, y } => {
            commands::input::click_at(&mut client, &session_id, *x, *y).await
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
        Commands::Snapshot => {
            commands::snapshot::take_snapshot(&mut client, &session_id, cli.json).await
        }
        Commands::Resize { width, height } => {
            commands::pages::resize(&mut client, &session_id, *width, *height).await
        }
        Commands::WaitFor { text, timeout } => {
            commands::pages::wait_for(&mut client, &session_id, text, *timeout).await
        }
        _ => unreachable!(),
    };

    let _ = client.detach_from_target(&session_id).await;
    let name = friendly::to_friendly(&target_id);
    result.map(|output| format!("{output}\n[target:{name}]"))
}

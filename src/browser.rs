use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

/// Resolve the WebSocket URL for connecting to Chrome or Edge.
///
/// Priority:
/// 1. Explicit --ws-endpoint
/// 2. Auto-connect via DevToolsActivePort (default)
pub fn resolve_ws_url(
    ws_endpoint: Option<&str>,
    user_data_dir: Option<&str>,
    channel: &str,
) -> Result<String> {
    if let Some(ws) = ws_endpoint {
        return Ok(ws.to_string());
    }

    // Auto-connect: read DevToolsActivePort from Chrome's user data directory
    let data_dir = match user_data_dir {
        Some(dir) => PathBuf::from(dir),
        None => default_chrome_user_data_dir(channel)?,
    };

    read_devtools_active_port(&data_dir)
}

/// Read DevToolsActivePort file and construct the WebSocket URL.
fn read_devtools_active_port(user_data_dir: &Path) -> Result<String> {
    let port_path = user_data_dir.join("DevToolsActivePort");

    let content = std::fs::read_to_string(&port_path).map_err(|_| {
        anyhow!(
            "Could not read DevToolsActivePort at {}\n\n\
             Make sure Chrome or Edge is running with remote debugging enabled:\n\
             1. Open Chrome/Edge\n\
             2. Go to chrome://inspect/#remote-debugging (or edge://inspect/#remote-debugging)\n\
             3. Enable the remote debugging server",
            port_path.display()
        )
    })?;

    let lines: Vec<&str> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.len() < 2 {
        bail!(
            "Invalid DevToolsActivePort content: expected port and path, got: {:?}",
            content.trim()
        );
    }

    let port: u16 = lines[0]
        .parse()
        .map_err(|_| anyhow!("Invalid port '{}' in DevToolsActivePort", lines[0]))?;

    if port == 0 {
        bail!("Port 0 in DevToolsActivePort — browser may not be running");
    }

    let path = lines[1];
    Ok(format!("ws://127.0.0.1:{port}{path}"))
}

/// Get the default browser user data directory for the given channel.
fn default_chrome_user_data_dir(channel: &str) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
        let dir = match channel {
            "stable" | "chrome" => home.join("Library/Application Support/Google/Chrome"),
            "beta" => home.join("Library/Application Support/Google/Chrome Beta"),
            "canary" => home.join("Library/Application Support/Google/Chrome Canary"),
            "dev" => home.join("Library/Application Support/Google/Chrome Dev"),
            "edge" => home.join("Library/Application Support/Microsoft Edge"),
            "edge-beta" => home.join("Library/Application Support/Microsoft Edge Beta"),
            "edge-canary" => home.join("Library/Application Support/Microsoft Edge Canary"),
            "edge-dev" => home.join("Library/Application Support/Microsoft Edge Dev"),
            _ => bail!("Unknown browser channel: {channel}. Valid: stable, beta, canary, dev, edge, edge-beta, edge-canary, edge-dev"),
        };
        Ok(dir)
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
        let dir = match channel {
            "stable" | "chrome" => home.join(".config/google-chrome"),
            "beta" => home.join(".config/google-chrome-beta"),
            "canary" => home.join(".config/google-chrome-unstable"),
            "dev" => home.join(".config/google-chrome-unstable"),
            "edge" => home.join(".config/microsoft-edge"),
            "edge-beta" => home.join(".config/microsoft-edge-beta"),
            "edge-canary" => home.join(".config/microsoft-edge-canary"),
            "edge-dev" => home.join(".config/microsoft-edge-dev"),
            _ => bail!("Unknown browser channel: {channel}. Valid: stable, beta, canary, dev, edge, edge-beta, edge-canary, edge-dev"),
        };
        Ok(dir)
    }

    #[cfg(target_os = "windows")]
    {
        let local_app_data =
            std::env::var("LOCALAPPDATA").map_err(|_| anyhow!("LOCALAPPDATA not set"))?;
        let base = PathBuf::from(&local_app_data);
        let dir = match channel {
            "stable" | "chrome" => base.join("Google/Chrome/User Data"),
            "beta" => base.join("Google/Chrome Beta/User Data"),
            "canary" => base.join("Google/Chrome SxS/User Data"),
            "dev" => base.join("Google/Chrome Dev/User Data"),
            "edge" => base.join("Microsoft/Edge/User Data"),
            "edge-beta" => base.join("Microsoft/Edge Beta/User Data"),
            "edge-canary" => base.join("Microsoft/Edge SxS/User Data"),
            "edge-dev" => base.join("Microsoft/Edge Dev/User Data"),
            _ => bail!("Unknown browser channel: {channel}. Valid: stable, beta, canary, dev, edge, edge-beta, edge-canary, edge-dev"),
        };
        Ok(dir)
    }
}

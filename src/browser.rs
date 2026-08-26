use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

/// Resolve the WebSocket URL for connecting to the browser.
///
/// Priority:
/// 1. Explicit --ws-endpoint
/// 2. Auto-connect via DevToolsActivePort (default)
pub fn resolve_ws_url(
    ws_endpoint: Option<&str>,
    user_data_dir: Option<&str>,
    browser: &str,
    channel: &str,
) -> Result<String> {
    if let Some(ws) = ws_endpoint {
        return Ok(ws.to_string());
    }

    let browser = Browser::parse(browser)?;

    // Auto-connect: read DevToolsActivePort from the browser's user data directory
    let data_dir = match user_data_dir {
        Some(dir) => PathBuf::from(dir),
        None => browser.default_user_data_dir(channel)?,
    };

    read_devtools_active_port(&data_dir, browser)
}

/// Read DevToolsActivePort file and construct the WebSocket URL.
fn read_devtools_active_port(user_data_dir: &Path, browser: Browser) -> Result<String> {
    let port_path = user_data_dir.join("DevToolsActivePort");
    let label = browser.label();

    let content = std::fs::read_to_string(&port_path).map_err(|_| {
        anyhow!(
            "Could not read DevToolsActivePort at {}\n\n\
             Make sure {label} is running with remote debugging enabled:\n\
             1. Open {label}\n\
             2. Go to {}://inspect/#remote-debugging\n\
             3. Enable the remote debugging server",
            port_path.display(),
            browser.scheme()
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
        bail!("Port 0 in DevToolsActivePort — {label} may not be running");
    }

    let path = lines[1];
    Ok(format!("ws://127.0.0.1:{port}{path}"))
}

/// A Chromium-based browser the CLI knows how to auto-connect to.
///
/// Both speak the same DevTools Protocol; they differ only in where the
/// profile (and therefore `DevToolsActivePort`) lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Browser {
    Chrome,
    Edge,
}

impl Browser {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "chrome" => Ok(Self::Chrome),
            "edge" | "msedge" => Ok(Self::Edge),
            _ => bail!("Unknown browser: {name} (expected 'chrome' or 'edge')"),
        }
    }

    /// Human-readable name, for error messages.
    fn label(self) -> &'static str {
        match self {
            Self::Chrome => "Chrome",
            Self::Edge => "Microsoft Edge",
        }
    }

    /// URL scheme for the `<scheme>://inspect` hint.
    fn scheme(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
        }
    }

    /// Default user data directory for the given release channel.
    fn default_user_data_dir(self, channel: &str) -> Result<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            let home =
                dirs::home_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
            let base = home.join("Library/Application Support");
            let dir = match (self, channel) {
                (Self::Chrome, "stable" | "chrome") => base.join("Google/Chrome"),
                (Self::Chrome, "beta") => base.join("Google/Chrome Beta"),
                (Self::Chrome, "canary") => base.join("Google/Chrome Canary"),
                (Self::Chrome, "dev") => base.join("Google/Chrome Dev"),
                (Self::Edge, "stable" | "edge") => base.join("Microsoft Edge"),
                (Self::Edge, "beta") => base.join("Microsoft Edge Beta"),
                (Self::Edge, "canary") => base.join("Microsoft Edge Canary"),
                (Self::Edge, "dev") => base.join("Microsoft Edge Dev"),
                _ => bail!("Unknown {} channel: {channel}", self.label()),
            };
            Ok(dir)
        }

        #[cfg(target_os = "linux")]
        {
            let home =
                dirs::home_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
            let dir = match (self, channel) {
                (Self::Chrome, "stable" | "chrome") => home.join(".config/google-chrome"),
                (Self::Chrome, "beta") => home.join(".config/google-chrome-beta"),
                // Chrome ships no Canary for Linux; unstable is the dev channel.
                (Self::Chrome, "canary" | "dev") => home.join(".config/google-chrome-unstable"),
                (Self::Edge, "stable" | "edge") => home.join(".config/microsoft-edge"),
                (Self::Edge, "beta") => home.join(".config/microsoft-edge-beta"),
                (Self::Edge, "dev") => home.join(".config/microsoft-edge-dev"),
                (Self::Edge, "canary") => {
                    bail!("Microsoft Edge Canary is not distributed for Linux")
                }
                _ => bail!("Unknown {} channel: {channel}", self.label()),
            };
            Ok(dir)
        }

        #[cfg(target_os = "windows")]
        {
            let local_app_data =
                std::env::var("LOCALAPPDATA").map_err(|_| anyhow!("LOCALAPPDATA not set"))?;
            let base = PathBuf::from(local_app_data);
            let dir = match (self, channel) {
                (Self::Chrome, "stable" | "chrome") => base.join("Google/Chrome/User Data"),
                (Self::Chrome, "beta") => base.join("Google/Chrome Beta/User Data"),
                (Self::Chrome, "canary") => base.join("Google/Chrome SxS/User Data"),
                (Self::Chrome, "dev") => base.join("Google/Chrome Dev/User Data"),
                (Self::Edge, "stable" | "edge") => base.join("Microsoft/Edge/User Data"),
                (Self::Edge, "beta") => base.join("Microsoft/Edge Beta/User Data"),
                (Self::Edge, "canary") => base.join("Microsoft/Edge SxS/User Data"),
                (Self::Edge, "dev") => base.join("Microsoft/Edge Dev/User Data"),
                _ => bail!("Unknown {} channel: {channel}", self.label()),
            };
            Ok(dir)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_browsers() {
        assert_eq!(Browser::parse("chrome").unwrap(), Browser::Chrome);
        assert_eq!(Browser::parse("edge").unwrap(), Browser::Edge);
        assert_eq!(Browser::parse("msedge").unwrap(), Browser::Edge);
    }

    #[test]
    fn rejects_unknown_browser() {
        let err = Browser::parse("firefox").unwrap_err().to_string();
        assert!(err.contains("Unknown browser: firefox"), "{err}");
    }

    #[test]
    fn rejects_cross_browser_channel_alias() {
        // "chrome" is a stable alias for Chrome only, "edge" for Edge only.
        assert!(Browser::Edge.default_user_data_dir("chrome").is_err());
        assert!(Browser::Chrome.default_user_data_dir("edge").is_err());
    }

    #[test]
    fn rejects_unknown_channel() {
        let err = Browser::Edge
            .default_user_data_dir("nightly")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Microsoft Edge channel: nightly"), "{err}");
    }

    #[test]
    fn stable_and_self_named_channels_agree() {
        for browser in [Browser::Chrome, Browser::Edge] {
            let stable = browser.default_user_data_dir("stable").unwrap();
            let alias = browser.default_user_data_dir(browser.scheme()).unwrap();
            assert_eq!(stable, alias);
        }
    }

    #[test]
    fn browsers_resolve_to_distinct_dirs() {
        let chrome = Browser::Chrome.default_user_data_dir("stable").unwrap();
        let edge = Browser::Edge.default_user_data_dir("stable").unwrap();
        assert_ne!(chrome, edge);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_edge_paths() {
        let home = dirs::home_dir().unwrap();
        let base = home.join("Library/Application Support");
        for (channel, expected) in [
            ("stable", "Microsoft Edge"),
            ("beta", "Microsoft Edge Beta"),
            ("dev", "Microsoft Edge Dev"),
            ("canary", "Microsoft Edge Canary"),
        ] {
            assert_eq!(
                Browser::Edge.default_user_data_dir(channel).unwrap(),
                base.join(expected),
                "channel {channel}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_chrome_paths_unchanged() {
        let home = dirs::home_dir().unwrap();
        let base = home.join("Library/Application Support/Google");
        for (channel, expected) in [
            ("stable", "Chrome"),
            ("beta", "Chrome Beta"),
            ("dev", "Chrome Dev"),
            ("canary", "Chrome Canary"),
        ] {
            assert_eq!(
                Browser::Chrome.default_user_data_dir(channel).unwrap(),
                base.join(expected),
                "channel {channel}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_edge_paths() {
        let home = dirs::home_dir().unwrap();
        for (channel, expected) in [
            ("stable", ".config/microsoft-edge"),
            ("beta", ".config/microsoft-edge-beta"),
            ("dev", ".config/microsoft-edge-dev"),
        ] {
            assert_eq!(
                Browser::Edge.default_user_data_dir(channel).unwrap(),
                home.join(expected),
                "channel {channel}"
            );
        }
        assert!(Browser::Edge.default_user_data_dir("canary").is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_edge_paths() {
        let base = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap());
        for (channel, expected) in [
            ("stable", "Microsoft/Edge/User Data"),
            ("beta", "Microsoft/Edge Beta/User Data"),
            ("dev", "Microsoft/Edge Dev/User Data"),
            ("canary", "Microsoft/Edge SxS/User Data"),
        ] {
            assert_eq!(
                Browser::Edge.default_user_data_dir(channel).unwrap(),
                base.join(expected),
                "channel {channel}"
            );
        }
    }

    #[test]
    fn ws_endpoint_short_circuits_browser_validation() {
        // An explicit endpoint needs no profile, so the browser is irrelevant.
        let ws = resolve_ws_url(Some("ws://127.0.0.1:9222/x"), None, "firefox", "stable").unwrap();
        assert_eq!(ws, "ws://127.0.0.1:9222/x");
    }

    #[test]
    fn active_port_error_names_the_browser() {
        let dir = std::env::temp_dir().join("chrome-devtools-cli-nonexistent-profile");
        let err = resolve_ws_url(None, Some(dir.to_str().unwrap()), "edge", "stable")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Microsoft Edge is running"), "{err}");
        assert!(err.contains("edge://inspect"), "{err}");
    }
}

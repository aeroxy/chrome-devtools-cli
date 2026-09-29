use anyhow::{anyhow, bail, Result};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Port tried when the default profile's `DevToolsActivePort` exists but cannot be read. 9222 is the conventional DevTools port and what chrome://inspect/#remote-debugging shows by default; `--port` covers anything else.
const FALLBACK_PORT: u16 = 9222;

/// Cap on each step of the `/json/version` probe. Loopback answers in well under a millisecond, so this only bounds a listener that accepts and then says nothing.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Most a `/json/version` reply may take up, headers included. Chrome's is under 1 KiB, so this only stops a listener that is not Chrome from making the CLI buffer without end.
const MAX_PROBE_REPLY: u64 = 64 * 1024;

/// Resolve the WebSocket URL for connecting to the browser.
///
/// Priority:
/// 1. Explicit `--ws-endpoint`
/// 2. Explicit `--port`, whichever kind of local server it names (see [`PortServer`])
/// 3. Auto-connect via `DevToolsActivePort` (default), falling back to [`FALLBACK_PORT`] when the default profile's copy cannot be read
///
/// An explicit endpoint wins because it names the browser directly, so it must
/// not be second-guessed by local profile discovery: it is how you reach a
/// browser this machine cannot find on disk — another host, a container, a
/// port-forwarded device, or an instance whose profile lives somewhere the
/// channel tables don't describe. It also short-circuits `--browser` and
/// `--channel` entirely, since those exist only to locate a profile directory.
/// `DevToolsActivePort` is the automatic fallback for the ordinary case where
/// the browser is local and its profile is where the vendor puts it.
///
/// `--port` short-circuits discovery the same way, since it names the server too. It exists for the case auto-connect cannot handle on its own: a profile directory the OS will not let this process read, which hides `DevToolsActivePort` while the browser's server is up. macOS does this until the user approves a privacy prompt letting the app running the CLI access another app's data, and security software can do it for good.
pub fn resolve_ws_url(
    ws_endpoint: Option<&str>,
    port: Option<u16>,
    user_data_dir: Option<&str>,
    browser: &str,
    channel: &str,
) -> Result<String> {
    if let Some(ws) = ws_endpoint {
        return Ok(ws.to_string());
    }
    if let Some(port) = port {
        // Only for the hint: --port skips profile discovery, so an unknown --browser is no error here.
        let scheme = Browser::parse(browser).map_or("chrome", Browser::scheme);
        return port_endpoint(port).map_err(|e| {
            anyhow!(
                "Cannot connect by --port {port}: {e}. Pass the port shown at \
                 {scheme}://inspect/#remote-debugging, or the one the browser was launched with."
            )
        });
    }

    let browser = Browser::parse(browser)?;

    // Auto-connect: read DevToolsActivePort from the browser's user data directory
    let data_dir = match user_data_dir {
        Some(dir) => PathBuf::from(dir),
        None => browser.default_user_data_dir(channel)?,
    };

    // Only the default profile may fall back to a guessed port. An explicit --user-data-dir usually names a throwaway instance on a port of its own, where 9222 would reach the everyday browser instead.
    let fallback_port = user_data_dir.is_none().then_some(FALLBACK_PORT);
    read_devtools_active_port(&data_dir, browser, fallback_port)
}

/// The two kinds of DevTools server a local port can have. They need different URLs, and `/json/version` tells them apart.
#[derive(Debug, PartialEq)]
enum PortServer {
    /// Started by `--remote-debugging-port`: advertises its browser endpoint there, and requires the UUID in it.
    Launched(String),
    /// Started from chrome://inspect/#remote-debugging: answers `/json/version` with 404, and needs no UUID.
    Inspect,
}

/// Browser endpoint of the DevTools server on `port`, whichever kind it is.
fn port_endpoint(port: u16) -> Result<String> {
    // Whatever holds the port writes the reply, so the URL it advertises must not send the CLI to another host, port or scheme. Chrome echoes the Host header the probe sends, so a real one always starts this way.
    let local = format!("ws://127.0.0.1:{port}/");
    Ok(match probe_port(port)? {
        PortServer::Launched(url) if url.starts_with(&local) => url,
        PortServer::Launched(url) => {
            bail!("the server there advertised {url:?}, which is not on 127.0.0.1:{port}")
        }
        PortServer::Inspect => inspect_ws_url(port),
    })
}

/// Ask the server on `port` for `/json/version` to learn which kind it is.
///
/// A raw request rather than an HTTP client crate, because those tend to honour `HTTP(S)_PROXY` and would send this loopback request to a proxy.
fn probe_port(port: u16) -> Result<PortServer> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)
        .map_err(|e| anyhow!("nothing is listening on 127.0.0.1:{port} ({e})"))?;
    let mut exchange = || -> Result<PortServer> {
        stream.set_read_timeout(Some(PROBE_TIMEOUT))?;
        write!(
            stream,
            "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        )?;
        read_version_response(&stream)
    };
    exchange().map_err(|e| anyhow!("127.0.0.1:{port} did not answer like a DevTools server ({e})"))
}

/// Classify a `/json/version` answer. Chrome keeps the connection open even when asked to close it, so the body is read up to `Content-Length` rather than to EOF.
fn read_version_response(reply: impl Read) -> Result<PortServer> {
    // Every read below goes through this cap, so a reply that would exceed it ends early instead of growing without bound.
    let mut reader = BufReader::new(reply.take(MAX_PROBE_REPLY));
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    match status_line.split_whitespace().nth(1) {
        Some("404") => return Ok(PortServer::Inspect),
        Some("200") => {}
        _ => bail!("unexpected reply {:?}", status_line.trim()),
    }
    let mut content_length = 0;
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 {
            bail!("reply cut short, or longer than {MAX_PROBE_REPLY} bytes");
        }
        if header.trim().is_empty() {
            break;
        }
        // Chrome writes `Content-Length:428`, with no space after the colon.
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse()?;
            }
        }
    }
    if content_length > MAX_PROBE_REPLY {
        bail!("reply of {content_length} bytes is longer than {MAX_PROBE_REPLY}");
    }
    let mut body = Vec::new();
    reader.take(content_length).read_to_end(&mut body)?;
    let version: serde_json::Value = serde_json::from_slice(&body)?;
    let url = version["webSocketDebuggerUrl"]
        .as_str()
        .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in its /json/version"))?;
    Ok(PortServer::Launched(url.to_string()))
}

/// Browser endpoint of the chrome://inspect/#remote-debugging server on `port`.
///
/// That server accepts `/devtools/browser` without the per-session UUID that `DevToolsActivePort` records, and serves no `/json/version` to look the UUID up from, so the port is all a client needs (verified on Chrome 154).
fn inspect_ws_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/devtools/browser")
}

/// Read DevToolsActivePort file and construct the WebSocket URL.
///
/// With `fallback_port`, a file this process is not permitted to read yields the chrome://inspect server on that port, when there is one, instead of an error.
fn read_devtools_active_port(
    user_data_dir: &Path,
    browser: Browser,
    fallback_port: Option<u16>,
) -> Result<String> {
    let port_path = user_data_dir.join("DevToolsActivePort");
    let label = browser.label();
    let scheme = browser.scheme();

    let content = match std::fs::read_to_string(&port_path) {
        Ok(content) => content,
        // Denied is not missing: the file may be there with remote debugging on, and only this process kept out of the directory. The enable-it steps below would send the user the wrong way.
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let tried = match fallback_port.map(|port| (port, probe_port(port))) {
                None => String::new(),
                // Only a chrome://inspect server can be this profile's own, since Chrome 136+ ignores --remote-debugging-port on the default profile.
                Some((port, Ok(PortServer::Inspect))) => {
                    eprintln!(
                        "Warning: cannot read {}: {e}. Connecting to port {port} instead; \
                         pass --port or set CHROME_PORT to connect by port without this warning.",
                        port_path.display()
                    );
                    return Ok(inspect_ws_url(port));
                }
                Some((port, Ok(PortServer::Launched(_)))) => format!(
                    "\n\nPort {port} was tried too, but the browser there was launched with \
                     --remote-debugging-port, so it is most likely a separate instance rather \
                     than this profile. Pass --port {port} if it is the one you want."
                ),
                Some((port, Err(why))) => format!("\n\nPort {port} was tried too, but {why}."),
            };
            bail!(
                "Could not read DevToolsActivePort at {}: {e}\n\n\
                 This process is not allowed to read the profile directory. On macOS that \
                 usually means the app running this command has not been allowed to access \
                 data from other apps: approve the privacy prompt, or allow it under System \
                 Settings > Privacy & Security. Security software or file permissions can do \
                 the same.\n\n\
                 You can also connect by port: pass --port with the port shown at \
                 {scheme}://inspect/#remote-debugging, or the one the browser was launched \
                 with.{tried}",
                port_path.display()
            );
        }
        Err(e) => bail!(
            "Could not read DevToolsActivePort at {}: {e}\n\n\
             Make sure {label} is running with remote debugging enabled:\n\
             1. Open {label}\n\
             2. Go to {scheme}://inspect/#remote-debugging\n\
             3. Enable the remote debugging server",
            port_path.display()
        ),
    };

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

/// Canonical spelling of a browser name, for recording and display — so a
/// daemon spawned by `--browser EDGE` or `--browser msedge` is still labelled
/// `edge` in `list-daemons`.
///
/// Falls back to the input for names we don't know: endpoint resolution has
/// already rejected those, so this is only reached for display.
pub fn canonical_name(name: &str) -> String {
    Browser::parse(name).map_or_else(|_| name.to_string(), |b| b.flag_name().to_string())
}

/// Human-readable browser name for messages the user reads — "Chrome",
/// "Microsoft Edge".
///
/// Falls back to the input for names we don't know, so a diagnostic never
/// silently claims the wrong browser.
pub fn display_name(name: &str) -> String {
    Browser::parse(name).map_or_else(|_| name.to_string(), |b| b.label().to_string())
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
    /// Case- and whitespace-insensitive, so `--browser Edge` and
    /// `CHROME_BROWSER=EDGE` work. The error quotes the name as typed.
    fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
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

    /// Canonical `--browser` spelling, for anything that records or displays
    /// the choice. Kept separate from [`Browser::scheme`] so the two can
    /// diverge if a browser ever needs different values.
    fn flag_name(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
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
        // Matched case-insensitively for the same reason as `parse`; the error
        // arms still report the channel as the user typed it.
        let normalized = channel.trim().to_ascii_lowercase();
        let channel_key = normalized.as_str();

        #[cfg(target_os = "macos")]
        {
            let home =
                dirs::home_dir().ok_or_else(|| anyhow!("Cannot determine home directory"))?;
            let base = home.join("Library/Application Support");
            let dir = match (self, channel_key) {
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
            let dir = match (self, channel_key) {
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
            let dir = match (self, channel_key) {
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
    fn parses_browsers_case_insensitively() {
        for name in ["Chrome", "CHROME", " chrome "] {
            assert_eq!(Browser::parse(name).unwrap(), Browser::Chrome, "{name}");
        }
        for name in ["Edge", "EDGE", "MSEdge", " edge "] {
            assert_eq!(Browser::parse(name).unwrap(), Browser::Edge, "{name}");
        }
    }

    #[test]
    fn channels_are_matched_case_insensitively() {
        for browser in [Browser::Chrome, Browser::Edge] {
            assert_eq!(
                browser.default_user_data_dir("stable").unwrap(),
                browser.default_user_data_dir("STABLE").unwrap()
            );
            assert_eq!(
                browser.default_user_data_dir("beta").unwrap(),
                browser.default_user_data_dir(" Beta ").unwrap()
            );
        }
    }

    /// The error must quote what the user typed, not the normalized form.
    #[test]
    fn unknown_browser_error_quotes_the_original_spelling() {
        let err = Browser::parse("FireFox").unwrap_err().to_string();
        assert!(err.contains("FireFox"), "{err}");
    }

    #[test]
    fn canonical_name_normalizes_spelling_and_aliases() {
        for name in ["edge", "Edge", "EDGE", "msedge", "MSEdge"] {
            assert_eq!(canonical_name(name), "edge", "{name}");
        }
        for name in ["chrome", "Chrome", "CHROME"] {
            assert_eq!(canonical_name(name), "chrome", "{name}");
        }
        // Unknown names pass through rather than being silently relabelled.
        assert_eq!(canonical_name("firefox"), "firefox");
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
        let ws = resolve_ws_url(
            Some("ws://127.0.0.1:9222/x"),
            None,
            None,
            "firefox",
            "stable",
        )
        .unwrap();
        assert_eq!(ws, "ws://127.0.0.1:9222/x");
    }

    /// `body` as a complete HTTP response, with the `Content-Length` a real server sends.
    fn http_reply(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length:{}\r\n\r\n{body}",
            body.len()
        )
    }

    /// The endpoint Chrome advertises when probed on `port`, since it echoes the probe's Host header.
    fn advertised_url(port: u16) -> String {
        format!("ws://127.0.0.1:{port}/devtools/browser/abc")
    }

    /// What a browser launched with `--remote-debugging-port` answers on `/json/version`.
    fn launched_reply(url: &str) -> String {
        http_reply("200 OK", &format!(r#"{{"webSocketDebuggerUrl": "{url}"}}"#))
    }

    /// What the chrome://inspect server answers on `/json/version`.
    fn inspect_reply() -> String {
        http_reply("404 Not Found", "")
    }

    /// Port of a local server that answers a single request with `reply(port)`, standing in for a DevTools server.
    fn serve_once(reply: impl FnOnce(u16) -> String + Send + 'static) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the request first: closing with it unread can reset the connection before the reply is seen.
            let mut request = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while request.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            stream.write_all(reply(port).as_bytes()).unwrap();
        });
        port
    }

    /// A port with nothing listening on it.
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn version_response_tells_the_servers_apart() {
        let parse = |reply: String| read_version_response(reply.as_bytes());
        assert_eq!(
            parse(launched_reply(&advertised_url(9))).unwrap(),
            PortServer::Launched(advertised_url(9))
        );
        assert_eq!(parse(inspect_reply()).unwrap(), PortServer::Inspect);
        // Anything else is not a DevTools server this CLI knows how to reach.
        assert!(parse(http_reply("200 OK", "{}")).is_err());
        assert!(parse(http_reply("500 Internal Server Error", "")).is_err());
        assert!(parse("SSH-2.0-OpenSSH_9.6\r\n".into()).is_err());
    }

    #[test]
    fn replies_beyond_the_cap_are_refused() {
        // A listener that is not Chrome can claim or send any amount, so both a declared body and the headers are held to the cap. Each reply below parses fine without it.
        let body = format!(r#"{{"webSocketDebuggerUrl": "{}"}}"#, advertised_url(9));
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length:{}\r\n\r\n{body}",
            MAX_PROBE_REPLY + 1
        );
        assert!(read_version_response(declared.as_bytes()).is_err());
        let padding = format!("\r\nX-Pad:{}\r\n", "a".repeat(MAX_PROBE_REPLY as usize));
        let padded = launched_reply(&advertised_url(9)).replacen("\r\n", &padding, 1);
        assert!(read_version_response(padded.as_bytes()).is_err());
    }

    #[test]
    fn port_short_circuits_profile_discovery() {
        // Like an explicit endpoint, a port needs no profile: the unreadable-profile case it exists for would otherwise fail before connecting.
        let port = serve_once(|_| inspect_reply());
        let ws =
            resolve_ws_url(None, Some(port), Some("/nonexistent"), "firefox", "stable").unwrap();
        assert_eq!(ws, format!("ws://127.0.0.1:{port}/devtools/browser"));
    }

    #[test]
    fn port_uses_the_url_a_launched_browser_advertises() {
        // A --remote-debugging-port server rejects /devtools/browser without its UUID, so the advertised URL is the only one that works.
        let port = serve_once(|p| launched_reply(&advertised_url(p)));
        let ws = resolve_ws_url(None, Some(port), None, "chrome", "stable").unwrap();
        assert_eq!(ws, advertised_url(port));
    }

    #[test]
    fn port_refuses_an_endpoint_advertised_elsewhere() {
        // Whatever holds the port writes the reply, so it must not be able to send the CLI to another host, port or scheme.
        let elsewhere: [fn(u16) -> String; 4] = [
            |p| format!("ws://203.0.113.9:{p}/devtools/browser/abc"),
            |_| "ws://127.0.0.1:1/devtools/browser/abc".to_string(),
            |p| format!("ws://127.0.0.1:{p}@203.0.113.9/devtools/browser/abc"),
            |p| format!("wss://127.0.0.1:{p}/devtools/browser/abc"),
        ];
        for url in elsewhere {
            let port = serve_once(move |p| launched_reply(&url(p)));
            let err = resolve_ws_url(None, Some(port), None, "chrome", "stable")
                .unwrap_err()
                .to_string();
            assert!(err.contains("which is not on 127.0.0.1"), "{err}");
        }
    }

    #[test]
    fn port_with_nothing_listening_says_so() {
        let err = resolve_ws_url(None, Some(closed_port()), None, "edge", "stable")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing is listening"), "{err}");
        assert!(err.contains("edge://inspect"), "{err}");
    }

    #[test]
    fn active_port_error_names_the_browser() {
        let dir = std::env::temp_dir().join("chrome-devtools-cli-nonexistent-profile");
        let err = resolve_ws_url(None, None, Some(dir.to_str().unwrap()), "edge", "stable")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Microsoft Edge is running"), "{err}");
        assert!(err.contains("edge://inspect"), "{err}");
    }

    #[test]
    fn missing_port_file_never_falls_back() {
        // No file means remote debugging is off or the browser is not running, so a guessed port would trade the enable-it steps for a bare connection failure.
        let dir = std::env::temp_dir().join("chrome-devtools-cli-nonexistent-profile");
        let err = read_devtools_active_port(&dir, Browser::Chrome, Some(closed_port()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Enable the remote debugging server"), "{err}");
    }

    /// Profile dir whose `DevToolsActivePort` exists but cannot be read, standing in for one that macOS or security software walls off. `None` when this process can read it anyway, as root can.
    #[cfg(unix)]
    fn unreadable_profile() -> Option<tempfile::TempDir> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("DevToolsActivePort");
        std::fs::write(&file, "9333\n/devtools/browser/x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::read(&file).is_err().then_some(dir)
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_default_profile_falls_back_to_inspect_server() {
        let Some(dir) = unreadable_profile() else {
            return;
        };
        let port = serve_once(|_| inspect_reply());
        let ws = read_devtools_active_port(dir.path(), Browser::Chrome, Some(port)).unwrap();
        assert_eq!(ws, format!("ws://127.0.0.1:{port}/devtools/browser"));
    }

    #[cfg(unix)]
    #[test]
    fn fallback_refuses_a_launched_browser() {
        // Chrome ignores --remote-debugging-port on the default profile, so a server launched that way is another instance, and attaching to it would drive the wrong browser.
        let Some(dir) = unreadable_profile() else {
            return;
        };
        let port = serve_once(|p| launched_reply(&advertised_url(p)));
        let err = read_devtools_active_port(dir.path(), Browser::Chrome, Some(port))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--remote-debugging-port"), "{err}");
        assert!(err.contains(&format!("--port {port}")), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn failed_fallback_keeps_the_full_explanation() {
        let Some(dir) = unreadable_profile() else {
            return;
        };
        let err = read_devtools_active_port(dir.path(), Browser::Chrome, Some(closed_port()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not allowed to read the profile directory"),
            "{err}"
        );
        assert!(err.contains("nothing is listening"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_explicit_profile_points_at_port_flag() {
        let Some(dir) = unreadable_profile() else {
            return;
        };
        let err = resolve_ws_url(None, None, dir.path().to_str(), "chrome", "stable")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--port"), "{err}");
        // The OS's own reason, which the message used to drop.
        assert!(err.contains("os error"), "{err}");
        assert!(!err.contains("Enable the remote debugging server"), "{err}");
    }
}

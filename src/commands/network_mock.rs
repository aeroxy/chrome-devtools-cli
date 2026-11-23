//! `mock-network` and `remove-network-mock`: answer a tab's requests from the
//! CLI instead of the network, through the Fetch domain.
//!
//! Chrome pauses every request matching `Fetch.enable`'s patterns until it is
//! answered, so a paused request must never wait on the CLI: `CdpClient`
//! answers them from every read of the socket, and the daemon keeps reading
//! between commands while any mock is set (`CdpClient::wants_idle_reads`).

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::fmt::Write;
use std::io::Read;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

#[derive(Debug, Clone)]
pub struct NetworkMock {
    /// URL glob as Chrome's Fetch domain reads it: `*` any characters, `?` one,
    /// `\` escapes.
    pub pattern: String,
    pub resource_type: Option<String>,
    pub action: MockAction,
    /// Requests answered so far, so a mock that never matched is easy to spot.
    pub hits: u64,
}

#[derive(Debug, Clone)]
pub enum MockAction {
    Fulfill {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Abort,
}

/// What `mock-network` was asked to set up, before the body file is read.
pub struct MockRequest<'a> {
    pub pattern: &'a str,
    pub status: Option<u16>,
    pub headers: Vec<String>,
    pub body: Option<&'a str>,
    pub body_file: Option<&'a str>,
    pub abort: bool,
    pub resource_type: Option<&'a str>,
}

/// Chrome's resource type names, for the `--resource-type` check. WebSocket
/// is not one: the Fetch domain can't intercept it and rejects the filter.
const RESOURCE_TYPES: &[&str] = &[
    "Document",
    "Stylesheet",
    "Image",
    "Media",
    "Font",
    "Script",
    "TextTrack",
    "XHR",
    "Fetch",
    "Prefetch",
    "EventSource",
    "Manifest",
    "SignedExchange",
    "Ping",
    "CSPViolationReport",
    "Preflight",
    "Other",
];

impl MockRequest<'_> {
    fn into_mock(self) -> Result<NetworkMock> {
        if self.pattern.is_empty() {
            bail!("The URL pattern is empty; use \"*\" to match every request");
        }
        let resource_type = match self.resource_type {
            None => None,
            Some(t) => Some(
                RESOURCE_TYPES
                    .iter()
                    .find(|known| known.eq_ignore_ascii_case(t))
                    .map(|known| known.to_string())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Unknown resource type '{t}'. Use one of: {}",
                            RESOURCE_TYPES.join(", ")
                        )
                    })?,
            ),
        };
        let action = if self.abort {
            MockAction::Abort
        } else {
            let status = self.status.unwrap_or(200);
            if !(100..=599).contains(&status) {
                bail!("--status {status} is not an HTTP status (100-599)");
            }
            let headers = self
                .headers
                .iter()
                .map(|h| parse_header(h))
                .collect::<Result<Vec<_>>>()?;
            let body = match (self.body, self.body_file) {
                (Some(text), _) => text.as_bytes().to_vec(),
                (None, Some(path)) => read_body_file(path)?,
                (None, None) => Vec::new(),
            };
            MockAction::Fulfill {
                status,
                headers,
                body,
            }
        };
        Ok(NetworkMock {
            pattern: self.pattern.to_string(),
            resource_type,
            action,
            hits: 0,
        })
    }
}

/// The largest `--body-file`. Every answer carries the body base64-encoded in
/// one CDP message, and Chrome closes the connection on a message past 100 MiB
/// (verified on Chrome 158), which would end the daemon.
const MAX_BODY_FILE_BYTES: u64 = 64 << 20;

/// Read `--body-file` in the daemon, which serves one command at a time. A
/// read that never ends (a FIFO nobody writes, /dev/stdin, /dev/zero) would
/// hold up every other caller, so only a regular file is read, and at most
/// [`MAX_BODY_FILE_BYTES`] of it.
fn read_body_file(path: &str) -> Result<Vec<u8>> {
    let context = || format!("Failed to read --body-file {path}");
    // Checked before opening: opening a FIFO waits for a writer.
    let metadata = std::fs::metadata(path).with_context(context)?;
    if !metadata.is_file() {
        bail!(
            "--body-file {path} is not a regular file. Save the body to a file first: the daemon \
             would wait on a pipe or a device, and every other command with it."
        );
    }
    let too_big = || {
        anyhow::anyhow!(
            "--body-file {path} is larger than {} MiB, the most one mocked response can carry",
            MAX_BODY_FILE_BYTES >> 20
        )
    };
    if metadata.len() > MAX_BODY_FILE_BYTES {
        return Err(too_big());
    }
    let mut body = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_BODY_FILE_BYTES + 1).read_to_end(&mut body))
        .with_context(context)?;
    // It grew after the check.
    if body.len() as u64 > MAX_BODY_FILE_BYTES {
        return Err(too_big());
    }
    Ok(body)
}

/// A header Chrome will accept. One it rejects would fail the reply after the
/// request was taken off the queue, leaving the page's request paused.
fn parse_header(header: &str) -> Result<(String, String)> {
    let Some((name, value)) = header.split_once(':') else {
        bail!("--header '{header}' is not 'Name: value'");
    };
    let (name, value) = (name.trim(), value.trim());
    if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
        bail!("--header '{header}': '{name}' is not a valid header name");
    }
    if value.contains(['\0', '\r', '\n']) {
        bail!("--header '{header}': the value has a line break or NUL in it");
    }
    Ok((name.to_string(), value.to_string()))
}

/// Chrome can't tell `fetch()` from XMLHttpRequest when it intercepts: it
/// reports both as XHR. So each of the two types matches either.
fn same_resource_type(wanted: &str, reported: &str) -> bool {
    let xhr_like = |t: &str| t == "XHR" || t == "Fetch";
    wanted == reported || (xhr_like(wanted) && xhr_like(reported))
}

/// The status line's phrase. Chrome supplies one only for codes it knows and
/// rejects a reply without one, which would leave the request paused.
fn reason_phrase(status: u16) -> &'static str {
    http::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason())
        .unwrap_or("Mocked")
}

/// Match `text` against a Fetch-domain URL glob: `*` is any run of
/// characters, `?` exactly one, `\` makes the next character literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative wildcard matching with one backtrack point, linear in practice.
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        let (literal, width) = match p.get(pi) {
            // A trailing backslash has nothing to escape and stands for itself.
            Some('\\') => match p.get(pi + 1) {
                Some(&c) => (Some(c), 2),
                None => (Some('\\'), 1),
            },
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
                continue;
            }
            Some('?') => (None, 1),
            Some(&c) => (Some(c), 1),
            None => (Some('\0'), 0),
        };
        let matches = width > 0 && literal.is_none_or(|c| c == t[ti]);
        if matches {
            pi += width;
            ti += 1;
        } else if let Some((star_pi, star_ti)) = star {
            pi = star_pi + 1;
            ti = star_ti + 1;
            star = Some((star_pi, star_ti + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The `Fetch.enable` patterns for a set of mocks.
fn fetch_patterns(mocks: &[NetworkMock]) -> Value {
    let patterns: Vec<Value> = mocks
        .iter()
        .map(|m| {
            let mut p = json!({"urlPattern": m.pattern, "requestStage": "Request"});
            if let Some(t) = &m.resource_type {
                p["resourceType"] = json!(t);
            }
            p
        })
        .collect();
    json!({ "patterns": patterns })
}

/// The answer to a `Fetch.requestPaused` event, as a CDP method and its
/// params, and the index of the mock that gave it. The most recently added
/// matching mock wins; a request none matches (Chrome's matching and ours
/// disagree, or the mock was just removed) goes on to the network.
pub fn reply_to_paused(
    mocks: &[NetworkMock],
    params: &Value,
) -> (&'static str, Value, Option<usize>) {
    let request_id = params["requestId"].clone();
    let url = params["request"]["url"].as_str().unwrap_or("");
    let resource_type = params["resourceType"].as_str().unwrap_or("");
    let matched = mocks.iter().rposition(|m| {
        glob_match(&m.pattern, url)
            && m.resource_type
                .as_deref()
                .is_none_or(|t| same_resource_type(t, resource_type))
    });
    let Some(i) = matched else {
        return (
            "Fetch.continueRequest",
            json!({"requestId": request_id}),
            None,
        );
    };
    let (method, params) = match &mocks[i].action {
        MockAction::Abort => (
            "Fetch.failRequest",
            json!({"requestId": request_id, "errorReason": "Failed"}),
        ),
        MockAction::Fulfill {
            status,
            headers,
            body,
        } => (
            "Fetch.fulfillRequest",
            json!({
                "requestId": request_id,
                "responseCode": status,
                "responsePhrase": reason_phrase(*status),
                "responseHeaders": headers
                    .iter()
                    .map(|(name, value)| json!({"name": name, "value": value}))
                    .collect::<Vec<_>>(),
                "body": base64::engine::general_purpose::STANDARD.encode(body),
            }),
        ),
    };
    (method, params, Some(i))
}

/// Make the session intercept exactly what `client.network_mocks` covers.
///
/// The HTTP cache is off while mocks are set: a cached response never
/// reaches the Fetch domain, so it would quietly bypass a mock.
pub async fn apply_mocks(client: &mut CdpClient, session_id: &str) -> Result<()> {
    if client.network_mocks.is_empty() {
        client
            .send_to_target(session_id, "Fetch.disable", json!({}))
            .await?;
        client
            .send_to_target(
                session_id,
                "Network.setCacheDisabled",
                json!({"cacheDisabled": false}),
            )
            .await?;
        return Ok(());
    }
    let patterns = fetch_patterns(&client.network_mocks);
    client
        .send_to_target(
            session_id,
            "Network.setCacheDisabled",
            json!({"cacheDisabled": true}),
        )
        .await?;
    client
        .send_to_target(session_id, "Fetch.enable", patterns)
        .await?;
    Ok(())
}

/// Mocks live on the daemon's long-lived session for the tab. A one-off
/// session detaches when the command ends and takes the interception with it.
fn require_persistent_session(client: &CdpClient, session_id: &str) -> Result<()> {
    if client.persistent_session.as_deref() != Some(session_id) {
        bail!(
            "Network mocks need the daemon's long-lived session for this tab, which this command \
             is not running in (no daemon, or it could not attach), so a mock would end with the \
             command. Run it again once the daemon is up."
        );
    }
    Ok(())
}

/// Add a mock, replacing any with the same pattern and resource type.
pub async fn mock_network(
    client: &mut CdpClient,
    session_id: &str,
    request: MockRequest<'_>,
) -> Result<CommandResult> {
    require_persistent_session(client, session_id)?;
    let mock = request.into_mock()?;
    let previous = client.network_mocks.clone();
    client
        .network_mocks
        .retain(|m| !(m.pattern == mock.pattern && m.resource_type == mock.resource_type));
    let summary = describe_mock(&mock);
    client.network_mocks.push(mock);
    if let Err(e) = apply_mocks(client, session_id).await {
        // A mock Chrome refused must not linger, or every later change would
        // resend it and fail too: go back to what applied before.
        client.network_mocks = previous;
        let _ = apply_mocks(client, session_id).await;
        return Err(e);
    }
    Ok(CommandResult::output(format!(
        "Mocked {summary}. It applies to this tab while the daemon runs (see `caffeinate`), \
         and only to requests made from now on: reload to mock the page's own requests."
    )))
}

/// Remove the mocks with exactly this pattern, or all of them.
pub async fn remove_network_mock(
    client: &mut CdpClient,
    session_id: &str,
    pattern: Option<&str>,
) -> Result<CommandResult> {
    require_persistent_session(client, session_id)?;
    let before = client.network_mocks.len();
    match pattern {
        Some(p) => client.network_mocks.retain(|m| m.pattern != p),
        None => client.network_mocks.clear(),
    }
    let removed = before - client.network_mocks.len();
    apply_mocks(client, session_id).await?;
    let noun = if removed == 1 { "mock" } else { "mocks" };
    Ok(CommandResult::output(format!(
        "Removed {removed} network {noun}; {} left on this tab.",
        client.network_mocks.len()
    )))
}

pub fn list_network_mocks(client: &CdpClient, format: OutputFormat) -> Result<CommandResult> {
    let mocks = &client.network_mocks;
    if !format.is_text() {
        let rows: Vec<Value> = mocks
            .iter()
            .map(|m| {
                let mut row = json!({
                    "pattern": m.pattern,
                    "resourceType": m.resource_type,
                    "hits": m.hits,
                });
                match &m.action {
                    MockAction::Abort => row["action"] = json!("abort"),
                    MockAction::Fulfill {
                        status,
                        headers,
                        body,
                    } => {
                        row["action"] = json!("fulfill");
                        row["status"] = json!(status);
                        row["headers"] = json!(headers
                            .iter()
                            .map(|(n, v)| format!("{n}: {v}"))
                            .collect::<Vec<_>>());
                        row["bodyBytes"] = json!(body.len());
                    }
                }
                row
            })
            .collect();
        return Ok(CommandResult::output(format_structured(
            &json!(rows),
            format,
        )?));
    }
    if mocks.is_empty() {
        return Ok(CommandResult::output("No network mocks on this tab."));
    }
    let mut out = String::new();
    for m in mocks.iter().rev() {
        writeln!(out, "{} — {} hit(s)", describe_mock(m), m.hits).unwrap();
    }
    Ok(CommandResult::output(out))
}

fn describe_mock(m: &NetworkMock) -> String {
    let scope = match &m.resource_type {
        Some(t) => format!("{} ({t})", m.pattern),
        None => m.pattern.clone(),
    };
    match &m.action {
        MockAction::Abort => format!("{scope} → abort"),
        MockAction::Fulfill { status, body, .. } => {
            format!("{scope} → {status}, {} byte body", body.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_the_fetch_domain() {
        assert!(glob_match("*", "https://a.test/x"));
        assert!(glob_match("*/api/*", "https://a.test/api/user?id=1"));
        assert!(!glob_match("*/api/*", "https://a.test/apix"));
        assert!(glob_match("https://a.test/?", "https://a.test/x"));
        assert!(!glob_match("https://a.test/?", "https://a.test/xy"));
        assert!(glob_match("*.png", "https://a.test/i.png"));
        assert!(!glob_match("*.png", "https://a.test/i.png?v=2"));
        assert!(glob_match(r"*\?v=2", "https://a.test/i.png?v=2"));
        assert!(!glob_match(r"*\*", "https://a.test/x"));
        assert!(glob_match("", ""));
        assert!(glob_match("a\\", "a\\") && !glob_match("a\\", "ab"));
    }

    fn mock(pattern: &str, action: MockAction) -> NetworkMock {
        NetworkMock {
            pattern: pattern.into(),
            resource_type: None,
            action,
            hits: 0,
        }
    }

    #[test]
    fn the_latest_matching_mock_answers() {
        let mocks = vec![
            mock("*", MockAction::Abort),
            mock(
                "*/api/*",
                MockAction::Fulfill {
                    status: 201,
                    headers: vec![("Content-Type".into(), "application/json".into())],
                    body: b"{}".to_vec(),
                },
            ),
        ];
        let paused = |url: &str| json!({"requestId": "r1", "resourceType": "Fetch", "request": {"url": url}});

        let (method, params, matched) = reply_to_paused(&mocks, &paused("https://a.test/api/u"));
        assert_eq!((method, matched), ("Fetch.fulfillRequest", Some(1)));
        assert_eq!(params["responseCode"], 201);
        assert_eq!(params["responsePhrase"], "Created");
        assert_eq!(params["body"], "e30=");
        assert_eq!(params["responseHeaders"][0]["name"], "Content-Type");

        let (method, _, matched) = reply_to_paused(&mocks, &paused("https://a.test/logo.png"));
        assert_eq!((method, matched), ("Fetch.failRequest", Some(0)));

        let (method, params, matched) = reply_to_paused(&[], &paused("https://a.test/"));
        assert_eq!(
            (method, params["requestId"].as_str(), matched),
            ("Fetch.continueRequest", Some("r1"), None)
        );
    }

    /// Chrome reports both fetch() and XMLHttpRequest as XHR when it
    /// intercepts, and has no phrase of its own for unusual codes.
    #[test]
    fn replies_fit_what_chrome_reports_and_accepts() {
        let mut fetch_only = mock("*", MockAction::Abort);
        fetch_only.resource_type = Some("Fetch".into());
        let mocks = [fetch_only];
        let paused = |ty: &str| json!({"requestId": "r", "resourceType": ty, "request": {"url": "https://a.test/"}});
        assert_eq!(reply_to_paused(&mocks, &paused("XHR")).2, Some(0));
        assert_eq!(reply_to_paused(&mocks, &paused("Script")).2, None);
        assert_eq!(reason_phrase(404), "Not Found");
        assert_eq!(reason_phrase(599), "Mocked");
    }

    #[test]
    fn requests_are_checked_before_anything_changes() {
        let req = |status: Option<u16>, headers: Vec<&str>, resource_type: Option<&'static str>| {
            MockRequest {
                pattern: "*",
                status,
                headers: headers.into_iter().map(String::from).collect(),
                body: Some("x"),
                body_file: None,
                abort: false,
                resource_type,
            }
        };
        assert!(req(Some(99), vec![], None)
            .into_mock()
            .unwrap_err()
            .to_string()
            .contains("not an HTTP status"));
        assert!(req(None, vec!["NoColon"], None)
            .into_mock()
            .unwrap_err()
            .to_string()
            .contains("Name: value"));
        let refused = |headers: Vec<&str>, ty: Option<&'static str>| {
            req(None, headers, ty).into_mock().unwrap_err().to_string()
        };
        assert!(refused(vec!["Bad Name: v"], None).contains("not a valid header name"));
        assert!(refused(vec!["X-A: a\rb"], None).contains("line break"));
        assert!(refused(vec![], Some("WebSocket")).contains("Unknown resource type"));
        assert!(req(None, vec![], Some("Pic"))
            .into_mock()
            .unwrap_err()
            .to_string()
            .contains("Unknown resource type"));
        let ok = req(None, vec!["X-A: 1"], Some("xhr")).into_mock().unwrap();
        assert_eq!(ok.resource_type.as_deref(), Some("XHR"));
    }

    #[test]
    fn a_body_file_is_read_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.bin");
        let bytes = [0u8, 0xff, 0xfe, b'\n', b'{', b'}'];
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_body_file(path.to_str().unwrap()).unwrap(), bytes);
    }

    /// The daemon serves one command at a time: nothing may wait on the file.
    #[test]
    fn a_body_file_that_could_stall_the_daemon_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let refused = |path: &std::path::Path| {
            read_body_file(path.to_str().unwrap())
                .unwrap_err()
                .to_string()
        };
        assert!(refused(dir.path()).contains("not a regular file"));
        #[cfg(unix)]
        {
            assert!(refused(std::path::Path::new("/dev/zero")).contains("not a regular file"));
            // A FIFO nobody writes: opening it would block forever.
            let fifo = dir.path().join("fifo");
            let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            assert!(refused(&fifo).contains("not a regular file"));
        }
        // Sparse, so the test writes nothing.
        let big = dir.path().join("big");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_BODY_FILE_BYTES + 1)
            .unwrap();
        assert!(refused(&big).contains("larger than 64 MiB"));
    }
}

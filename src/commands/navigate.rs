use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::constants::NAVIGATION_TIMEOUT_MS;
use crate::result::CommandResult;

/// Navigate to a URL, go back/forward in history, or reload the page.
pub async fn navigate(
    client: &mut CdpClient,
    session_id: &str,
    url: Option<&str>,
    back: bool,
    forward: bool,
    reload: bool,
    extra_headers: Option<&str>,
    output: Option<&str>,
) -> Result<CommandResult> {
    // Validate navigation intent before mutating session state
    let intent_count = [back, forward, reload, url.is_some()]
        .iter()
        .filter(|&&b| b)
        .count();
    if intent_count == 0 {
        bail!("URL required (or use --back, --forward, --reload)");
    }
    if intent_count > 1 {
        bail!("Conflicting navigation intents: only one of URL, --back, --forward, or --reload can be specified");
    }

    super::pages::apply_extra_headers(client, session_id, extra_headers).await?;

    let navigate_result = async {
        if back {
            return go_back(client, session_id, output).await;
        }
        if forward {
            return go_forward(client, session_id, output).await;
        }
        if reload {
            return do_reload(client, session_id, output).await;
        }

        let url = url.unwrap(); // safe: validated above

        let result = client
            .send_to_target(session_id, "Page.navigate", json!({"url": url}))
            .await?;

        if let Some(err) = result.get("errorText").and_then(|v| v.as_str()) {
            bail!("Navigation error: {err}");
        }

        wait_for_load(client, session_id, NAVIGATION_TIMEOUT_MS, None).await?;
        let result =
            CommandResult::output(format!("Navigated to {url}")).with_navigated_to(url.to_string());
        Ok(result.save_output(output).await?)
    }
    .await;

    // Only clear headers that this invocation applied.
    if extra_headers.is_some() {
        if let Err(e) = super::pages::clear_extra_headers(client, session_id).await {
            eprintln!("Warning: failed to clear extra headers: {e}");
        }
    }

    navigate_result
}

async fn go_back(
    client: &mut CdpClient,
    session_id: &str,
    output: Option<&str>,
) -> Result<CommandResult> {
    let history = client
        .send_to_target(session_id, "Page.getNavigationHistory", json!({}))
        .await?;

    let current_index = history["currentIndex"].as_i64().unwrap_or(0);
    let entries = history["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No navigation history entries"))?;

    if current_index <= 0 {
        bail!("Already at the beginning of history");
    }

    let prev_entry = &entries[(current_index - 1) as usize];
    let entry_id = prev_entry["id"].as_i64().unwrap_or(0);

    traverse_history(client, session_id, entry_id).await?;
    let url = prev_entry["url"].as_str().unwrap_or("unknown");
    let result = CommandResult::output(format!("Navigated back to {url}"))
        .with_navigated_to(url.to_string());
    Ok(result.save_output(output).await?)
}

async fn go_forward(
    client: &mut CdpClient,
    session_id: &str,
    output: Option<&str>,
) -> Result<CommandResult> {
    let history = client
        .send_to_target(session_id, "Page.getNavigationHistory", json!({}))
        .await?;

    let current_index = history["currentIndex"].as_i64().unwrap_or(0) as usize;
    let entries = history["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No navigation history entries"))?;

    if current_index + 1 >= entries.len() {
        bail!("Already at the end of history");
    }

    let next_entry = &entries[current_index + 1];
    let entry_id = next_entry["id"].as_i64().unwrap_or(0);

    traverse_history(client, session_id, entry_id).await?;
    let url = next_entry["url"].as_str().unwrap_or("unknown");
    let result = CommandResult::output(format!("Navigated forward to {url}"))
        .with_navigated_to(url.to_string());
    Ok(result.save_output(output).await?)
}

/// Go to a history entry and wait until the page shows it.
///
/// `Page.navigateToHistoryEntry` returns before the traversal commits, and an
/// entry that `pushState` made can keep the document and even the URL, so
/// the wait is for the commit itself: a new document, or a navigation within
/// one, in a frame the traversal moves ([`traversal_frames`]).
async fn traverse_history(client: &mut CdpClient, session_id: &str, entry_id: i64) -> Result<()> {
    let main_frame = super::input::main_frame_id(client, session_id).await?;
    // Only this traversal's events count.
    client.clear_events();
    client
        .send_to_target(
            session_id,
            "Page.navigateToHistoryEntry",
            json!({"entryId": entry_id}),
        )
        .await?;
    // One budget for both phases: a slow commit must not give the load wait
    // a fresh full timeout.
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(NAVIGATION_TIMEOUT_MS);
    let frames = traversal_frames(&client.events, session_id, &main_frame);
    // None named: the entry moves only an iframe from another site, which
    // commits in its own process, out of this session's sight.
    if !frames.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        client
            .wait_for_event_match(
                &["Page.frameNavigated", "Page.navigatedWithinDocument"],
                remaining,
                |method, p| {
                    let frame = if method == "Page.frameNavigated" {
                        &p["frame"]["id"]
                    } else {
                        &p["frameId"]
                    };
                    frame
                        .as_str()
                        .is_some_and(|f| frames.iter().any(|w| w == f))
                },
            )
            .await
            .map_err(history_wait_error)?;
    }
    wait_for_load_until(client, session_id, deadline, NAVIGATION_TIMEOUT_MS, None).await
}

/// A lost connection keeps its type, which the daemon exits on; any other
/// failure of the wait is the wait running out.
fn history_wait_error(e: anyhow::Error) -> anyhow::Error {
    if crate::cdp::is_connection_or_page_failure(&e) {
        e
    } else {
        anyhow::anyhow!(
            "Timeout ({NAVIGATION_TIMEOUT_MS}ms) waiting for the history navigation to happen"
        )
    }
}

/// The frames whose commit ends a history traversal, from the events read
/// while `Page.navigateToHistoryEntry` was answered.
///
/// Chrome names each frame of the page that a traversal navigates
/// (`Page.frameStartedNavigating`, `Page.frameStartedLoading`) before it
/// replies, even while the page is busy: the browser sends them. Verified on
/// Chrome 158. An entry that an iframe's own navigation made moves only that
/// iframe, so the main frame never commits. `Page.getNavigationHistory` can't
/// tell instead: its `currentIndex` points at the entry before the commit.
fn traversal_frames(
    events: &std::collections::VecDeque<serde_json::Value>,
    session_id: &str,
    main_frame: &str,
) -> Vec<String> {
    let mut started: Vec<String> = events
        .iter()
        .filter(|e| e["sessionId"].as_str() == Some(session_id))
        .filter(|e| {
            matches!(
                e["method"].as_str(),
                Some("Page.frameStartedNavigating" | "Page.frameStartedLoading")
            )
        })
        .filter_map(|e| e["params"]["frameId"].as_str().map(str::to_string))
        .collect();
    started.dedup();
    // When the main frame moves too, its commit is the one that counts.
    if started.iter().any(|f| f == main_frame) {
        vec![main_frame.to_string()]
    } else {
        started
    }
}

async fn do_reload(
    client: &mut CdpClient,
    session_id: &str,
    output: Option<&str>,
) -> Result<CommandResult> {
    let before = document_mark(client, session_id).await?;
    // Chrome refuses the reload if a different document committed after the
    // mark, instead of applying it to that document and reporting success.
    client
        .send_to_target(
            session_id,
            "Page.reload",
            json!({"loaderId": before.loader_id}),
        )
        .await?;
    wait_for_load(client, session_id, NAVIGATION_TIMEOUT_MS, Some(&before)).await?;
    let url = client.current_url(session_id).await?;
    let result = CommandResult::output("Reloaded page".to_string());
    Ok(result.save_output(output).await?.with_navigated_to(url))
}

/// Which document is showing, by the browser's loader ID for the main frame.
/// A reload keeps the URL, and the page can replace `performance` (and its
/// time origin), so neither page-provided value can tell the two documents
/// apart. The loader ID is Chrome's, from the frame tree.
pub struct DocumentMark {
    loader_id: String,
}

/// The main frame's loader ID from a `Page.getFrameTree` reply.
fn loader_id_from_tree(tree: &serde_json::Value) -> Result<String> {
    tree["frameTree"]["frame"]["loaderId"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Could not read the document's loader ID: {tree}"))
}

/// Chrome's identity for the current document, independent of page scripts.
async fn document_loader_id(client: &mut CdpClient, session_id: &str) -> Result<String> {
    let tree = client
        .send_to_target(session_id, "Page.getFrameTree", json!({}))
        .await?;
    loader_id_from_tree(&tree)
}

pub async fn document_mark(client: &mut CdpClient, session_id: &str) -> Result<DocumentMark> {
    Ok(DocumentMark {
        loader_id: document_loader_id(client, session_id).await?,
    })
}

/// A navigation can destroy the context between probes; that is a reason to
/// poll again, not to fail the wait.
fn transient_context_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Execution context was destroyed") || msg.contains("Cannot find context")
}

/// Whether `wait_for_load`'s probe shows the load done: readyState
/// "complete", in a document other than `previous` when there is one.
fn load_finished(
    result: &serde_json::Value,
    previous: Option<&DocumentMark>,
    loader_id: Option<&str>,
) -> bool {
    match previous {
        None => result["result"]["value"] == "complete",
        Some(p) => {
            result["result"]["value"] == "complete" && loader_id.is_some_and(|id| id != p.loader_id)
        }
    }
}

/// Poll until document.readyState is "complete" or the timeout expires.
///
/// With `previous`, the document must also have changed from that one first.
/// `Page.reload` and `Page.navigateToHistoryEntry` return before the old page
/// goes away, and its readyState is already "complete".
pub async fn wait_for_load(
    client: &mut CdpClient,
    session_id: &str,
    timeout_ms: u64,
    previous: Option<&DocumentMark>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    wait_for_load_until(client, session_id, deadline, timeout_ms, previous).await
}

/// `wait_for_load` with a caller-owned deadline, so history traversal can
/// spend the same navigation budget on its commit and this load wait.
async fn wait_for_load_until(
    client: &mut CdpClient,
    session_id: &str,
    deadline: tokio::time::Instant,
    timeout_ms: u64,
    previous: Option<&DocumentMark>,
) -> Result<()> {
    let timed_out =
        || anyhow::anyhow!("Timeout ({timeout_ms}ms) waiting for page readyState=complete");
    let pause = std::time::Duration::from_millis(100);

    tokio::time::sleep(pause).await;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(timed_out());
        }
        // A probe must itself end by the shared deadline: sending it with the
        // default call timeout would let one slow page extend the whole wait.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());

        // The document identity comes from Chrome, not the page: a page can
        // replace `performance`, and a reload can keep the same URL.
        let loader = if previous.is_some() {
            let tree = client
                .send_to_target_within(session_id, "Page.getFrameTree", json!({}), remaining)
                .await;
            match tree.and_then(|tree| loader_id_from_tree(&tree)) {
                Ok(id) => Some(id),
                Err(e) if transient_context_error(&e) => {
                    tokio::time::sleep(pause.min(remaining)).await;
                    continue;
                }
                Err(e) => {
                    return Err(e.context(format!("wait_for_load failed for session {session_id}")))
                }
            }
        } else {
            None
        };
        let result = client
            .send_to_target_within(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "document.readyState",
                    "returnByValue": true,
                }),
                remaining,
            )
            .await;

        match result {
            Ok(val) => {
                if load_finished(&val, previous, loader.as_deref()) {
                    return Ok(());
                }
            }
            Err(e) if transient_context_error(&e) => {
                tokio::time::sleep(pause.min(remaining)).await;
            }
            Err(e) => {
                // Context rather than a new error, so a lost connection keeps
                // its type and the daemon still exits on it.
                return Err(e.context(format!("wait_for_load failed for session {session_id}")));
            }
        }

        tokio::time::sleep(pause.min(remaining)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(session: &str, method: &str, frame: &str) -> serde_json::Value {
        json!({"sessionId": session, "method": method, "params": {"frameId": frame}})
    }

    /// What Chrome 158 sent before replying to `Page.navigateToHistoryEntry`.
    #[test]
    fn traversal_waits_for_the_frames_chrome_names() {
        let main_frame = started("s1", "Page.frameStartedNavigating", "MAIN");
        let iframe = [
            started("s1", "Page.frameStartedNavigating", "CHILD"),
            started("s1", "Page.frameStartedLoading", "CHILD"),
        ];
        let events = [main_frame, iframe[0].clone()].into();
        assert_eq!(traversal_frames(&events, "s1", "MAIN"), ["MAIN"]);
        // An entry an iframe's navigation made: the main frame never commits.
        let events = iframe.clone().into();
        assert_eq!(traversal_frames(&events, "s1", "MAIN"), ["CHILD"]);
        // An iframe from another site commits in its own process: nothing.
        assert!(traversal_frames(&Default::default(), "s1", "MAIN").is_empty());
        // Another session's events are no part of it.
        let other = [started("s2", "Page.frameStartedLoading", "MAIN")].into();
        assert!(traversal_frames(&other, "s1", "MAIN").is_empty());
    }

    #[test]
    fn a_lost_connection_is_not_reported_as_a_timeout() {
        let lost = history_wait_error(anyhow::Error::new(crate::cdp::ConnectionLost(
            "WebSocket closed by server".into(),
        )));
        assert!(lost.downcast_ref::<crate::cdp::ConnectionLost>().is_some());
        let waited = history_wait_error(anyhow::anyhow!("Timeout waiting for any event"));
        assert!(waited.to_string().starts_with(&format!(
            "Timeout ({NAVIGATION_TIMEOUT_MS}ms) waiting for the history"
        )));
    }

    /// Only readyState still comes from the page, read by value; the page's
    /// JSON.stringify, replaced or not, is never involved.
    #[test]
    fn a_load_needs_only_readystate_by_value() {
        let state = |s: &str| json!({"result": {"type": "string", "value": s}});
        assert!(load_finished(&state("complete"), None, None));
        assert!(!load_finished(&state("interactive"), None, None));

        let old = DocumentMark {
            loader_id: "old".into(),
        };
        assert!(!load_finished(&state("complete"), Some(&old), Some("old")));
        assert!(load_finished(&state("complete"), Some(&old), Some("new")));
        assert!(!load_finished(
            &state("interactive"),
            Some(&old),
            Some("new")
        ));
        // Without a new identity the old document may still be showing.
        assert!(!load_finished(&state("complete"), Some(&old), None));
    }

    /// A page that replaces `performance` or rewrites its URL cannot fake a
    /// reload: the identity comes from Chrome's frame tree.
    #[test]
    fn a_reload_identity_is_browser_reported() {
        let tree = json!({"frameTree": {"frame": {"id": "MAIN", "loaderId": "L2"}}});
        assert_eq!(loader_id_from_tree(&tree).unwrap(), "L2");
        assert!(loader_id_from_tree(&json!({"frameTree": {}})).is_err());
    }
}

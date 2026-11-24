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
        wait_for_event_from_session(client, remaining, |e| {
            commit_from_session(e, session_id, &frames)
        })
        .await
        .map_err(history_wait_error)?;
    }
    wait_for_load_until(client, session_id, deadline, NAVIGATION_TIMEOUT_MS, None).await
}

/// A commit that ends this traversal must come from the session running it.
/// A failed detach can leave a stale session on this tab reporting the same
/// main-frame (or same-process iframe) commit, which is not this traversal's.
fn commit_from_session(event: &serde_json::Value, session_id: &str, frames: &[String]) -> bool {
    if event["sessionId"].as_str() != Some(session_id) {
        return false;
    }
    let frame = match event["method"].as_str() {
        Some("Page.frameNavigated") => &event["params"]["frame"]["id"],
        Some("Page.navigatedWithinDocument") => &event["params"]["frameId"],
        _ => return false,
    };
    frame
        .as_str()
        .is_some_and(|f| frames.iter().any(|w| w == f))
}

/// Read until an event from the acting session matches, checking both buffered
/// and freshly read events. The session lives on the event envelope, so the
/// predicate needs the whole event, not just its method and params.
async fn wait_for_event_from_session<F>(
    client: &mut CdpClient,
    timeout: std::time::Duration,
    mut matches: F,
) -> Result<serde_json::Value>
where
    F: FnMut(&serde_json::Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(idx) = client.events.iter().position(&mut matches) {
            if let Some(event) = client.events.remove(idx) {
                return Ok(event);
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("Timeout waiting for an event from this session");
        }
        let text = tokio::time::timeout(remaining, client.read_text())
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for an event from this session"))??;
        let event: serde_json::Value = serde_json::from_str(&text)?;
        if event.get("method").is_some() {
            if matches(&event) {
                return Ok(event);
            }
            client.push_event(event);
        }
    }
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

/// The frame tree's loader ID, read within the navigation's remaining budget.
async fn loader_id_within(
    client: &mut CdpClient,
    session_id: &str,
    remaining: std::time::Duration,
) -> Result<String> {
    let tree = client
        .send_to_target_within(session_id, "Page.getFrameTree", json!({}), remaining)
        .await?;
    loader_id_from_tree(&tree)
}

/// The same document answered the readiness probe only if its loader ID did
/// not change while the probe ran. A commit in between needs another poll.
fn same_document(before: Option<&str>, after: Option<&str>) -> bool {
    before.is_some() && before == after
}

/// A CDP call given the remaining budget can still spend extra time on a
/// liveness check and a termination write after its own limit. Bound the whole
/// probe so a silent page cannot hold the daemon past the navigation deadline.
async fn probe_within_deadline<T, F, E>(
    remaining: std::time::Duration,
    timed_out: E,
    probe: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
    E: Fn() -> anyhow::Error,
{
    match tokio::time::timeout(remaining, probe).await {
        Ok(result) => result,
        Err(_) => Err(timed_out()),
    }
}

/// A navigation can destroy the context between probes; that is a reason to
/// poll again, not to fail the wait.
fn transient_context_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("Execution context was destroyed") || msg.contains("Cannot find context")
}

/// A ready reply counts only inside the navigation's shared budget. A reply a
/// liveness ping set aside can arrive after that deadline; reporting success
/// then would claim the navigation finished after its allowance ran out.
fn readiness_in_budget(
    finished: bool,
    now: tokio::time::Instant,
    deadline: tokio::time::Instant,
    timeout_ms: u64,
) -> Result<bool> {
    if !finished {
        return Ok(false);
    }
    if now >= deadline {
        return Err(anyhow::anyhow!(
            "Timeout ({timeout_ms}ms) waiting for page readyState=complete"
        ));
    }
    Ok(true)
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
            match loader_id_within(client, session_id, remaining).await {
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
        // The frame-tree probe may have used up the budget. A readiness call
        // with the original allowance could return success after the deadline.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let result = probe_within_deadline(
            remaining,
            &timed_out,
            client.send_to_target_within(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "document.readyState",
                    "returnByValue": true,
                }),
                remaining,
            ),
        )
        .await;

        match result {
            Ok(val) => {
                let finished = load_finished(&val, previous, loader.as_deref());
                if finished && previous.is_some() {
                    // The page can commit another document between the frame
                    // tree probe above and the readiness probe. A `complete`
                    // from that new document would otherwise pass with the
                    // earlier loader ID, so confirm the same one answered both.
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(timed_out());
                    }
                    match loader_id_within(client, session_id, remaining).await {
                        Ok(after) if same_document(loader.as_deref(), Some(&after)) => {}
                        Ok(_) => {
                            tokio::time::sleep(pause.min(remaining)).await;
                            continue;
                        }
                        Err(e) if transient_context_error(&e) => {
                            tokio::time::sleep(pause.min(remaining)).await;
                            continue;
                        }
                        Err(e) => {
                            return Err(
                                e.context(format!("wait_for_load failed for session {session_id}"))
                            )
                        }
                    }
                }
                if readiness_in_budget(finished, tokio::time::Instant::now(), deadline, timeout_ms)?
                {
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

    /// A failed detach can leave another session on this tab; its commit must
    /// not end the wait for this traversal.
    #[test]
    fn a_history_commit_must_come_from_the_traversing_session() {
        let main = json!({
            "sessionId": "s1",
            "method": "Page.frameNavigated",
            "params": {"frame": {"id": "MAIN"}},
        });
        assert!(commit_from_session(&main, "s1", &["MAIN".into()]));
        // The same main frame reported by a stale session does not count.
        assert!(!commit_from_session(&main, "s2", &["MAIN".into()]));
        assert!(!commit_from_session(&main, "s1", &["CHILD".into()]));
        let within = started("s1", "Page.navigatedWithinDocument", "MAIN");
        assert!(commit_from_session(&within, "s1", &["MAIN".into()]));
        assert!(!commit_from_session(&within, "s2", &["MAIN".into()]));
        // An unrelated method is not a commit.
        let other = started("s1", "Page.frameStartedLoading", "MAIN");
        assert!(!commit_from_session(&other, "s1", &["MAIN".into()]));
    }

    #[test]
    fn a_readiness_reply_from_a_replaced_document_is_not_accepted() {
        // The loader ID before and after the readiness probe must name the
        // same document; a commit in between sends the wait back to polling.
        assert!(same_document(Some("L2"), Some("L2")));
        assert!(!same_document(Some("L2"), Some("L3")));
        assert!(!same_document(Some("L2"), None));
        assert!(!same_document(None, None));
    }

    /// A readiness probe that hangs cannot hold the daemon past the navigation
    /// budget, even when its own CDP call is still doing liveness recovery.
    #[tokio::test]
    async fn a_readiness_probe_is_bounded_by_the_navigation_deadline() {
        let started = tokio::time::Instant::now();
        let slow_probe = async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok::<(), anyhow::Error>(())
        };
        let err = probe_within_deadline(
            std::time::Duration::from_millis(10),
            || anyhow::anyhow!("late"),
            slow_probe,
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(err.to_string(), "late");
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

    /// A `complete` reply read after the shared deadline is too late: the
    /// navigation budget has expired, and reporting success would hide that.
    #[test]
    fn a_readiness_reply_after_the_deadline_is_not_success() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let before = deadline - std::time::Duration::from_millis(1);
        assert!(readiness_in_budget(true, before, deadline, 1000).unwrap());
        assert!(!readiness_in_budget(false, before, deadline, 1000).unwrap());
        let err = readiness_in_budget(true, deadline, deadline, 1000).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Timeout (1000ms) waiting for page readyState=complete"
        );
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

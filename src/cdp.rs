use crate::friendly;
use anyhow::{anyhow, bail, Result};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde_json::{json, Value};

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Trait abstracting Chrome DevTools Protocol operations.
///
/// Enables dependency injection for testing command logic without
/// requiring a live Chrome/WebSocket connection.
///
/// NOTE: Full trait implementation requires the `async-trait` crate.
/// Currently `CdpClient` is used directly; this trait is a placeholder
/// for future mock-based testing.
#[allow(dead_code)]
pub trait CdpClientTrait: Debug + Send {
    fn send(
        &mut self,
        method: &str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>>;
    fn send_to_target(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + '_>>;
    fn current_url(
        &mut self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;
    fn resolve_page(
        &mut self,
        target: Option<&str>,
        page: Option<usize>,
    ) -> Pin<Box<dyn Future<Output = Result<TargetInfo>> + Send + '_>>;
    fn attach_to_target(
        &mut self,
        target_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;
    fn detach_from_target(
        &mut self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn activate_target(
        &mut self,
        target_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn create_target(
        &mut self,
        url: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;
    fn close_target(
        &mut self,
        target_id: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn set_dialog_action(&mut self, action: Option<String>);
}

/// Concrete CDP client backed by a WebSocket connection.
#[derive(Debug)]
pub struct CdpClient {
    write: SplitSink<WsStream, Message>,
    read: SplitStream<WsStream>,
    next_id: u64,
    /// Dialog action to automatically handle JavaScript dialogs during command execution.
    /// Valid values: "accept", "dismiss", or custom prompt text.
    pub dialog_action: Option<String>,
    /// Buffer for storing unhandled events (e.g., navigation events)
    pub events: std::collections::VecDeque<Value>,
}

const MAX_BUFFERED_EVENTS: usize = 1000;

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub target_id: String,
    pub title: String,
    pub url: String,
    #[allow(dead_code)]
    pub target_type: String,
}

impl CdpClient {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _) = connect_async(ws_url)
            .await
            .map_err(|e| anyhow!("Failed to connect to Chrome at {ws_url}: {e}"))?;
        let (write, read) = ws.split();
        Ok(Self {
            write,
            read,
            next_id: 1,
            dialog_action: None,
            events: std::collections::VecDeque::new(),
        })
    }

    /// Clear all buffered events.
    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    fn push_event(&mut self, event: Value) {
        Self::push_to_buffer(&mut self.events, event);
    }

    fn push_to_buffer(events: &mut std::collections::VecDeque<Value>, event: Value) {
        events.push_back(event);
        if events.len() > MAX_BUFFERED_EVENTS {
            events.pop_front();
        }
    }

    /// Send a browser-level CDP command.
    pub async fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        self.send_raw(method, params, None).await
    }

    /// Send a page-level CDP command (with session ID from attach_to_target).
    pub async fn send_to_target(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.send_raw(method, params, Some(session_id)).await
    }

    /// Send a command and return the message ID immediately without waiting for response.
    pub async fn send_raw_no_wait(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;

        let mut msg = json!({"id": id, "method": method});
        if !params.is_null() && params != json!({}) {
            msg["params"] = params;
        }
        if let Some(sid) = session_id {
            msg["sessionId"] = json!(sid);
        }

        let text = serde_json::to_string(&msg)?;
        self.write.send(Message::Text(text)).await?;
        Ok(id)
    }

    async fn send_raw(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.send_raw_no_wait(session_id, method, params).await?;

        loop {
            let resp_text = self.read_text().await?;
            let resp: Value = serde_json::from_str(&resp_text)?;

            // Proactively handle or fail if a dialog is opened during execution
            if resp.get("method").and_then(|v| v.as_str()) == Some("Page.javascriptDialogOpening")
                && method != "Page.handleJavaScriptDialog"
            {
                let dialog_type = resp
                    .get("params")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown");
                let msg = resp
                    .get("params")
                    .and_then(|p| p.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");

                if let Some(action) = &self.dialog_action {
                    let mut handler_params = json!({});
                    match action.as_str() {
                        "accept" => {
                            handler_params["accept"] = json!(true);
                        }
                        "dismiss" => {
                            handler_params["accept"] = json!(false);
                        }
                        text => {
                            handler_params["accept"] = json!(true);
                            handler_params["promptText"] = json!(text);
                        }
                    }
                    // Send the handle command but don't wait for its response here
                    // (we are still waiting for the original 'id')
                    self.send_raw_no_wait(
                        session_id,
                        "Page.handleJavaScriptDialog",
                        handler_params,
                    )
                    .await
                    .map_err(|e| anyhow!("Failed to send dialog handle command: {e}"))?;
                    continue;
                } else {
                    bail!("A javascript dialog is open ({dialog_type}: {msg}). Use `evaluate` with --dialog-action to dismiss it.");
                }
            }

            if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(error) = resp.get("error") {
                    bail!(
                        "CDP error in {method}: {}",
                        serde_json::to_string_pretty(error)?
                    );
                }
                return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
            } else if resp.get("method").is_some() {
                // Store events for later consumption
                self.push_event(resp);
            }
            // Skip other unrelated responses
        }
    }

    /// Read until we get an event with the given method name (for waiting on page load, etc).
    #[allow(dead_code)]
    pub async fn wait_for_event_match<F>(
        &mut self,
        event_methods: &[&str],
        timeout: std::time::Duration,
        mut predicate: F,
    ) -> Result<(String, Value)>
    where
        F: FnMut(&str, &Value) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // First check if we already have any of the events buffered
            if let Some(idx) = self.events.iter().position(|e| {
                if let Some(m) = e.get("method").and_then(|v| v.as_str()) {
                    let params = e.get("params").unwrap_or(&Value::Null);
                    event_methods.contains(&m) && predicate(m, params)
                } else {
                    false
                }
            }) {
                let resp = self.events.remove(idx).unwrap();
                let method = resp.get("method").unwrap().as_str().unwrap().to_string();
                return Ok((method, resp.get("params").cloned().unwrap_or(Value::Null)));
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("Timeout waiting for any event {:?}", event_methods);
            }
            let text = tokio::time::timeout(remaining, self.read_text())
                .await
                .map_err(|_| anyhow!("Timeout waiting for any event {:?}", event_methods))??;
            let resp: Value = serde_json::from_str(&text)?;

            if let Some(m) = resp.get("method").and_then(|v| v.as_str()) {
                let params = resp.get("params").cloned().unwrap_or(Value::Null);
                if event_methods.contains(&m) && predicate(m, &params) {
                    return Ok((m.to_string(), params));
                } else {
                    self.push_event(resp);
                }
            }
        }
    }


    pub async fn read_text(&mut self) -> Result<String> {
        loop {
            match self.read.next().await {
                Some(Ok(Message::Text(text))) => return Ok(text.to_string()),
                Some(Ok(Message::Close(_))) => bail!("WebSocket closed by server"),
                Some(Ok(_)) => continue,
                Some(Err(e)) => bail!("WebSocket error: {e}"),
                None => bail!("WebSocket connection closed"),
            }
        }
    }

    // ── Target domain helpers ──

    pub async fn get_page_targets(&mut self) -> Result<Vec<TargetInfo>> {
        let result = self.send("Target.getTargets", json!({})).await?;
        let targets = result["targetInfos"].as_array().ok_or_else(|| {
            anyhow!("Malformed Target.getTargets response: missing 'targetInfos' array")
        })?;

        let mut pages = Vec::new();
        for t in targets {
            let target_type = t["type"].as_str().unwrap_or("");
            if target_type == "page" {
                let target_id = t["targetId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Malformed TargetInfo: missing 'targetId'"))?
                    .to_string();
                let title = t["title"].as_str().unwrap_or("").to_string();
                let url = t["url"].as_str().unwrap_or("").to_string();

                pages.push(TargetInfo {
                    target_id,
                    title,
                    url,
                    target_type: target_type.to_string(),
                });
            }
        }
        Ok(pages)
    }

    pub async fn attach_to_target(&mut self, target_id: &str) -> Result<String> {
        let result = self
            .send(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;
        result["sessionId"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No sessionId in attachToTarget response"))
    }

    pub async fn detach_from_target(&mut self, session_id: &str) -> Result<()> {
        self.send("Target.detachFromTarget", json!({"sessionId": session_id}))
            .await?;
        Ok(())
    }

    pub async fn activate_target(&mut self, target_id: &str) -> Result<()> {
        self.send("Target.activateTarget", json!({"targetId": target_id}))
            .await?;
        Ok(())
    }

    pub async fn create_target(&mut self, url: &str) -> Result<String> {
        let result = self
            .send("Target.createTarget", json!({"url": url}))
            .await?;
        result["targetId"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No targetId in createTarget response"))
    }

    pub async fn close_target(&mut self, target_id: &str) -> Result<()> {
        self.send("Target.closeTarget", json!({"targetId": target_id}))
            .await?;
        Ok(())
    }

    /// Get the current page URL via JavaScript evaluation.
    pub async fn current_url(&mut self, session_id: &str) -> Result<String> {
        let result = self
            .send_to_target(
                session_id,
                "Runtime.evaluate",
                json!({"expression": "window.location.href", "returnByValue": true}),
            )
            .await?;
        result["result"]["value"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| {
                anyhow!(
                    "Failed to get current URL: evaluation did not return a string. Result: {}",
                    result
                )
            })
    }

    /// Resolve which page to operate on.
    /// Priority: --target (by ID or friendly name) > --page (by index) > first page.
    pub async fn resolve_page(
        &mut self,
        target: Option<&str>,
        page: Option<usize>,
    ) -> Result<TargetInfo> {
        let pages = self.get_page_targets().await?;

        if let Some(tid) = target {
            if friendly::is_friendly(tid) {
                return pages
                    .into_iter()
                    .find(|p| friendly::to_friendly(&p.target_id) == tid)
                    .ok_or_else(|| anyhow!("No page matching '{tid}'"));
            }
            return pages
                .into_iter()
                .find(|p| p.target_id == tid)
                .ok_or_else(|| anyhow!("No page with target ID: {tid}"));
        }

        let idx = page.unwrap_or(0);
        pages
            .into_iter()
            .nth(idx)
            .ok_or_else(|| anyhow!("No page at index {idx}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_event_buffer_capping() {
        let mut events = std::collections::VecDeque::new();

        // Push more than MAX_BUFFERED_EVENTS
        for i in 0..(MAX_BUFFERED_EVENTS + 10) {
            CdpClient::push_to_buffer(&mut events, json!({"method": "test", "params": {"i": i}}));
        }

        assert_eq!(events.len(), MAX_BUFFERED_EVENTS);
        // The first 10 events should have been popped globally
        assert_eq!(events.front().unwrap()["params"]["i"], json!(10));
        assert_eq!(
            events.back().unwrap()["params"]["i"],
            json!(MAX_BUFFERED_EVENTS + 9)
        );
    }

    #[test]
    fn test_parse_target_infos() {
        // Valid response with one page and one background_page
        let valid_resp = json!({
            "targetInfos": [
                {
                    "targetId": "123",
                    "type": "page",
                    "title": "Test Page",
                    "url": "https://example.com"
                },
                {
                    "targetId": "456",
                    "type": "background_page",
                    "title": "BG",
                    "url": "chrome-extension://..."
                }
            ]
        });

        let mut pages = Vec::new();
        let targets = valid_resp["targetInfos"].as_array().unwrap();
        for t in targets {
            if t["type"].as_str() == Some("page") {
                pages.push(TargetInfo {
                    target_id: t["targetId"].as_str().unwrap().to_string(),
                    title: t["title"].as_str().unwrap_or("").to_string(),
                    url: t["url"].as_str().unwrap_or("").to_string(),
                    target_type: "page".to_string(),
                });
            }
        }
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].target_id, "123");

        // Empty response
        let empty_resp = json!({"targetInfos": []});
        let targets = empty_resp["targetInfos"].as_array().unwrap();
        assert!(targets.is_empty());

        // Malformed - missing targetInfos
        let malformed_resp = json!({});
        assert!(malformed_resp["targetInfos"].as_array().is_none());
    }
}

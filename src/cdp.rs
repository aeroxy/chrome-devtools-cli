use crate::friendly;
use anyhow::{anyhow, bail, Result};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct CdpClient {
    write: SplitSink<WsStream, Message>,
    read: SplitStream<WsStream>,
    next_id: u64,
    /// Dialog action to automatically handle JavaScript dialogs during command execution.
    /// Valid values: "accept", "dismiss", or custom prompt text.
    pub dialog_action: Option<String>,
}

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
        })
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
        self.write.send(Message::Text(text.into())).await?;
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
                    self.send_raw_no_wait(session_id, "Page.handleJavaScriptDialog", handler_params)
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
            }
            // Skip events and unrelated responses
        }
    }

    /// Read until we get an event with the given method name (for waiting on page load, etc).
    #[allow(dead_code)]
    pub async fn wait_for_event(
        &mut self,
        event_method: &str,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("Timeout waiting for event {event_method}");
            }
            let text = tokio::time::timeout(remaining, self.read_text())
                .await
                .map_err(|_| anyhow!("Timeout waiting for event {event_method}"))??;
            let resp: Value = serde_json::from_str(&text)?;
            if resp.get("method").and_then(|v| v.as_str()) == Some(event_method) {
                return Ok(resp.get("params").cloned().unwrap_or(Value::Null));
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
        let targets = result["targetInfos"]
            .as_array()
            .ok_or_else(|| anyhow!("Unexpected getTargets response"))?;

        let mut pages = Vec::new();
        for t in targets {
            let target_type = t["type"].as_str().unwrap_or("");
            if target_type == "page" {
                pages.push(TargetInfo {
                    target_id: t["targetId"].as_str().unwrap_or("").to_string(),
                    title: t["title"].as_str().unwrap_or("").to_string(),
                    url: t["url"].as_str().unwrap_or("").to_string(),
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
                // Resolve friendly name → target ID
                return pages
                    .into_iter()
                    .find(|p| friendly::to_friendly(&p.target_id) == tid)
                    .ok_or_else(|| anyhow!("No page matching '{tid}'"));
            }
            // Raw target ID
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

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
    /// Persistent CDP session for continuous event collection across commands.
    pub persistent_session: Option<String>,
    /// Target ID the persistent session is attached to.
    pub persistent_target_id: Option<String>,
    /// Accumulated network events from the persistent session.
    /// VecDeque so the size cap is O(1) (pop_front) instead of O(N) (Vec front drain).
    pub network_events: std::collections::VecDeque<Value>,
    /// Accumulated console events from the persistent session.
    pub console_events: std::collections::VecDeque<Value>,
    /// Persistent URL block patterns for `Network.setBlockedURLs`.
    /// Re-applied by `ensure_persistent_session` whenever a new target is attached.
    pub blocklist: Vec<String>,
    /// Active viewport override, re-applied on target switch (like `blocklist`).
    pub viewport: Option<ViewportOverride>,
    /// Active geolocation override, re-applied on target switch.
    pub geolocation: Option<GeolocationOverride>,
    /// Per-tab emulation state for tabs that are NOT currently active. The active
    /// tab's state lives in the `blocklist`/`viewport`/`geolocation` fields above;
    /// `ensure_persistent_session` swaps state in/out of here on target switch so
    /// each tab keeps its own overrides (per-tab isolation).
    pub emulation_saved: std::collections::HashMap<String, TabEmulation>,
}

const MAX_BUFFERED_EVENTS: usize = 1000;
const MAX_PERSISTENT_EVENTS: usize = 5000;

/// Metadata for a Chrome target (page, worker, etc.).
#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub target_id: String,
    pub title: String,
    pub url: String,
    #[allow(dead_code)]
    pub target_type: String,
}

/// Active viewport override. Stored structured (not as a display string) so it
/// can be re-applied to a new session on target switch, like the blocklist.
#[derive(Debug, Clone)]
pub struct ViewportOverride {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

/// Active geolocation override. Stored structured for the same reason.
#[derive(Debug, Clone)]
pub struct GeolocationOverride {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
}

/// A tab's emulation state, saved while another tab is active so each tab keeps
/// its own viewport/geolocation/blocklist (per-tab isolation).
#[derive(Debug, Clone, Default)]
pub struct TabEmulation {
    pub blocklist: Vec<String>,
    pub viewport: Option<ViewportOverride>,
    pub geolocation: Option<GeolocationOverride>,
}

impl TabEmulation {
    /// True when the tab has no overrides of any kind.
    fn is_empty(&self) -> bool {
        self.blocklist.is_empty() && self.viewport.is_none() && self.geolocation.is_none()
    }
}

/// Stash an outgoing tab's emulation state under its target id so it can be
/// restored when that tab becomes active again. Empty state is dropped so the
/// map doesn't accumulate blank entries for every tab ever visited. Pure (no
/// I/O) — this is the per-tab isolation invariant and is unit-tested below.
fn stash_tab_emulation(
    saved: &mut std::collections::HashMap<String, TabEmulation>,
    target: String,
    state: TabEmulation,
) {
    if !state.is_empty() {
        saved.insert(target, state);
    }
}

impl CdpClient {
    /// Connect to Chrome via WebSocket and return a CDP client.
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
            persistent_session: None,
            persistent_target_id: None,
            network_events: std::collections::VecDeque::new(),
            console_events: std::collections::VecDeque::new(),
            blocklist: Vec::new(),
            viewport: None,
            geolocation: None,
            emulation_saved: std::collections::HashMap::new(),
        })
    }

    /// Clear all buffered events.
    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    /// Ensure a persistent CDP session is attached to the given target for continuous
    /// event collection (Network + Console). If the target has changed, detaches
    /// the old session and attaches to the new one.
    pub async fn ensure_persistent_session(&mut self, target_id: &str) -> Result<()> {
        // Already attached to this target
        if self.persistent_target_id.as_deref() == Some(target_id)
            && self.persistent_session.is_some()
        {
            return Ok(());
        }

        // Target is changing — discard events accumulated under the previous
        // target so a later drain under the new page can't return stale events.
        self.network_events.clear();
        self.console_events.clear();

        // Save the previously-active tab's emulation state so it can be restored
        // when we return to it (per-tab isolation). The active state lives in the
        // top-level fields; stash it under the old target id — but only if the
        // tab actually has overrides, so the map doesn't accumulate empty entries
        // for every tab ever visited.
        if let Some(old_target) = self.persistent_target_id.clone() {
            let saved = TabEmulation {
                blocklist: std::mem::take(&mut self.blocklist),
                viewport: self.viewport.take(),
                geolocation: self.geolocation.take(),
            };
            stash_tab_emulation(&mut self.emulation_saved, old_target, saved);
        }

        // Detach old session if target changed
        if let Some(old_session) = self.persistent_session.take() {
            let _ = self
                .send("Target.detachFromTarget", json!({"sessionId": old_session}))
                .await;
            self.persistent_target_id = None;
            self.events.retain(|event| {
                event["sessionId"]
                    .as_str()
                    .map(|s| s != old_session)
                    .unwrap_or(true)
            });
        }

        // Attach new persistent session
        let result = self
            .send(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;

        let session_id = result["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("No sessionId in persistent attachToTarget"))?
            .to_string();

        // Store the session id BEFORE enabling domains so events arriving
        // during Network.enable / Runtime.enable are routed to persistent
        // buffers instead of the generic events buffer.
        self.persistent_session = Some(session_id.clone());
        self.persistent_target_id = Some(target_id.to_string());

        // Enable Network and Runtime domains on the persistent session. If
        // either fails, clear the just-set session, detach it (so it isn't
        // leaked in Chrome), and bail — per-command collectors then fall
        // back to their own enable instead of silently draining an
        // uninitialized (always-empty) buffer.
        for method in ["Network.enable", "Runtime.enable"] {
            if let Err(e) = self.send_to_target(&session_id, method, json!({})).await {
                self.cleanup_failed_session(&session_id, target_id, false).await;
                return Err(e);
            }
        }

        // Load the new tab's saved emulation state into the active fields (empty
        // if this tab has none), then apply it to the new session. A tab with no
        // saved state starts clean — overrides from other tabs don't leak in.
        let restored = self.emulation_saved.remove(target_id).unwrap_or_default();
        self.blocklist = restored.blocklist;
        self.viewport = restored.viewport;
        self.geolocation = restored.geolocation;

        if let Err(e) = self.apply_network_rules_internal(&session_id).await {
            self.cleanup_failed_session(&session_id, target_id, true).await;
            return Err(e);
        }

        if let Err(e) = self.apply_emulation_internal(&session_id).await {
            self.cleanup_failed_session(&session_id, target_id, true).await;
            return Err(e);
        }

        Ok(())
    }

    /// Clean up a failed persistent session: detach from Chrome, stash the new
    /// tab's state back into the map, and transition to a clean detached state.
    async fn cleanup_failed_session(
        &mut self,
        session_id: &str,
        failed_target_id: &str,
        state_loaded: bool,
    ) {
        if state_loaded {
            // Stash the state of the tab we failed to initialize so it's not lost.
            // It's currently in the active fields.
            let failed_state = TabEmulation {
                blocklist: std::mem::take(&mut self.blocklist),
                viewport: self.viewport.take(),
                geolocation: self.geolocation.take(),
            };
            stash_tab_emulation(
                &mut self.emulation_saved,
                failed_target_id.to_string(),
                failed_state,
            );
        }

        let _ = self
            .send("Target.detachFromTarget", json!({"sessionId": session_id}))
            .await;

        self.persistent_session = None;
        self.persistent_target_id = None;

        // Clear events captured from the failed session during the detach call
        // (which can read from the websocket and thus trigger push_event).
        self.network_events.clear();
        self.console_events.clear();
        self.events.retain(|event| {
            event["sessionId"]
                .as_str()
                .map(|s| s != session_id)
                .unwrap_or(true)
        });
    }

    /// Drop any stored emulation state for a target (e.g. when its tab closes).
    /// If it's the active tab, also reset the active fields and the now-invalid
    /// persistent session so the next command attaches cleanly.
    pub fn forget_target(&mut self, target_id: &str) {
        self.emulation_saved.remove(target_id);
        if self.persistent_target_id.as_deref() == Some(target_id) {
            self.blocklist.clear();
            self.viewport = None;
            self.geolocation = None;
            self.persistent_session = None;
            self.persistent_target_id = None;
        }
    }

    /// Re-apply the current viewport/geolocation overrides to a session.
    /// Mirrors `apply_network_rules_internal` so overrides persist across
    /// target switches. No-op for whichever override isn't set.
    pub(crate) async fn apply_emulation_internal(&mut self, session_id: &str) -> Result<()> {
        if let Some(vp) = self.viewport.clone() {
            self.send_to_target(
                session_id,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": vp.width,
                    "height": vp.height,
                    "deviceScaleFactor": vp.device_scale_factor,
                    "mobile": vp.mobile,
                }),
            )
            .await?;
        }
        if let Some(geo) = self.geolocation.clone() {
            self.send_to_target(
                session_id,
                "Emulation.setGeolocationOverride",
                json!({
                    "latitude": geo.latitude,
                    "longitude": geo.longitude,
                    "accuracy": geo.accuracy,
                }),
            )
            .await?;
        }
        Ok(())
    }

    /// Apply the daemon's current `blocklist` to the persistent session via
    /// `Network.setBlockedURLs`. Patterns are Chrome-style globs (e.g. `*.png`).
    /// Falls back silently if no persistent session is active (e.g. during
    /// daemon startup or after a target switch that hasn't finished attaching).
    pub async fn apply_network_rules(&mut self) -> Result<()> {
        if let Some(ref session) = self.persistent_session.clone() {
            self.apply_network_rules_internal(session).await?;
        }
        Ok(())
    }

    /// Apply the current `blocklist` to a specific session via
    /// `Network.setBlockedURLs`. Used for both the persistent session and
    /// fallback per-command sessions (when the persistent one is unavailable).
    pub(crate) async fn apply_network_rules_internal(&mut self, session_id: &str) -> Result<()> {
        self.send_to_target(session_id, "Network.enable", json!({}))
            .await?;
        self.send_to_target(
            session_id,
            "Network.setBlockedURLs",
            json!({"urls": self.blocklist}),
        )
        .await?;
        Ok(())
    }

    /// Drain accumulated network events from the persistent session.
    pub fn drain_network_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.network_events).into()
    }

    /// Drain accumulated console events from the persistent session.
    pub fn drain_console_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.console_events).into()
    }

    fn push_event(&mut self, event: Value) {
        // Only route events into the persistent buffers when the event's
        // sessionId matches the persistent page session (flatten-mode events
        // are tagged with sessionId). Events from ad-hoc sessions (sw-logs
        // attach, per-command live sessions) fall through and go into the
        // general events buffer — otherwise a subsequent page `console` drain
        // would return extension service worker logs.
        let from_persistent_session = self.persistent_session.as_deref().is_some_and(|s| {
            event["sessionId"]
                .as_str()
                .is_some_and(|session_id| session_id == s)
        });

        if from_persistent_session {
            let method = event.get("method").and_then(|v| v.as_str()).unwrap_or("");
            match method {
                "Network.requestWillBeSent"
                | "Network.responseReceived"
                | "Network.loadingFinished"
                | "Network.loadingFailed" => {
                    self.network_events.push_back(event);
                    if self.network_events.len() > MAX_PERSISTENT_EVENTS {
                        self.network_events.pop_front();
                    }
                    return;
                }
                "Runtime.consoleAPICalled" | "Runtime.exceptionThrown" => {
                    self.console_events.push_back(event);
                    if self.console_events.len() > MAX_PERSISTENT_EVENTS {
                        self.console_events.pop_front();
                    }
                    return;
                }
                _ => {}
            }
        }
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

                if let Some(action) = self.dialog_action.clone() {
                    self.handle_dialog_helper(session_id, action).await?;
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

    fn handle_dialog_helper<'a>(
        &'a mut self,
        session_id: Option<&'a str>,
        action: String,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
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

            let id = self
                .send_raw_no_wait(session_id, "Page.handleJavaScriptDialog", handler_params)
                .await?;

            loop {
                let resp_text = self.read_text().await?;
                let resp: Value = serde_json::from_str(&resp_text)?;

                if resp.get("method").and_then(|v| v.as_str())
                    == Some("Page.javascriptDialogOpening")
                {
                    self.handle_dialog_helper(session_id, action.clone())
                        .await?;
                    continue;
                }

                if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    if let Some(error) = resp.get("error") {
                        bail!(
                            "CDP error in Page.handleJavaScriptDialog: {}",
                            serde_json::to_string_pretty(error)?
                        );
                    }
                    return Ok(());
                } else if resp.get("method").is_some() {
                    self.push_event(resp);
                }
            }
        })
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
                if let Some(resp) = self.events.remove(idx) {
                    let method = resp
                        .get("method")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    return Ok((method, resp.get("params").cloned().unwrap_or(Value::Null)));
                }
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

    /// Read the next text message from the WebSocket, skipping non-text frames.
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

    /// List all open page targets.
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

    /// List all targets (pages, workers, etc.).
    pub async fn get_all_targets(&mut self) -> Result<Vec<TargetInfo>> {
        let result = self.send("Target.getTargets", json!({})).await?;
        let targets = result["targetInfos"].as_array().ok_or_else(|| {
            anyhow!("Malformed Target.getTargets response: missing 'targetInfos' array")
        })?;

        let mut all = Vec::new();
        for t in targets {
            all.push(TargetInfo {
                target_id: t["targetId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Malformed TargetInfo: missing 'targetId'"))?
                    .to_string(),
                title: t["title"].as_str().unwrap_or("").to_string(),
                url: t["url"].as_str().unwrap_or("").to_string(),
                target_type: t["type"].as_str().unwrap_or("").to_string(),
            });
        }
        Ok(all)
    }

    /// Attach to a target and return the session ID for subsequent commands.
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

    /// Detach from a target session.
    pub async fn detach_from_target(&mut self, session_id: &str) -> Result<()> {
        self.send("Target.detachFromTarget", json!({"sessionId": session_id}))
            .await?;
        Ok(())
    }

    /// Bring a target to the foreground.
    pub async fn activate_target(&mut self, target_id: &str) -> Result<()> {
        self.send("Target.activateTarget", json!({"targetId": target_id}))
            .await?;
        Ok(())
    }

    /// Create a new page target and return its target ID.
    pub async fn create_target(&mut self, url: &str) -> Result<String> {
        let result = self
            .send("Target.createTarget", json!({"url": url}))
            .await?;
        result["targetId"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No targetId in createTarget response"))
    }

    /// Close a target by its target ID.
    pub async fn close_target(&mut self, target_id: &str) -> Result<()> {
        self.send("Target.closeTarget", json!({"targetId": target_id}))
            .await?;
        Ok(())
    }

    /// Collect CDP events for a given duration (milliseconds).
    ///
    /// Drains any already-buffered events first, then reads from the WebSocket
    /// until the timeout expires. Events are also routed through persistent
    /// session buffers when active to avoid duplicates.
    pub async fn read_events_for(&mut self, duration_ms: u64) -> Result<Vec<Value>> {
        let mut events: Vec<Value> = self.events.drain(..).collect();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(duration_ms);

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, self.read_text()).await {
                Ok(Ok(text)) => {
                    if let Ok(resp) = serde_json::from_str::<Value>(&text) {
                        if let Some(method) = resp.get("method").and_then(|v| v.as_str()) {
                            // Events are routed to persistent session buffers (network_events,
                            // console_events) AND returned in the `events` vector here.
                            //
                            // In direct mode (no persistent session), we avoid pushing
                            // Network/Runtime events to the generic `self.events` buffer.
                            // This prevents them from being stashed and then returned
                            // again in a subsequent drain (double-processing).
                            let is_persistent_event = self.persistent_session.as_deref().is_some_and(|s| {
                                resp["sessionId"]
                                    .as_str()
                                    .is_some_and(|session_id| session_id == s)
                            });
                            if is_persistent_event {
                                self.push_event(resp.clone());
                            } else if !method.starts_with("Network.") && !method.starts_with("Runtime.") {
                                self.push_event(resp.clone());
                            }
                            events.push(resp);
                        }
                    }
                }
                // Timeout expired — normal completion path.
                Err(_) => break,
                // read_text() itself failed (socket closed, decode error,
                // etc.) — real transport error. Log it so the user has a
                // signal when the partial result is suspicious, then stop.
                Ok(Err(e)) => {
                    eprintln!("Warning: WebSocket read failed during event collection: {e}");
                    break;
                }
            }
        }

        Ok(events)
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

/// Helper to join console API arguments into a single string.
/// Chrome's `Runtime.consoleAPICalled` returns arguments as an array of RemoteObjects.
pub fn join_console_args(args: &[Value]) -> String {
    args.iter()
        .map(|arg| match arg.get("value") {
            // String primitives: emit the raw text (no quotes).
            Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
            // Other primitives (number, bool, null): stringify directly.
            Some(v) => v.to_string(),
            // Objects have no `value` — fall back to their description.
            None => arg["description"]
                .as_str()
                .unwrap_or("<object>")
                .to_string(),
        })
        .collect::<Vec<String>>()
        .join(" ")
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

    // Per-tab emulation isolation: the save/restore that `ensure_persistent_session`
    // performs on target switch. These exercise the pure state logic directly,
    // without a live CDP connection. They guard the invariant that one tab's
    // blocklist/overrides never leak into another — the property a side-by-side
    // two-tab comparison workflow depends on.

    #[test]
    fn switching_to_fresh_tab_does_not_inherit_blocklist() {
        let mut saved = std::collections::HashMap::new();
        // Leaving tab A, which has a block rule: stash it.
        stash_tab_emulation(
            &mut saved,
            "A".to_string(),
            TabEmulation {
                blocklist: vec!["*.png".to_string()],
                ..Default::default()
            },
        );

        // Arriving at fresh tab B (no saved state) → clean slate, as
        // ensure_persistent_session does via `remove(target).unwrap_or_default()`.
        let b_state = saved.remove("B").unwrap_or_default();
        assert!(
            b_state.blocklist.is_empty(),
            "tab B must not inherit tab A's block rule"
        );
        // A's rule is preserved for when we switch back.
        assert_eq!(saved.get("A").unwrap().blocklist, vec!["*.png".to_string()]);
    }

    #[test]
    fn returning_to_tab_restores_and_consumes_its_state() {
        let mut saved = std::collections::HashMap::new();
        stash_tab_emulation(
            &mut saved,
            "A".to_string(),
            TabEmulation {
                blocklist: vec!["*.png".to_string()],
                ..Default::default()
            },
        );

        let restored = saved.remove("A").unwrap_or_default();
        assert_eq!(restored.blocklist, vec!["*.png".to_string()]);
        assert!(
            saved.is_empty(),
            "the restored entry should be consumed, not duplicated"
        );
    }

    #[test]
    fn empty_tab_state_is_not_stashed() {
        let mut saved = std::collections::HashMap::new();
        stash_tab_emulation(&mut saved, "A".to_string(), TabEmulation::default());
        assert!(
            !saved.contains_key("A"),
            "an empty tab must not create a map entry"
        );
    }
}

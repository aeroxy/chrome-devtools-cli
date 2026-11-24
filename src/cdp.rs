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
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
    MaybeTlsStream, WebSocketStream,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Largest message read from the browser. Chrome sends each CDP message as a
/// single frame, so this is the frame limit too. tungstenite's default of
/// 16 MiB per frame is too small: a page can exceed it with one long console
/// message or POST body, and after a failed read the connection is unusable.
/// The daemon then exits, and the next command needs the user to approve a new
/// connection. Playwright allows 256 MiB on the same connection.
const MAX_MESSAGE_SIZE: usize = 256 << 20;

/// Chrome's WebSocket is gone. A type rather than a message prefix, because
/// the daemon exits on it and error text can come from the page: an
/// `evaluate` exception can say "WebSocket closed" too.
#[derive(Debug)]
pub struct ConnectionLost(pub(crate) String);

impl std::fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConnectionLost {}

/// A JavaScript dialog (alert, confirm, prompt, beforeunload) is open on the
/// page. A type rather than a message, so input commands can tell "my click
/// opened a dialog" apart from a real failure.
#[derive(Debug, Clone)]
pub struct DialogOpen {
    pub dialog_type: String,
    pub message: String,
}

impl std::fmt::Display for DialogOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A javascript dialog is open ({}: {}). The page is paused until it is handled: run \
             `evaluate --dialog-action accept|dismiss|<prompt text> true` on this page.",
            self.dialog_type, self.message
        )
    }
}

impl std::error::Error for DialogOpen {}

/// A failure applying the daemon's blocklist, tagged with whether the
/// `Network.setBlockedURLs` write may have reached Chrome. A failure during
/// `Network.enable` happens before the rule write (`write_attempted == false`),
/// so the stored list can be restored; once the write was attempted its outcome
/// is unknown and the new list must be kept.
#[derive(Debug)]
pub(crate) struct ApplyRulesError {
    pub(crate) error: anyhow::Error,
    pub(crate) write_attempted: bool,
}

/// The browser answers but the page doesn't. Usually a JavaScript dialog
/// opened where this client couldn't see it (before it attached, or under a
/// session it has since left): Chrome tells only the sessions that were
/// listening, only they can handle it, and until then the page answers no
/// session at all. Otherwise an endless loop or a hung renderer. A type so
/// the executor stops instead of trying the page again on another session.
#[derive(Debug)]
pub struct PageUnresponsive(pub String);

impl std::fmt::Display for PageUnresponsive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PageUnresponsive {}

/// A dialog this client saw open and has not seen closed. Chrome sends the
/// opening event once, to the sessions that had `Page` enabled, and only those
/// sessions can handle it; while it is open the page answers nothing else.
#[derive(Debug, Clone)]
pub struct OpenDialog {
    pub session_id: Option<String>,
    pub dialog: DialogOpen,
}

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
        background: bool,
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
    /// The dialog seen open and not yet seen closed, so the next command on
    /// that page can handle it or fail at once instead of waiting on a page
    /// that will not answer.
    pub open_dialog: Option<OpenDialog>,
    /// Open dialogs per session, in the order Chrome announced them.
    /// `open_dialog` mirrors the newest one for callers that read the public
    /// field directly; opening or closing one session's dialog must not lose
    /// another's.
    open_dialogs: Vec<OpenDialog>,
    /// Dialogs left open on a tab whose session detached. A detach does not
    /// close a JavaScript dialog, but only the session that saw it can settle
    /// it, so the record follows the tab until a later session on it can
    /// report or handle the dialog.
    orphan_dialogs: std::collections::HashMap<String, DialogOpen>,
    /// Ids of calls that are waiting for a reply. A reply to any other id is
    /// fire-and-forget and dropped, not parked in `deferred_replies` forever.
    awaiting_replies: std::collections::HashSet<u64>,
    /// Set once an input command has sent its action to the page. A dialog
    /// that interrupts the command after that is the action's doing; before
    /// it, the action never ran.
    pub input_sent: bool,
    /// Buffer for storing unhandled events (e.g., navigation events)
    pub events: std::collections::VecDeque<Value>,
    /// Messages already taken off the socket that `read_text` must return
    /// before reading more, oldest first. [`CdpClient::ping`] and queued
    /// dialog transitions fill it.
    unread: std::collections::VecDeque<String>,
    /// Replies to other in-flight calls set aside while a dialog was handled.
    /// Keyed by request id: pushing them to `unread` alone would make the
    /// dialog handler read its own deferred reply again on every iteration.
    deferred_replies: std::collections::VecDeque<(u64, String)>,
    /// Persistent CDP session for continuous event collection across commands.
    pub persistent_session: Option<String>,
    /// Target ID the persistent session is attached to.
    pub persistent_target_id: Option<String>,
    /// Accumulated network events from the persistent session.
    /// VecDeque so the size cap is O(1) (pop_front) instead of O(N) (Vec front drain).
    pub network_events: std::collections::VecDeque<Value>,
    /// Accumulated console events from the persistent session.
    pub console_events: std::collections::VecDeque<Value>,
    /// Console events ever buffered. Unlike the buffer's length it never
    /// drops, so a command can tell which events arrived while it ran.
    pub console_pushed: u64,
    /// Persistent URL block patterns for `Network.setBlockedURLs`.
    /// Re-applied by `ensure_persistent_session` whenever a new target is attached.
    pub blocklist: Vec<String>,
    /// Active viewport override, re-applied on target switch (like `blocklist`).
    pub viewport: Option<ViewportOverride>,
    /// Active geolocation override, re-applied on target switch.
    pub geolocation: Option<GeolocationOverride>,
    /// Active CSS media emulation, re-applied on target switch.
    pub media: Option<MediaOverride>,
    /// Virtual WebAuthn authenticator on the active tab, re-created on target
    /// switch because Chrome removes it when the session that added it detaches.
    pub webauthn: Option<VirtualAuthenticator>,
    /// Network mocks on the active tab (`mock-network`), saved per tab like
    /// the emulation state.
    pub network_mocks: Vec<crate::commands::network_mock::NetworkMock>,
    /// Chrome may still be intercepting requests even when the mock list is
    /// empty: a failed rollback can leave `Fetch.enable` in place. Keep
    /// reading between commands in that case so paused requests get answered.
    pub fetch_may_be_enabled: bool,
    /// Sessions whose detach failed and that may still be intercepting
    /// requests. Tracked per session so ending a later tab cannot clear the
    /// interception state of one that is still live. Idle reads stay on while
    /// any such session remains.
    sessions_may_intercept: std::collections::HashSet<String>,
    /// Sessions whose `Fetch.enable` was actually written. A setup that fails
    /// after that write but before the detach is confirmed leaves Chrome able
    /// to pause requests, so the session must keep idle reads on. The shared
    /// `fetch_may_be_enabled` cannot decide this: a different session may have
    /// set it. A session that failed before the write needs no such record.
    fetch_enable_sent_sessions: std::collections::HashSet<String>,
    /// Target ID of each session left intercepting by a failed detach. A late
    /// pause on that session always resolves its mocks through its tab's live
    /// or saved list — never a stale copy — so mock additions and removals on
    /// the tab apply immediately and hit counts stay in one place.
    retained_session_targets: std::collections::HashMap<String, String>,
    /// `Fetch.requestPaused` events not yet answered. The page's request
    /// waits until they are, so every read answers them first.
    paused_requests: std::collections::VecDeque<Value>,
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

/// Active CSS media emulation (`Emulation.setEmulatedMedia`). Chrome replaces
/// every emulated feature on each call, so the whole set is kept to resend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaOverride {
    /// Emulated media type (`print` or `screen`); `None` leaves it alone.
    pub media: Option<String>,
    /// Emulated media features as (name, value), in the order they were set.
    pub features: Vec<(String, String)>,
}

impl MediaOverride {
    /// Parameters for `Emulation.setEmulatedMedia`. An empty media string and
    /// an empty feature list are what turn the override off.
    pub fn cdp_params(&self) -> Value {
        let features: Vec<Value> = self
            .features
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect();
        json!({"media": self.media.as_deref().unwrap_or(""), "features": features})
    }
}

/// Options of a virtual WebAuthn authenticator, as `configure-webauthn` sets them.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthenticatorOptions {
    pub protocol: String,
    pub transport: String,
    pub has_resident_key: bool,
    pub has_user_verification: bool,
    pub is_user_verified: bool,
}

impl AuthenticatorOptions {
    /// The `options` for `WebAuthn.addVirtualAuthenticator`. Presence is
    /// simulated so a `navigator.credentials` call never waits for a touch.
    pub fn cdp_options(&self) -> Value {
        json!({
            "protocol": self.protocol,
            "transport": self.transport,
            "hasResidentKey": self.has_resident_key,
            "hasUserVerification": self.has_user_verification,
            "isUserVerified": self.is_user_verified,
            "automaticPresenceSimulation": true,
        })
    }
}

/// A virtual authenticator the CLI added to a tab. `id` belongs to the session
/// that added it and changes whenever the authenticator is re-created.
#[derive(Debug, Clone)]
pub struct VirtualAuthenticator {
    pub id: String,
    pub options: AuthenticatorOptions,
}

/// A tab's emulation state, saved while another tab is active so each tab keeps
/// its own viewport/geolocation/blocklist (per-tab isolation).
#[derive(Debug, Clone, Default)]
pub struct TabEmulation {
    pub blocklist: Vec<String>,
    pub viewport: Option<ViewportOverride>,
    pub geolocation: Option<GeolocationOverride>,
    pub media: Option<MediaOverride>,
    pub webauthn: Option<VirtualAuthenticator>,
    pub network_mocks: Vec<crate::commands::network_mock::NetworkMock>,
    /// Session whose Fetch interception these mocks belong to. A failed detach
    /// leaves it interrupting even after the tab is switched away, so a paused
    /// request from it must be answered from this list, not the active tab's.
    pub session_id: Option<String>,
}

impl TabEmulation {
    /// True when the tab has no overrides of any kind.
    fn is_empty(&self) -> bool {
        self.blocklist.is_empty()
            && self.viewport.is_none()
            && self.geolocation.is_none()
            && self.media.is_none()
            && self.webauthn.is_none()
            && self.network_mocks.is_empty()
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

/// `Target.createTarget` parameters. `background` is only sent when set, so a
/// foreground request stays the one non-Chrome CDP servers (Kitesurf) already
/// accept.
fn create_target_params(url: &str, background: bool) -> Value {
    let mut params = json!({"url": url});
    if background {
        params["background"] = json!(true);
    }
    params
}

/// Read the Chrome WebSocket connect timeout from `CHROME_CONNECT_TIMEOUT_SECS`,
/// defaulting to 10. Kept pure (value passed in) so the parsing policy is
/// unit-testable without mutating process-global env vars.
fn parse_connect_timeout(raw: Option<&str>) -> std::time::Duration {
    parse_timeout_secs("CHROME_CONNECT_TIMEOUT_SECS", raw, 10)
}

/// How long one CDP call may go unanswered while the browser itself still
/// answers, from `CHROME_CDP_TIMEOUT_SECS` (default 180, Puppeteer's protocol
/// timeout). Long enough for a slow `run-script`; short enough that an endless
/// loop or a hung renderer can't hold the daemon, which serves one request at
/// a time, and every caller queued behind it, forever.
/// Read once, so a bad value warns once rather than on every call.
pub(crate) fn cdp_call_timeout() -> std::time::Duration {
    static LIMIT: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        let raw = std::env::var("CHROME_CDP_TIMEOUT_SECS").ok();
        parse_timeout_secs("CHROME_CDP_TIMEOUT_SECS", raw.as_deref(), 180)
    })
}

/// Limit for calls that set a session up (`Runtime.enable` and the like). They
/// answer in milliseconds unless the page is blocked, and a blocked page would
/// otherwise cost the full call timeout for each of them in turn.
pub const SETUP_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Silence this long during a call prompts a check that the browser still
/// answers: a laptop that slept or a dropped network can leave the socket
/// open with nobody on the other end.
const QUIET_BEFORE_CHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// How long the browser gets to answer that check. Its browser process answers
/// even while a page is stuck, so no answer means the connection is gone.
const ALIVE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn parse_timeout_secs(var: &str, raw: Option<&str>, default_secs: u64) -> std::time::Duration {
    let default = std::time::Duration::from_secs(default_secs);
    match raw {
        None => default,
        Some(v) => match v.parse::<u64>() {
            // 0 would make every connect fail instantly with the misleading
            // "Chrome may be waiting for approval" message; treat it as
            // invalid rather than honoring it.
            Ok(secs) if secs > 0 => std::time::Duration::from_secs(secs),
            _ => {
                eprintln!(
                    "[warning] Ignoring invalid {var} value '{v}' \
                     (expected a positive integer); using default of {}s",
                    default.as_secs()
                );
                default
            }
        },
    }
}

fn connect_timeout() -> std::time::Duration {
    let raw = std::env::var("CHROME_CONNECT_TIMEOUT_SECS").ok();
    parse_connect_timeout(raw.as_deref())
}

/// `ws_url` as diagnostics show it. A hosted CDP service may put an API key in
/// the query string (`?token=`, `?apiKey=`) or the userinfo (`user:key@`), and
/// errors end up in logs and agent transcripts. The path stays, so a local
/// browser's `/devtools/browser/<id>` is still there to debug with.
fn redact_endpoint(ws_url: &str) -> String {
    let (scheme, rest) = match ws_url.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), ws_url),
    };
    let (before_query, query) = rest.split_at(rest.find(['?', '#']).unwrap_or(rest.len()));
    let (authority, path) =
        before_query.split_at(before_query.find('/').unwrap_or(before_query.len()));
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("***@{host}"),
        None => authority.to_string(),
    };
    let query = if query.is_empty() { "" } else { "?***" };
    format!("{scheme}{authority}{path}{query}")
}

impl CdpClient {
    /// Connect to the browser via WebSocket and return a CDP client.
    ///
    /// Bounded by a timeout: without it, a pending remote-debugging consent
    /// dialog leaves the WebSocket handshake hanging indefinitely, which (via
    /// the daemon) makes every command appear to hang forever with no
    /// diagnostic.
    ///
    /// `browser` is the human-readable name for the diagnostics only — these
    /// messages are the ones a user actually reads, and telling an Edge user to
    /// go and check Chrome sends them to the wrong window.
    pub async fn connect(ws_url: &str, browser: &str) -> Result<Self> {
        let timeout_dur = connect_timeout();
        let config = WebSocketConfig {
            max_message_size: Some(MAX_MESSAGE_SIZE),
            max_frame_size: Some(MAX_MESSAGE_SIZE),
            ..WebSocketConfig::default()
        };
        let connecting = connect_async_with_config(ws_url, Some(config), false);
        let shown_url = redact_endpoint(ws_url);
        let (ws, _) = tokio::time::timeout(timeout_dur, connecting)
            .await
            .map_err(|_| {
                anyhow!(
                    "Timed out after {}s connecting to {browser} at {shown_url}. {browser} may be \
                     waiting for a human to approve the remote-debugging connection dialog. \
                     If you are an automated agent: do not retry in a loop and do not run \
                     kill-daemon (it will not fix this and will require a fresh approval) — \
                     stop and ask the human to check {browser}, then retry once.",
                    timeout_dur.as_secs()
                )
            })?
            .map_err(|e| anyhow!("Failed to connect to {browser} at {shown_url}: {e}"))?;
        let (write, read) = ws.split();
        Ok(Self {
            write,
            read,
            next_id: 1,
            dialog_action: None,
            open_dialog: None,
            open_dialogs: Vec::new(),
            orphan_dialogs: std::collections::HashMap::new(),
            awaiting_replies: std::collections::HashSet::new(),
            input_sent: false,
            events: std::collections::VecDeque::new(),
            unread: std::collections::VecDeque::new(),
            deferred_replies: std::collections::VecDeque::new(),
            persistent_session: None,
            persistent_target_id: None,
            network_events: std::collections::VecDeque::new(),
            console_events: std::collections::VecDeque::new(),
            console_pushed: 0,
            blocklist: Vec::new(),
            viewport: None,
            geolocation: None,
            media: None,
            webauthn: None,
            network_mocks: Vec::new(),
            fetch_may_be_enabled: false,
            sessions_may_intercept: std::collections::HashSet::new(),
            fetch_enable_sent_sessions: std::collections::HashSet::new(),
            retained_session_targets: std::collections::HashMap::new(),
            paused_requests: std::collections::VecDeque::new(),
            emulation_saved: std::collections::HashMap::new(),
        })
    }

    /// Clear all buffered events.
    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    /// Fail a setup step. A lost connection is already unusable: writing a
    /// detach to it would spend the ordinary call budget while the daemon waits,
    /// and the error must reach the daemon unchanged so it retires the socket.
    /// Ordinary CDP refusals still get the session cleaned up.
    async fn fail_setup(
        &mut self,
        session_id: &str,
        target_id: &str,
        state_loaded: bool,
        error: anyhow::Error,
    ) -> anyhow::Error {
        if error.downcast_ref::<ConnectionLost>().is_some() {
            return error;
        }
        let cleanup = self
            .cleanup_failed_session(session_id, target_id, state_loaded)
            .await;
        cleanup.err().unwrap_or(error)
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

        let old_target = self.persistent_target_id.clone();

        // Detach old session if target changed. Keep the id outside the block
        // so the outgoing tab's state can be saved with it below.
        let old_session = self.persistent_session.take();
        if let Some(ref old_session) = old_session {
            // A detach does not close an open dialog, and only the session
            // that saw it can settle it. Keep the dialog on that session while
            // the detach is in flight: a close arriving meanwhile is applied
            // to the session, and a dialog opening during the wait is recorded
            // too. Save for the tab only if a dialog is open afterwards.
            let dialog_target = old_target.clone();
            match self
                .send("Target.detachFromTarget", json!({"sessionId": old_session}))
                .await
            {
                Ok(_) => {
                    // The old tab's Fetch interception died with its session;
                    // otherwise a later tab inherits idle reads it doesn't need.
                    self.session_ended(old_session);
                }
                Err(e) if e.downcast_ref::<ConnectionLost>().is_some() => {
                    // The socket is gone: writing the detach left it unusable,
                    // and the daemon retires it on this typed error. Attaching
                    // on the same socket would spend another full write budget
                    // before the failure reaches anyone.
                    return Err(e);
                }
                Err(_) => {
                    // A failed detach can leave the old tab's Fetch interception
                    // and paused requests alive. Remember it and its target so
                    // later tabs keep idle reads on and resolve its mocks from
                    // its tab's live/saved list. Answer its paused requests now
                    // before switching the active tab's state.
                    self.sessions_may_intercept.insert(old_session.clone());
                    if let Some(ref target) = old_target {
                        self.retained_session_targets
                            .insert(old_session.clone(), target.clone());
                    }
                    self.answer_paused_requests().await?;
                }
            }
            if let (Some(target), Some(open)) = (dialog_target, self.open_dialog_for(old_session)) {
                self.orphan_dialogs.insert(target, open.dialog);
            }
            self.forget_dialog(Some(old_session.as_str()));
            self.persistent_target_id = None;
            self.events
                .retain(|event| event["sessionId"].as_str() != Some(old_session.as_str()));
        }

        // Save the previously-active tab's emulation state so it can be restored
        // when we return to it (per-tab isolation). The active state lives in the
        // top-level fields; stash it under the old target id — but only if the
        // tab actually has overrides, so the map doesn't accumulate empty entries
        // for every tab ever visited. Stashed after the detach so the paused
        // requests above are answered with this tab's own mocks.
        if let Some(old_target) = old_target {
            let saved = TabEmulation {
                blocklist: std::mem::take(&mut self.blocklist),
                viewport: self.viewport.take(),
                geolocation: self.geolocation.take(),
                media: self.media.take(),
                webauthn: self.webauthn.take(),
                network_mocks: std::mem::take(&mut self.network_mocks),
                session_id: old_session.clone(),
            };
            stash_tab_emulation(&mut self.emulation_saved, old_target, saved);
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
            if let Err(e) = self
                .send_to_target_within(&session_id, method, json!({}), SETUP_CALL_TIMEOUT)
                .await
            {
                return Err(self.fail_setup(&session_id, target_id, false, e).await);
            }
        }

        // Load the new tab's saved emulation state into the active fields (empty
        // if this tab has none), then apply it to the new session. A tab with no
        // saved state starts clean — overrides from other tabs don't leak in.
        let restored = self.emulation_saved.remove(target_id).unwrap_or_default();
        // A session whose detach failed can still intercept on its old tab.
        // Link it to this target so late pauses keep using the tab's current
        // mock list (active here, or saved once we switch away again).
        if let Some(sid) = restored.session_id.as_deref() {
            if self.sessions_may_intercept.contains(sid) {
                self.retained_session_targets
                    .insert(sid.to_string(), target_id.to_string());
            }
        }
        self.blocklist = restored.blocklist;
        self.viewport = restored.viewport;
        self.geolocation = restored.geolocation;
        self.media = restored.media;
        self.webauthn = restored.webauthn;
        self.network_mocks = restored.network_mocks;

        if let Err(e) = self
            .apply_network_rules_on_enabled_session(&session_id)
            .await
        {
            return Err(self.fail_setup(&session_id, target_id, true, e).await);
        }

        if let Err(e) = self.apply_emulation_internal(&session_id).await {
            return Err(self.fail_setup(&session_id, target_id, true, e).await);
        }

        if !self.network_mocks.is_empty() {
            if let Err(e) = crate::commands::network_mock::apply_mocks(self, &session_id).await {
                return Err(self.fail_setup(&session_id, target_id, true, e).await);
            }
        }

        // Only after setup succeeds: adopting earlier would make the setup
        // calls themselves fail behind the dialog. The command that asked for
        // this tab now sees the dialog on its session and can report or settle
        // it before touching the page.
        if let Some(dialog) = self.orphan_dialogs.remove(target_id) {
            self.set_dialog_for_session(&session_id, dialog);
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
    ) -> Result<()> {
        if state_loaded {
            // Stash the state of the tab we failed to initialize so it's not lost.
            // It's currently in the active fields.
            let failed_state = TabEmulation {
                blocklist: std::mem::take(&mut self.blocklist),
                viewport: self.viewport.take(),
                geolocation: self.geolocation.take(),
                media: self.media.take(),
                webauthn: self.webauthn.take(),
                network_mocks: std::mem::take(&mut self.network_mocks),
                session_id: Some(session_id.to_string()),
            };
            stash_tab_emulation(
                &mut self.emulation_saved,
                failed_target_id.to_string(),
                failed_state,
            );
        }

        self.forget_dialog(Some(session_id));
        let detached = self
            .send("Target.detachFromTarget", json!({"sessionId": session_id}))
            .await;
        let connection_lost = detached
            .as_ref()
            .err()
            .is_some_and(|e| e.downcast_ref::<ConnectionLost>().is_some());
        if detached.is_ok() {
            self.session_ended(session_id);
        } else {
            // This session wrote Fetch.enable and its detach is unconfirmed:
            // Chrome may still pause its requests, so keep idle reads on and
            // keep its mock policy until this session really ends. The shared
            // `fetch_may_be_enabled` cannot decide this (another session may
            // have set it). A dead socket needs no record: the daemon retires
            // the connection and its sessions.
            let fetch_was_enabled = self.fetch_enable_sent_sessions.remove(session_id);
            if fetch_was_enabled && !connection_lost {
                self.sessions_may_intercept.insert(session_id.to_string());
                self.retained_session_targets
                    .insert(session_id.to_string(), failed_target_id.to_string());
            }
        }

        self.persistent_session = None;
        self.persistent_target_id = None;

        // Clear events captured from the failed session during the detach call
        // (which can read from the websocket and thus trigger push_event).
        self.network_events.clear();
        self.console_events.clear();
        self.events
            .retain(|event| event["sessionId"].as_str() != Some(session_id));

        // A lost connection has to reach the daemon, which retires it. Every
        // other cleanup failure keeps the original setup error instead.
        if connection_lost {
            detached.map(|_| ())
        } else {
            Ok(())
        }
    }

    /// Drop any stored emulation state for a target (e.g. when its tab closes).
    /// If it's the active tab, also reset the active fields and the now-invalid
    /// persistent session so the next command attaches cleanly.
    pub fn forget_target(&mut self, target_id: &str) {
        self.emulation_saved.remove(target_id);
        self.orphan_dialogs.remove(target_id);
        let retained: Vec<String> = self
            .retained_session_targets
            .iter()
            .filter(|(_, tid)| tid.as_str() == target_id)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in retained {
            self.session_ended(&sid);
        }
        if self.persistent_target_id.as_deref() == Some(target_id) {
            let session = self.persistent_session.clone();
            self.forget_dialog(session.as_deref());
            if let Some(ref sid) = session {
                self.session_ended(sid);
            }
            self.blocklist.clear();
            self.viewport = None;
            self.geolocation = None;
            self.media = None;
            self.webauthn = None;
            self.network_mocks.clear();
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
        if let Some(media) = self.media.clone() {
            self.send_to_target(session_id, "Emulation.setEmulatedMedia", media.cdp_params())
                .await?;
        }
        if let Some(options) = self.webauthn.as_ref().map(|a| a.options.clone()) {
            // The old authenticator went with the session that added it, so a
            // new one replaces it; credentials registered before do not come
            // back. If that fails the tab is left without one, as after any
            // detach, rather than failing every later command on it.
            match self.add_virtual_authenticator(session_id, &options).await {
                Ok(id) => self.webauthn = Some(VirtualAuthenticator { id, options }),
                // The connection or the page failed, not WebAuthn: the caller
                // must hear about it (a lost connection exits the daemon).
                Err(e) if is_connection_or_page_failure(&e) => return Err(e),
                Err(_) => self.webauthn = None,
            }
        }
        Ok(())
    }

    /// Turn on the virtual authenticator environment for a session and add one
    /// authenticator to it, returning its id. `enableUI: false` keeps Chrome's
    /// own passkey dialogs from opening in the user's browser.
    pub(crate) async fn add_virtual_authenticator(
        &mut self,
        session_id: &str,
        options: &AuthenticatorOptions,
    ) -> Result<String> {
        self.send_to_target(session_id, "WebAuthn.enable", json!({"enableUI": false}))
            .await?;
        let result = self
            .send_to_target(
                session_id,
                "WebAuthn.addVirtualAuthenticator",
                json!({"options": options.cdp_options()}),
            )
            .await?;
        result["authenticatorId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("No authenticatorId in addVirtualAuthenticator response"))
    }

    /// Apply the daemon's current `blocklist` to the persistent session via
    /// `Network.setBlockedURLs`. Patterns are Chrome-style globs (e.g. `*.png`).
    /// Falls back silently if no persistent session is active (e.g. during
    /// daemon startup or after a target switch that hasn't finished attaching).
    pub async fn apply_network_rules(&mut self) -> Result<()> {
        self.apply_network_rules_reported()
            .await
            .map_err(|e| e.error)
    }

    /// As [`Self::apply_network_rules`], but reports whether the rule write was
    /// attempted so a caller that changes `blocklist` can roll it back only
    /// when the outcome is certain (see [`ApplyRulesError`]).
    pub(crate) async fn apply_network_rules_reported(
        &mut self,
    ) -> std::result::Result<(), ApplyRulesError> {
        if let Some(ref session) = self.persistent_session.clone() {
            self.apply_network_rules_reported_for(session).await?;
        }
        Ok(())
    }

    /// Apply the current `blocklist` to a specific session via
    /// `Network.setBlockedURLs`. Used for both the persistent session and
    /// fallback per-command sessions (when the persistent one is unavailable).
    pub(crate) async fn apply_network_rules_internal(&mut self, session_id: &str) -> Result<()> {
        self.apply_network_rules_reported_for(session_id)
            .await
            .map_err(|e| e.error)
    }

    async fn apply_network_rules_reported_for(
        &mut self,
        session_id: &str,
    ) -> std::result::Result<(), ApplyRulesError> {
        self.send_to_target(session_id, "Network.enable", json!({}))
            .await
            .map_err(|error| ApplyRulesError {
                error,
                write_attempted: false,
            })?;
        // The pre-write dialog guard can reject this before anything reaches
        // Chrome. Only report an attempted write once it is on the wire; a
        // refused write leaves the stored list safe to roll back.
        let mut wrote = false;
        self.send_raw_within_reported(
            "Network.setBlockedURLs",
            json!({"urls": self.blocklist}),
            Some(session_id),
            cdp_call_timeout(),
            &mut wrote,
        )
        .await
        .map_err(|error| ApplyRulesError {
            error,
            write_attempted: wrote,
        })?;
        Ok(())
    }

    /// Setup path only: `Network.enable` already ran with the bounded setup
    /// budget, so sending it again here would spend the ordinary call budget
    /// on a page that stopped answering in between. Only the blocklist is
    /// left to apply, within the same setup budget. Callers that have not
    /// enabled the domain keep using [`Self::apply_network_rules_internal`].
    async fn apply_network_rules_on_enabled_session(&mut self, session_id: &str) -> Result<()> {
        self.send_to_target_within(
            session_id,
            "Network.setBlockedURLs",
            json!({"urls": self.blocklist}),
            SETUP_CALL_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Drain accumulated network events from the persistent session.
    pub fn drain_network_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.network_events).into()
    }

    /// Errors the page raised since `mark` (an earlier `console_pushed`), as
    /// stderr notes, without draining them: `console` still returns them.
    pub fn page_errors_since(&self, mark: u64) -> Vec<String> {
        let new = usize::try_from(self.console_pushed - mark).unwrap_or(usize::MAX);
        let skip = self.console_events.len().saturating_sub(new);
        page_error_notes(self.console_events.iter().skip(skip))
    }

    /// Drain accumulated console events from the persistent session.
    pub fn drain_console_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.console_events).into()
    }

    /// Forget calls that stopped waiting without cleaning up after
    /// themselves: a caller's own timeout (`probe_within_deadline` in
    /// navigate.rs) drops a call mid-wait, with its id still in
    /// `awaiting_replies`, and its late reply would then sit in
    /// `deferred_replies` for the life of the daemon. Run between daemon
    /// requests, when no call is waiting, so every entry left is such a one.
    pub(crate) fn forget_abandoned_calls(&mut self) {
        self.awaiting_replies.clear();
        self.deferred_replies.clear();
    }

    pub(crate) fn push_event(&mut self, event: Value) {
        // Only take-heapsnapshot reads these, straight off the socket. One it
        // gave up on keeps streaming, and its chunks, the whole heap, must not
        // fill the buffers or crowd out the events later commands wait for.
        if matches!(
            event["method"].as_str(),
            Some("HeapProfiler.addHeapSnapshotChunk" | "HeapProfiler.reportHeapSnapshotProgress")
        ) {
            return;
        }
        self.note_dialog_event(&event);
        if event["method"] == "Target.detachedFromTarget" {
            if let Some(sid) = event["params"]["sessionId"].as_str() {
                self.session_ended(sid);
            }
        }
        if event["method"] == "Fetch.requestPaused" {
            self.paused_requests.push_back(event);
            return;
        }
        // Only route events into the persistent buffers when the event's
        // sessionId matches the persistent page session (flatten-mode events
        // are tagged with sessionId). Events from ad-hoc sessions (sw-logs
        // attach, per-command live sessions) fall through and go into the
        // general events buffer — otherwise a subsequent page `console` drain
        // would return extension service worker logs.
        let from_persistent_session = self
            .persistent_session
            .as_deref()
            .is_some_and(|s| event["sessionId"].as_str() == Some(s));

        if from_persistent_session {
            let method = event.get("method").and_then(|v| v.as_str()).unwrap_or("");
            match method {
                // Nothing reads it; buffering it only crowded requests out.
                "Network.loadingFinished" => return,
                "Network.requestWillBeSent"
                | "Network.requestWillBeSentExtraInfo"
                | "Network.responseReceived"
                | "Network.responseReceivedExtraInfo"
                | "Network.loadingFailed" => {
                    self.network_events.push_back(slim_network_event(event));
                    if self.network_events.len() > MAX_PERSISTENT_EVENTS {
                        self.network_events.pop_front();
                    }
                    return;
                }
                "Runtime.consoleAPICalled" | "Runtime.exceptionThrown" => {
                    self.console_pushed += 1;
                    self.console_events.push_back(slim_console_event(event));
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

    /// [`Self::send_to_target`] with its own limit, for setup calls that should
    /// fail fast on a blocked page ([`SETUP_CALL_TIMEOUT`]).
    pub async fn send_to_target_within(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
        limit: std::time::Duration,
    ) -> Result<Value> {
        self.send_raw_within(method, params, Some(session_id), limit)
            .await
    }

    /// Send a command and return the message ID immediately without waiting for response.
    pub async fn send_raw_no_wait(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<u64> {
        self.send_raw_no_wait_within(session_id, method, params, cdp_call_timeout())
            .await
    }

    /// [`Self::send_raw_no_wait`] with a caller-supplied write budget. A short
    /// setup call must not spend the full write timeout on a browser that has
    /// stopped reading: that would hold the daemon, and every caller queued
    /// behind it, long past the call's own limit.
    async fn send_raw_no_wait_within(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
        limit: std::time::Duration,
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
        // A failed write means Chrome's connection is gone too, so it is a
        // ConnectionLost like read_text's failures: the daemon exits only on
        // those. So is a write that never finishes (a remote browser that
        // stopped reading), which would otherwise hold the daemon, and every
        // caller queued behind it, forever; a frame given up halfway leaves
        // the stream unusable anyway.
        match tokio::time::timeout(limit, self.write.send(Message::Text(text))).await {
            Ok(sent) => sent.map_err(|e| ConnectionLost(format!("WebSocket error: {e}")))?,
            Err(_) => bail!(ConnectionLost(format!(
                "Writing to the browser did not finish within {}s; the connection is treated \
                 as lost",
                limit.as_secs()
            ))),
        }
        Ok(id)
    }

    async fn send_raw(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        self.send_raw_within(method, params, session_id, cdp_call_timeout())
            .await
    }

    async fn send_raw_within(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        limit: std::time::Duration,
    ) -> Result<Value> {
        let mut wrote = false;
        self.send_raw_within_reported(method, params, session_id, limit, &mut wrote)
            .await
    }

    /// [`Self::send_raw_within`], also reporting whether the request reached
    /// the write. A dialog the pre-write guard settled or rejected would
    /// otherwise look like a request Chrome may have applied.
    async fn send_raw_within_reported(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        limit: std::time::Duration,
        wrote: &mut bool,
    ) -> Result<Value> {
        // The deadline covers all the call waits on: a dialog left open, the
        // write, the reply, and the dialogs handled while waiting for it.
        let started = tokio::time::Instant::now();
        // A page with a dialog open answers nothing until the dialog closes,
        // and Chrome announced it only once, possibly to an earlier read.
        if method != "Page.handleJavaScriptDialog" {
            if let Some(sid) = session_id {
                // A liveness ping can leave the opening in `unread`. Note it
                // before writing so this call settles or reports it first.
                let queued = self.note_queued_dialogs_for(sid);
                if let Err(e) = self.settle_dialog_within(sid, method, started, limit).await {
                    // Only a dialog a ping set aside was necessarily open
                    // before this call's write. One the action itself opened
                    // is still in `open_dialogs` and must keep input_sent, or
                    // a successful action would be reported as skipped.
                    if queued {
                        self.input_sent = false;
                    }
                    return Err(e);
                }
            }
        }
        // Settlement can run past the limit when its reply arrives during a
        // liveness ping. Writing the operation then would let it run after the
        // caller gave up, and for non-Runtime calls nothing stops it.
        let remaining = limit.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            bail!(PageUnresponsive(format!(
                "{method} was not sent: its {}s limit passed while a JavaScript dialog was \
                 settled",
                limit.as_secs()
            )));
        }
        *wrote = true;
        // Remember which session actually got Fetch.enable: a failed setup
        // needs this to tell whether its detach left requests paused.
        if method == "Fetch.enable" {
            if let Some(sid) = session_id {
                self.fetch_enable_sent_sessions.insert(sid.to_string());
            }
        }
        let id = self
            .send_raw_no_wait_within(
                session_id,
                method,
                params,
                remaining.min(cdp_call_timeout()),
            )
            .await?;
        self.awaiting_replies.insert(id);
        // Unregister on every exit, including errors, or a late reply would
        // be parked in `deferred_replies` for a call that has stopped waiting.
        let result = async {
            // A message a deadline ping queued is read here and its owner loops
            // again; the check it made must count for the whole call.
            let mut checked = false;
            loop {
                let resp_text = self
                    .read_text_for_call(id, method, session_id, started, limit, &mut checked)
                    .await?;
                let resp: Value = serde_json::from_str(&resp_text)?;
                self.note_dialog_event(&resp);

                // Proactively handle or fail if a dialog is opened during execution
                if resp.get("method").and_then(|v| v.as_str())
                    == Some("Page.javascriptDialogOpening")
                    && method != "Page.handleJavaScriptDialog"
                {
                    let dialog_session = resp["sessionId"].as_str().map(str::to_string);
                    let same_session = dialog_session
                        .as_deref()
                        .is_none_or(|s| Some(s) == session_id);
                    // The persistent session belongs to one target. A call on
                    // a one-off session is only made when that target is a
                    // different one (or the session is gone), so a dialog on
                    // the persistent session is another page's: recorded above
                    // for that page, and this call keeps waiting.
                    let other_page = !same_session
                        && self.persistent_session.as_deref() == dialog_session.as_deref()
                        && session_id.is_some();
                    if other_page {
                        continue;
                    }
                    if same_session {
                        // A ping may have set aside the closing alongside the
                        // opening. Apply it first, as the no-action path does,
                        // or this action could answer a replacement dialog.
                        if self.take_queued_dialog_close(dialog_session.as_deref().or(session_id)) {
                            continue;
                        }
                        if let Some(action) = self.dialog_action.clone() {
                            self.handle_dialog_helper(
                                dialog_session.as_deref().or(session_id),
                                action,
                                dialog_from_event(&resp),
                                method,
                                started,
                                limit,
                            )
                            .await?;
                            continue;
                        }
                    }
                    // A dialog holds up its page, not the browser: a browser-level
                    // call still gets its reply, and failing it would report a tab
                    // `new-page` opened as not opened. The dialog, recorded above,
                    // is for the next call on its page. A page call fails at once
                    // rather than wait out a dialog it cannot handle — a stale
                    // session tag on this page must not borrow its --dialog-action.
                    if session_id.is_some() {
                        if self.take_queued_dialog_close(dialog_session.as_deref().or(session_id)) {
                            // The opening and its closing were set aside together
                            // by a ping; the dialog is gone, so keep reading for
                            // this call's reply.
                            continue;
                        }
                        bail!(dialog_from_event(&resp));
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
                } else if let Some(other_id) = resp.get("id").and_then(|v| v.as_u64()) {
                    // A reply to another call still waiting on it: the dialog
                    // handler may have read it while draining the socket. Keep
                    // it for its owner; fire-and-forget replies need no slot.
                    if self.awaiting_replies.contains(&other_id) {
                        self.deferred_replies.push_back((other_id, resp_text));
                    }
                }
                // Skip other unrelated responses
            }
        }
        .await;
        self.awaiting_replies.remove(&id);
        // Nobody will read a reply deferred for a call that has exited, so
        // drop it instead of carrying its payload for the daemon's lifetime.
        self.deferred_replies.retain(|(rid, _)| *rid != id);
        result
    }

    /// Answer `dialog` with `action`, within the limit of the call it held up
    /// (`call`, timed from `started`).
    fn handle_dialog_helper<'a>(
        &'a mut self,
        session_id: Option<&'a str>,
        action: String,
        dialog: DialogOpen,
        call: &'a str,
        started: tokio::time::Instant,
        limit: std::time::Duration,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // A page that opens another dialog for each one answered would
            // otherwise hold the call, and every caller queued behind it,
            // forever.
            if started.elapsed() >= limit {
                return Err(dialog_past_limit(call, limit, dialog));
            }
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

            let remaining = limit.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(dialog_past_limit(call, limit, dialog));
            }
            let id = self
                .send_raw_no_wait_within(
                    session_id,
                    "Page.handleJavaScriptDialog",
                    handler_params,
                    remaining.min(cdp_call_timeout()),
                )
                .await?;
            self.awaiting_replies.insert(id);
            let result = async {
                let mut checked = false;
                loop {
                    let resp_text = self
                        .read_text_for_call(
                            id,
                            "Page.handleJavaScriptDialog",
                            session_id,
                            started,
                            limit,
                            &mut checked,
                        )
                        .await?;
                    let resp: Value = serde_json::from_str(&resp_text)?;
                    self.note_dialog_event(&resp);

                    if resp.get("method").and_then(|v| v.as_str())
                        == Some("Page.javascriptDialogOpening")
                    {
                        let other_session = resp["sessionId"].as_str();
                        if other_session.is_some_and(|s| session_id.is_some_and(|sid| s != sid)) {
                            // Another session's dialog: recorded above for its
                            // own page, not ours to answer with this action.
                            continue;
                        }
                        // A ping may have set the closing aside with the
                        // opening. Apply it first, as the top-level branch
                        // does, or this action can answer a later dialog.
                        if self.take_queued_dialog_close(other_session.or(session_id)) {
                            continue;
                        }
                        self.handle_dialog_helper(
                            session_id,
                            action.clone(),
                            dialog_from_event(&resp),
                            call,
                            started,
                            limit,
                        )
                        .await?;
                        continue;
                    }

                    if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        if let Some(error) = resp.get("error") {
                            // Closed meanwhile, by a person or the page navigating
                            // away: the goal is met either way.
                            if is_no_dialog_error(error) {
                                self.forget_dialog(session_id);
                                return Ok(());
                            }
                            // The dialog is still open: keep tracking it, or later
                            // calls would wait behind it with no way to settle it.
                            bail!(
                                "CDP error in Page.handleJavaScriptDialog: {}",
                                serde_json::to_string_pretty(error)?
                            );
                        }
                        self.forget_dialog(session_id);
                        return Ok(());
                    } else if resp.get("method").is_some() {
                        self.push_event(resp);
                    } else if let Some(other_id) = resp.get("id").and_then(|v| v.as_u64()) {
                        // The call this dialog held up can finish while we wait
                        // for the handler's own reply. Hand its reply back to it.
                        if self.awaiting_replies.contains(&other_id) {
                            self.deferred_replies.push_back((other_id, resp_text));
                        }
                    }
                }
            }
            .await;
            self.awaiting_replies.remove(&id);
            self.deferred_replies.retain(|(rid, _)| *rid != id);
            result
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

    /// Check that the browser still answers, for `caffeinate`, without changing
    /// what the next command sees.
    ///
    /// `send` would fail on a dialog-opening event and use it up, but nobody
    /// reads a ping's errors: the next command would never hear about the
    /// dialog and could hang behind it. The event is set aside for that
    /// command instead, which reports or handles it as usual.
    pub async fn ping(&mut self) -> Result<()> {
        let id = self
            .send_raw_no_wait(None, "Browser.getVersion", json!({}))
            .await?;
        loop {
            // The socket, not read_text: an event set aside by an earlier ping
            // would come straight back and be set aside again, forever.
            let text = self.read_socket_text().await?;
            let msg: Value = serde_json::from_str(&text)?;
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(error) = msg.get("error") {
                    bail!(
                        "CDP error in Browser.getVersion: {}",
                        serde_json::to_string_pretty(error)?
                    );
                }
                return Ok(());
            }
            match msg.get("method").and_then(|v| v.as_str()) {
                // Closings wait with openings, so the next reader sees them in
                // wire order and a dialog closed meanwhile isn't left open.
                Some("Page.javascriptDialogOpening" | "Page.javascriptDialogClosed") => {
                    self.unread.push_back(text)
                }
                Some(_) => self.push_event(msg),
                // A reply to a call still waiting on it: a ping can run in
                // the middle of one (`read_text_for_call`), which must get
                // its reply.
                None => self.unread.push_back(text),
            }
        }
    }

    /// Read the next message for a call waiting on its reply, giving up at
    /// `limit` after `started`.
    ///
    /// Whenever the socket goes quiet, the browser gets a ping. No answer
    /// means the connection is gone ([`ConnectionLost`], so the daemon exits
    /// and the next command reconnects). An answer means the page, not the
    /// connection, is slow, and the call is only given up at `limit`, leaving
    /// the daemon and its approved connection in place.
    pub(crate) async fn read_text_for_call(
        &mut self,
        id: u64,
        method: &str,
        session_id: Option<&str>,
        started: tokio::time::Instant,
        limit: std::time::Duration,
        // Owned by the caller's read loop, so a ping that queued a message the
        // owner then handles does not earn another check on re-entry.
        checked: &mut bool,
    ) -> Result<String> {
        loop {
            // A dialog handler that drained the socket may have set this
            // call's reply aside. It belongs to us, not to the handler.
            if let Some(pos) = self.deferred_replies.iter().position(|(rid, _)| *rid == id) {
                let (_, text) = self.deferred_replies.remove(pos).unwrap();
                return Ok(text);
            }
            let remaining = limit.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                // Whose fault: a lost connection exits the daemon, a stuck
                // page must not.
                if self.unread.is_empty() && !*checked {
                    self.check_alive().await?;
                    *checked = true;
                }
                // What a liveness ping set aside arrived in time, the awaited
                // reply perhaps among it.
                if let Some(text) = self.unread.pop_front() {
                    return Ok(text);
                }
                // An endless loop in the page would keep answering nothing
                // for every later command too; stopping the script frees it.
                let mut stopped = "";
                if method.starts_with("Runtime.") {
                    if let Some(sid) = session_id {
                        // At the deadline this is the last thing the daemon
                        // writes. A failed write means the connection is gone:
                        // the daemon has to see that error to retire it.
                        match self
                            .send_raw_no_wait_within(
                                Some(sid),
                                "Runtime.terminateExecution",
                                json!({}),
                                SETUP_CALL_TIMEOUT,
                            )
                            .await
                        {
                            Ok(_) => stopped = " (a stop was requested for its running JavaScript)",
                            Err(e) if e.downcast_ref::<ConnectionLost>().is_some() => {
                                return Err(e);
                            }
                            Err(_) => {}
                        }
                    }
                }
                let longer = if limit >= cdp_call_timeout() {
                    " For a call that is just slow, set CHROME_CDP_TIMEOUT_SECS (read when the \
                     daemon starts)."
                } else {
                    ""
                };
                bail!(PageUnresponsive(format!(
                    "{method} got no answer from the page within {}s, though the browser \
                     answers. Either the page is stuck (an endless loop{stopped}, a hung \
                     renderer), or a JavaScript dialog is open that this daemon did not see \
                     open and cannot handle: a person has to close it, or `close-page` closes \
                     the tab.{longer}",
                    limit.as_secs()
                )));
            }
            match tokio::time::timeout(remaining.min(QUIET_BEFORE_CHECK), self.read_text()).await {
                Ok(read) => return read,
                Err(_) => {
                    self.check_alive().await?;
                    *checked = true;
                }
            }
        }
    }

    /// Fail with [`ConnectionLost`] unless the browser answers a ping soon.
    pub(crate) async fn check_alive(&mut self) -> Result<()> {
        match tokio::time::timeout(ALIVE_CHECK_TIMEOUT, self.ping()).await {
            Err(_) => bail!(ConnectionLost(format!(
                "The browser did not answer a check within {}s; the connection is treated as \
                 lost",
                ALIVE_CHECK_TIMEOUT.as_secs()
            ))),
            Ok(Err(e)) if e.downcast_ref::<ConnectionLost>().is_some() => Err(e),
            // A CDP error is still an answer.
            Ok(_) => Ok(()),
        }
    }

    /// Track dialogs opening and closing, from whichever read sees them.
    fn note_dialog_event(&mut self, msg: &Value) {
        match msg["method"].as_str() {
            Some("Page.javascriptDialogOpening") => {
                let dialog = OpenDialog {
                    session_id: msg["sessionId"].as_str().map(str::to_string),
                    dialog: dialog_from_event(msg),
                };
                // A new opening supersedes an older one on the same session;
                // every other session keeps its own record.
                self.open_dialogs
                    .retain(|d| d.session_id != dialog.session_id);
                self.open_dialogs.push(dialog.clone());
                self.open_dialog = Some(dialog);
            }
            Some("Page.javascriptDialogClosed") => {
                self.forget_dialog(msg["sessionId"].as_str());
            }
            _ => {}
        }
    }

    /// Apply dialog transitions a liveness ping set aside for `session_id`
    /// before the next command writes to that page. Only messages for that
    /// session are removed; everything else stays in `unread` in wire order,
    /// so other callers still get their replies and events.
    fn note_queued_dialogs_for(&mut self, session_id: &str) -> bool {
        let mut found = false;
        let mut i = 0;
        while i < self.unread.len() {
            let is_transition = serde_json::from_str::<Value>(&self.unread[i])
                .ok()
                .is_some_and(|m| {
                    matches!(
                        m["method"].as_str(),
                        Some("Page.javascriptDialogOpening" | "Page.javascriptDialogClosed")
                    ) && m["sessionId"].as_str() == Some(session_id)
                });
            if is_transition {
                let text = self.unread.remove(i).unwrap();
                if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                    self.note_dialog_event(&msg);
                }
                found = true;
            } else {
                i += 1;
            }
        }
        found
    }

    /// Apply a closing a liveness ping set aside alongside its opening, so a
    /// dialog that already closed does not fail the call reading the opening.
    /// Returns whether one was found for `session_id`.
    fn take_queued_dialog_close(&mut self, session_id: Option<&str>) -> bool {
        let pos = self.unread.iter().position(|t| {
            serde_json::from_str::<Value>(t).ok().is_some_and(|m| {
                m["method"].as_str() == Some("Page.javascriptDialogClosed")
                    && m["sessionId"].as_str() == session_id
            })
        });
        if let Some(text) = pos.and_then(|p| self.unread.remove(p)) {
            if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                self.note_dialog_event(&msg);
            }
            return true;
        }
        false
    }

    /// The open dialog on `session_id`, if any. The public field is a mirror
    /// of the newest dialog; a caller writing it directly (as `css_styles`
    /// does) still has to be believed.
    fn open_dialog_for(&self, session_id: &str) -> Option<OpenDialog> {
        self.open_dialogs
            .iter()
            .rev()
            .find(|d| d.session_id.as_deref() == Some(session_id))
            .or_else(|| {
                self.open_dialog
                    .as_ref()
                    .filter(|d| d.session_id.as_deref() == Some(session_id))
            })
            .cloned()
    }

    /// Record a dialog previously seen on another session of this tab as open
    /// on `session_id`, so the next call settles or reports it at once.
    fn set_dialog_for_session(&mut self, session_id: &str, dialog: DialogOpen) {
        self.open_dialogs
            .retain(|d| d.session_id.as_deref() != Some(session_id));
        self.open_dialogs.push(OpenDialog {
            session_id: Some(session_id.to_string()),
            dialog,
        });
        self.open_dialog = self.open_dialogs.last().cloned();
    }

    /// Forget the open dialog if it belongs to `session_id`: it was handled,
    /// closed, or its session is gone (only that session could handle it).
    /// Other sessions' records are left alone.
    pub fn forget_dialog(&mut self, session_id: Option<&str>) {
        self.open_dialogs
            .retain(|d| d.session_id.as_deref() != session_id);
        if self
            .open_dialog
            .as_ref()
            .is_some_and(|d| d.session_id.as_deref() == session_id)
        {
            self.open_dialog = self.open_dialogs.last().cloned();
        }
    }

    /// Deal with a dialog already open on `session_id` before a command
    /// talks to the page, which answers nothing while it is open: handle it
    /// with the command's `--dialog-action`, or fail now.
    pub async fn settle_open_dialog(&mut self, session_id: &str) -> Result<()> {
        let started = tokio::time::Instant::now();
        let call = "Page.handleJavaScriptDialog";
        self.note_queued_dialogs_for(session_id);
        self.settle_dialog_within(session_id, call, started, cdp_call_timeout())
            .await
    }

    /// [`Self::settle_open_dialog`] as part of `call`, within its limit.
    async fn settle_dialog_within(
        &mut self,
        session_id: &str,
        call: &str,
        started: tokio::time::Instant,
        limit: std::time::Duration,
    ) -> Result<()> {
        // A ping may have set the closing aside with the opening. Only apply
        // it once that opening is the one we are tracking; consuming the
        // closing first would leave a still-queued opening to fail this call.
        let mut open = self.open_dialog_for(session_id);
        if open.is_some() {
            self.take_queued_dialog_close(Some(session_id));
            open = self.open_dialog_for(session_id);
        }
        let Some(open) = open else {
            return Ok(());
        };
        match self.dialog_action.clone() {
            Some(action) => {
                self.handle_dialog_helper(
                    Some(session_id),
                    action,
                    open.dialog,
                    call,
                    started,
                    limit,
                )
                .await
            }
            None => Err(open.dialog.into()),
        }
    }

    /// Read the next text message, skipping non-text frames. Messages a ping
    /// set aside come first, as if they had only just arrived.
    pub async fn read_text(&mut self) -> Result<String> {
        self.answer_paused_requests().await?;
        if let Some(text) = self.unread.pop_front() {
            return Ok(text);
        }
        self.read_socket_text().await
    }

    /// Mocks governing a paused request's session. During a tab switch the
    /// active list still belongs to the outgoing session, so it is used as-is;
    /// once a new session is attached, an older session's mocks live in its
    /// stashed tab state and must be found there. A session with no mocks falls
    /// back to an empty list, which sends the request on to the network.
    fn mocks_for_session(
        &self,
        session: Option<&str>,
    ) -> &[crate::commands::network_mock::NetworkMock] {
        // A retained session from a failed detach resolves through its target's
        // authoritative mock list — saved if stashed (including mid-switch or
        // mid-cleanup), active if that tab is active — before the generic
        // `persistent_session.is_none()` fallback can hijack it.
        if let Some(session) = session {
            if let Some(target_id) = self.retained_session_targets.get(session) {
                if let Some(state) = self.emulation_saved.get(target_id) {
                    return &state.network_mocks;
                }
                if self.persistent_target_id.as_deref() == Some(target_id.as_str()) {
                    return &self.network_mocks;
                }
                return &[];
            }
            if let Some(state) = self
                .emulation_saved
                .values()
                .find(|state| state.session_id.as_deref().is_some_and(|s| s == session))
            {
                return &state.network_mocks;
            }
        }
        if session.is_none()
            || self.persistent_session.is_none()
            || self.persistent_session.as_deref() == session
        {
            return &self.network_mocks;
        }
        &[]
    }

    /// The matching mock in the same list [`Self::mocks_for_session`] chose, so
    /// the hit is credited to the tab whose interception answered the request.
    fn mock_for_session_mut(
        &mut self,
        session: Option<&str>,
        index: usize,
    ) -> Option<&mut crate::commands::network_mock::NetworkMock> {
        if let Some(session) = session {
            if let Some(target_id) = self.retained_session_targets.get(session).cloned() {
                if let Some(state) = self.emulation_saved.get_mut(&target_id) {
                    return state.network_mocks.get_mut(index);
                }
                if self.persistent_target_id.as_deref() == Some(target_id.as_str()) {
                    return self.network_mocks.get_mut(index);
                }
                return None;
            }
            if let Some(state) = self
                .emulation_saved
                .values_mut()
                .find(|state| state.session_id.as_deref().is_some_and(|s| s == session))
            {
                return state.network_mocks.get_mut(index);
            }
        }
        if session.is_none()
            || self.persistent_session.is_none()
            || self.persistent_session.as_deref() == session
        {
            return self.network_mocks.get_mut(index);
        }
        None
    }

    /// Answer the requests the page is holding for a mock. Each stays queued
    /// until its answer is sent, so a read cancelled mid-send retries it
    /// rather than leave the request hanging.
    async fn answer_paused_requests(&mut self) -> Result<()> {
        while let Some(event) = self.paused_requests.front().cloned() {
            let session = event["sessionId"].as_str().map(str::to_string);
            let (method, params, matched) = crate::commands::network_mock::reply_to_paused(
                self.mocks_for_session(session.as_deref()),
                &event["params"],
            );
            self.send_raw_no_wait(session.as_deref(), method, params)
                .await?;
            self.paused_requests.pop_front();
            // Counted only now: a send cancelled above is retried, and a
            // retry is the same request.
            if let Some(i) = matched {
                if let Some(mock) = self.mock_for_session_mut(session.as_deref(), i) {
                    mock.hits += 1;
                }
            }
        }
        Ok(())
    }

    /// A session ended, so it can no longer hold requests for a mock. Stop
    /// idle reads and drop the requests it left paused. Kept separate from a
    /// failed `Fetch.disable`: there the same session may still intercept.
    fn session_ended(&mut self, session_id: &str) {
        self.sessions_may_intercept.remove(session_id);
        self.retained_session_targets.remove(session_id);
        self.fetch_enable_sent_sessions.remove(session_id);
        for saved in self.emulation_saved.values_mut() {
            if saved.session_id.as_deref() == Some(session_id) {
                saved.session_id = None;
            }
        }
        // `fetch_may_be_enabled` belongs to the active persistent session: only
        // clear it when that session ended (or none is active) and no other
        // retained session may still be intercepting.
        if self
            .persistent_session
            .as_deref()
            .is_none_or(|active| active == session_id)
            && self.sessions_may_intercept.is_empty()
        {
            self.fetch_may_be_enabled = false;
        }
        self.paused_requests
            .retain(|e| e["sessionId"].as_str() != Some(session_id));
    }

    /// Whether the daemon must keep reading between commands: with mocks
    /// set, the page's requests wait on answers only reads produce.
    pub fn wants_idle_reads(&self) -> bool {
        !self.network_mocks.is_empty()
            || !self.paused_requests.is_empty()
            || self.fetch_may_be_enabled
            || !self.sessions_may_intercept.is_empty()
    }

    /// One read between commands: buffer what arrives and answer paused
    /// requests. Cancel-safe, so the daemon can drop it for a new command.
    pub async fn pump_once(&mut self) -> Result<()> {
        let text = self.read_text().await?;
        if let Ok(msg) = serde_json::from_str::<Value>(&text) {
            if msg.get("method").is_some() {
                self.push_event(msg);
            }
        }
        self.answer_paused_requests().await
    }

    /// Read the next text message from the WebSocket itself, skipping non-text frames.
    async fn read_socket_text(&mut self) -> Result<String> {
        loop {
            match self.read.next().await {
                Some(Ok(Message::Text(text))) => return Ok(text.to_string()),
                Some(Ok(Message::Close(_))) => {
                    bail!(ConnectionLost("WebSocket closed by server".into()))
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => bail!(ConnectionLost(format!("WebSocket error: {e}"))),
                None => bail!(ConnectionLost("WebSocket connection closed".into())),
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

    /// Create a new page target and return its target ID. `background` opens
    /// it without bringing it to the front, so it doesn't take focus from the
    /// tab the user is looking at.
    pub async fn create_target(&mut self, url: &str, background: bool) -> Result<String> {
        let result = self
            .send("Target.createTarget", create_target_params(url, background))
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
    ///
    /// With `collect` false the events only go to the buffers and nothing is
    /// returned: callers draining the persistent buffers would otherwise hold
    /// a second, uncapped copy of everything for the whole window.
    pub async fn read_events_for(&mut self, duration_ms: u64, collect: bool) -> Result<Vec<Value>> {
        // With `collect` false nothing is returned, so pending events must stay
        // buffered for the later drains that do collect them.
        let mut events: Vec<Value> = if collect {
            self.events.drain(..).collect()
        } else {
            Vec::new()
        };
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
                            let is_persistent_event = self
                                .persistent_session
                                .as_deref()
                                .is_some_and(|s| resp["sessionId"].as_str() == Some(s));
                            if is_persistent_event {
                                self.push_event(resp.clone());
                            } else if !method.starts_with("Network.")
                                && !method.starts_with("Runtime.")
                            {
                                self.push_event(resp.clone());
                            }
                            if collect {
                                events.push(resp);
                            }
                        }
                    }
                }
                // Timeout expired — normal completion path.
                Err(_) => break,
                // A read fails only when the connection is gone. Returning
                // what came before would pass a cut-short window off as the
                // whole one, and a warning would reach nobody: the daemon has
                // no stderr. The daemon also exits only on this error.
                Ok(Err(e)) => return Err(e),
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

/// Longest string argument a buffered console event keeps. A page can log a
/// whole file in one call, and the buffer holds thousands of events.
const MAX_BUFFERED_STRING_CHARS: usize = 10_000;

/// The fields `network` reads, headers included so `--request-headers` and
/// `--response-headers` can show requests made before the command. POST
/// bodies, initiator stacks (up to hundreds of kilobytes each) and timing are
/// dropped, or the event cap would be the only bound on the daemon's memory.
fn slim_network_event(event: Value) -> Value {
    let p = &event["params"];
    let mut params = json!({"requestId": p["requestId"]});
    match event["method"].as_str() {
        Some("Network.requestWillBeSent") => {
            let r = &p["request"];
            params["request"] =
                json!({"url": r["url"], "method": r["method"], "headers": r["headers"]});
            params["type"] = p["type"].clone();
            if let Some(r) = p.get("redirectResponse") {
                params["redirectResponse"] = json!({
                    "status": r["status"],
                    "statusText": r["statusText"],
                    "headers": r["headers"],
                });
                // Which hops have wire headers, to line those up with them.
                params["redirectHasExtraInfo"] = p["redirectHasExtraInfo"].clone();
            }
        }
        Some("Network.responseReceived") => {
            let r = &p["response"];
            params["response"] = json!({
                "status": r["status"],
                "statusText": r["statusText"],
                "headers": r["headers"],
            });
            params["hasExtraInfo"] = p["hasExtraInfo"].clone();
        }
        // The headers as they went over the wire, Cookie and Set-Cookie
        // included, which the page's view leaves out.
        Some("Network.requestWillBeSentExtraInfo" | "Network.responseReceivedExtraInfo") => {
            params["headers"] = p["headers"].clone();
        }
        Some("Network.loadingFailed") => params["errorText"] = p["errorText"].clone(),
        _ => return event,
    }
    json!({"method": event["method"], "sessionId": event["sessionId"], "params": params})
}

/// A console event without its stack traces, and with long strings cut.
fn slim_console_event(mut event: Value) -> Value {
    if let Some(params) = event.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("stackTrace");
        if let Some(details) = params
            .get_mut("exceptionDetails")
            .and_then(Value::as_object_mut)
        {
            details.remove("stackTrace");
        }
        for arg in params
            .get_mut("args")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(Value::String(s)) = arg.get_mut("value") {
                let cut = match crate::format::truncate_chars(s, MAX_BUFFERED_STRING_CHARS) {
                    std::borrow::Cow::Owned(cut) => Some(cut),
                    std::borrow::Cow::Borrowed(_) => None,
                };
                if let Some(cut) = cut {
                    *s = cut;
                }
            }
        }
    }
    event
}

/// Most errors a command lists; the rest are a `console` call away.
const MAX_PAGE_ERROR_NOTES: usize = 10;

/// One line per uncaught exception or `console.error`, so a click that broke
/// the page doesn't look like one that worked. First lines only: the stack
/// is in `console`.
fn page_error_notes<'a>(events: impl Iterator<Item = &'a Value>) -> Vec<String> {
    let errors: Vec<String> = events
        .filter_map(|e| {
            let params = &e["params"];
            let text = match e["method"].as_str()? {
                "Runtime.exceptionThrown" => {
                    crate::remote_object::describe_exception(&params["exceptionDetails"])
                }
                "Runtime.consoleAPICalled" if params["type"] == "error" => {
                    let args = params["args"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                    format!("console.error: {}", join_console_args(args))
                }
                _ => return None,
            };
            let first_line = text.lines().next().unwrap_or("").to_string();
            Some(first_line)
        })
        .collect();
    let mut notes: Vec<String> = errors
        .iter()
        .take(MAX_PAGE_ERROR_NOTES)
        .map(|e| format!("[page error] {e}"))
        .collect();
    if errors.len() > MAX_PAGE_ERROR_NOTES {
        notes.push(format!(
            "[page error] … and {} more; `console --type error` lists them all",
            errors.len() - MAX_PAGE_ERROR_NOTES
        ));
    }
    notes
}

/// A failure of the connection or of the page as a whole, as opposed to one
/// call being refused: whoever runs setup has to stop on it.
pub fn is_connection_or_page_failure(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ConnectionLost>().is_some() || e.downcast_ref::<PageUnresponsive>().is_some()
}

fn dialog_from_event(event: &Value) -> DialogOpen {
    DialogOpen {
        dialog_type: event["params"]["type"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        message: event["params"]["message"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

/// A dialog that opens once the call's time is up is left open, and
/// recorded, so the next command handles it with its own --dialog-action or
/// fails at once. Still a [`DialogOpen`], so an input that set the dialogs
/// off reports that it ran rather than invite a second click. Stopping the
/// script would not help: Chrome 158 does not act on `Runtime.terminateExecution`
/// while a script is going from one alert to the next.
fn dialog_past_limit(call: &str, limit: std::time::Duration, dialog: DialogOpen) -> anyhow::Error {
    anyhow::Error::new(dialog).context(format!(
        "{call} ran past its {}s limit, so this dialog is left open (`close-page` closes a tab \
         that keeps opening them)",
        limit.as_secs()
    ))
}

fn is_no_dialog_error(error: &Value) -> bool {
    error["message"]
        .as_str()
        .is_some_and(|m| m.contains("No dialog is showing"))
}

/// Helper to join console API arguments into a single string.
/// Chrome's `Runtime.consoleAPICalled` returns arguments as an array of RemoteObjects.
pub fn join_console_args(args: &[Value]) -> String {
    args.iter()
        .map(crate::remote_object::describe)
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_events_keep_only_what_is_read() {
        let request = json!({"method": "Network.requestWillBeSent", "sessionId": "S", "params": {
            "requestId": "1", "type": "Fetch",
            "request": {"url": "https://a.test/x", "method": "POST", "postData": "x".repeat(5000), "headers": {"a": "b"}},
            "initiator": {"stack": {"callFrames": [{"url": "big"}]}},
            "redirectResponse": {"status": 302, "statusText": "Found", "headers": {"location": "/y"}},
            "redirectHasExtraInfo": false,
        }});
        assert_eq!(
            slim_network_event(request),
            json!({"method": "Network.requestWillBeSent", "sessionId": "S", "params": {
                "requestId": "1", "type": "Fetch",
                "request": {"url": "https://a.test/x", "method": "POST", "headers": {"a": "b"}},
                "redirectResponse": {"status": 302, "statusText": "Found", "headers": {"location": "/y"}},
                "redirectHasExtraInfo": false,
            }})
        );
        let response = json!({"method": "Network.responseReceived", "sessionId": "S", "params": {
            "requestId": "1", "type": "Fetch", "hasExtraInfo": true,
            "response": {"status": 200, "statusText": "OK", "headers": {}, "timing": {"sendStart": 1}},
        }});
        assert_eq!(
            slim_network_event(response),
            json!({"method": "Network.responseReceived", "sessionId": "S", "params": {
                "requestId": "1", "hasExtraInfo": true,
                "response": {"status": 200, "statusText": "OK", "headers": {}},
            }})
        );
        let wire = json!({"method": "Network.requestWillBeSentExtraInfo", "sessionId": "S", "params": {
            "requestId": "1", "headers": {"cookie": "sid=1"},
            "associatedCookies": [{"cookie": {"name": "sid"}, "blockedReasons": []}],
        }});
        assert_eq!(
            slim_network_event(wire),
            json!({"method": "Network.requestWillBeSentExtraInfo", "sessionId": "S", "params": {
                "requestId": "1", "headers": {"cookie": "sid=1"},
            }})
        );

        let log = json!({"method": "Runtime.consoleAPICalled", "params": {
            "type": "log", "stackTrace": {"callFrames": []},
            "args": [{"type": "string", "value": "é".repeat(MAX_BUFFERED_STRING_CHARS + 5)}],
        }});
        let slim = slim_console_event(log);
        assert!(slim["params"].get("stackTrace").is_none());
        let kept = slim["params"]["args"][0]["value"].as_str().unwrap();
        assert_eq!(kept.chars().count(), MAX_BUFFERED_STRING_CHARS + 1);
    }

    #[test]
    fn page_errors_list_exceptions_and_console_errors_only() {
        let event = |method: &str, params: Value| json!({"method": method, "params": params});
        let events = [
            event(
                "Runtime.consoleAPICalled",
                json!({"type": "log", "args": [{"type": "string", "value": "fine"}]}),
            ),
            event(
                "Runtime.consoleAPICalled",
                json!({"type": "error", "args": [{"type": "string", "value": "bad thing"}]}),
            ),
            event(
                "Runtime.exceptionThrown",
                json!({"exceptionDetails": {"text": "Uncaught", "exception": {
                "type": "object", "subtype": "error", "description": "TypeError: x is not a function\n    at click (app.js:1:1)"}}}),
            ),
        ];
        assert_eq!(
            page_error_notes(events.iter()),
            vec![
                "[page error] console.error: bad thing".to_string(),
                "[page error] TypeError: x is not a function".to_string(),
            ]
        );

        let many: Vec<Value> = (0..12)
            .map(|i| {
                event(
                    "Runtime.consoleAPICalled",
                    json!({"type": "error", "args": [{"type": "number", "value": i}]}),
                )
            })
            .collect();
        let notes = page_error_notes(many.iter());
        assert_eq!(notes.len(), MAX_PAGE_ERROR_NOTES + 1);
        assert!(notes.last().unwrap().contains("2 more"), "{notes:?}");
    }
    use serde_json::json;

    #[test]
    fn test_parse_connect_timeout_default_when_unset() {
        assert_eq!(
            parse_connect_timeout(None),
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn test_parse_connect_timeout_valid_value() {
        assert_eq!(
            parse_connect_timeout(Some("3")),
            std::time::Duration::from_secs(3)
        );
    }

    #[test]
    fn test_parse_connect_timeout_rejects_zero() {
        // 0 would make every connect fail instantly with a misleading
        // "waiting for approval" error — must fall back to the default.
        assert_eq!(
            parse_connect_timeout(Some("0")),
            std::time::Duration::from_secs(10)
        );
    }

    #[test]
    fn test_parse_connect_timeout_rejects_garbage() {
        assert_eq!(
            parse_connect_timeout(Some("fast")),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            parse_connect_timeout(Some("-5")),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            parse_connect_timeout(Some("")),
            std::time::Duration::from_secs(10)
        );
    }

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

    /// Media emulation and a virtual authenticator are tab state too: a tab
    /// with only one of them must be stashed, or switching away loses it.
    #[test]
    fn media_or_authenticator_alone_is_stashed() {
        let mut saved = std::collections::HashMap::new();
        stash_tab_emulation(
            &mut saved,
            "A".to_string(),
            TabEmulation {
                media: Some(MediaOverride {
                    media: Some("print".into()),
                    features: vec![],
                }),
                ..Default::default()
            },
        );
        stash_tab_emulation(
            &mut saved,
            "B".to_string(),
            TabEmulation {
                webauthn: Some(VirtualAuthenticator {
                    id: "x".into(),
                    options: AuthenticatorOptions {
                        protocol: "ctap2".into(),
                        transport: "internal".into(),
                        has_resident_key: true,
                        has_user_verification: true,
                        is_user_verified: true,
                    },
                }),
                ..Default::default()
            },
        );
        assert!(saved.contains_key("A") && saved.contains_key("B"));
    }

    #[test]
    fn background_is_sent_only_when_set() {
        assert_eq!(
            create_target_params("about:blank", false),
            json!({"url": "about:blank"})
        );
        assert_eq!(
            create_target_params("about:blank", true),
            json!({"url": "about:blank", "background": true})
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

    /// A dialog that a `caffeinate` ping runs into must still reach the next
    /// command, or that command never learns the page is blocked.
    #[tokio::test]
    async fn ping_sets_a_dialog_aside_for_the_next_command() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Answer two pings; an alert opens just before the first answer.
            for n in 0..2 {
                let Some(Ok(Message::Text(req))) = ws.next().await else {
                    return;
                };
                let id = serde_json::from_str::<Value>(&req).unwrap()["id"].clone();
                if n == 0 {
                    let dialog = json!({
                        "method": "Page.javascriptDialogOpening",
                        "sessionId": "S",
                        "params": {"type": "alert", "message": "hi"}
                    });
                    ws.send(Message::Text(dialog.to_string())).await.unwrap();
                }
                let reply = json!({"id": id, "result": {}});
                ws.send(Message::Text(reply.to_string())).await.unwrap();
            }
            // Stay connected until the client hangs up.
            while let Some(Ok(_)) = ws.next().await {}
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();

        client.ping().await.expect("first ping");
        // A later ping must neither use up the set-aside dialog nor spin on it.
        client.ping().await.expect("second ping");

        let error = client
            .send_to_target("S", "DOM.getDocument", json!({}))
            .await
            .expect_err("the dialog must reach the next command on its page");
        assert!(
            error
                .to_string()
                .contains("A javascript dialog is open (alert: hi)"),
            "{error:#}"
        );
        drop(client);
        chrome.await.unwrap();
    }

    /// A browser for one client. Each call it gets is answered with the
    /// messages `answer` makes from it, sent after `answer`'s delay. The task
    /// yields the methods called, in order.
    async fn fake_chrome(
        answer: impl Fn(&Value) -> (std::time::Duration, Vec<Value>) + Send + 'static,
    ) -> (CdpClient, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                calls.push(call["method"].as_str().unwrap_or_default().to_string());
                let (delay, messages) = answer(&call);
                tokio::time::sleep(delay).await;
                for message in messages {
                    if ws.send(Message::Text(message.to_string())).await.is_err() {
                        return calls;
                    }
                }
            }
            calls
        });
        let client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        (client, chrome)
    }

    fn opening(message: &str) -> Value {
        json!({
            "method": "Page.javascriptDialogOpening",
            "sessionId": "S",
            "params": {"type": "alert", "message": message}
        })
    }

    /// A dialog set aside before the rule write is refused by the pre-write
    /// guard: Chrome never got `Network.setBlockedURLs`, so the caller must
    /// be told no write was attempted.
    #[tokio::test]
    async fn a_pre_write_dialog_refusal_reports_no_rule_write() {
        let (mut client, chrome) = fake_chrome(|_call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": _call["id"], "result": {}})],
            )
        })
        .await;
        client.persistent_session = Some("S".into());
        client
            .unread
            .push_back(opening("before the write").to_string());
        let error = client
            .apply_network_rules_reported()
            .await
            .expect_err("the dialog blocks the rule write");
        assert!(
            error.error.downcast_ref::<DialogOpen>().is_some(),
            "{:#}",
            error.error
        );
        assert!(
            !error.write_attempted,
            "a pre-write refusal never reached Chrome"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert!(
            !calls.iter().any(|c| c == "Network.setBlockedURLs"),
            "{calls:?}"
        );
    }

    /// A dialog handler drains the socket while the call it held up finishes.
    /// That call's reply must not be discarded with the unrelated messages, or
    /// it waits out its limit after the dialog is already gone.
    #[tokio::test]
    async fn a_reply_read_while_handling_a_dialog_reaches_its_call() {
        let (mut client, chrome) = fake_chrome(|call| {
            let reply = json!({"id": call["id"], "sessionId": "S", "result": {}});
            let messages = match call["method"].as_str() {
                Some("Runtime.evaluate") => vec![opening("hi")],
                Some("Page.handleJavaScriptDialog") => vec![
                    // The evaluation finishes first. Its reply is not the
                    // handler's, so it has to be kept for the caller.
                    json!({"id": 1, "sessionId": "S", "result": {"value": 42}}),
                    reply,
                ],
                _ => Vec::new(),
            };
            (std::time::Duration::ZERO, messages)
        })
        .await;
        client.dialog_action = Some("accept".into());
        let result = client
            .send_to_target_within(
                "S",
                "Runtime.evaluate",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("the evaluation's reply is not lost");
        assert_eq!(result["value"], 42);
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Runtime.evaluate", "Page.handleJavaScriptDialog"]);
    }

    /// A liveness ping can set aside an opening, its closing and the call's
    /// reply. Reading the opening alone must not fail a call whose dialog is
    /// already gone.
    #[tokio::test]
    async fn a_queued_dialog_close_lets_the_call_read_its_reply() {
        let (mut client, chrome) = fake_chrome(|_| (std::time::Duration::ZERO, Vec::new())).await;
        client.unread.push_back(opening("hi").to_string());
        client.unread.push_back(
            json!({"method": "Page.javascriptDialogClosed", "sessionId": "S",
                   "params": {"result": true, "userInput": ""}})
            .to_string(),
        );
        client
            .unread
            .push_back(json!({"id": 1, "sessionId": "S", "result": {"value": 7}}).to_string());
        let result = client
            .send_to_target_within(
                "S",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("the reply queued with the dialog is returned");
        assert_eq!(result["value"], 7);
        assert!(client.open_dialog.is_none(), "the queued close is applied");
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["DOM.getDocument"]);
    }

    /// Fetch interception belongs to a session. Switching away from a tab that
    /// had mocks must not leave the daemon reading idle events for a tab that
    /// never enabled Fetch.
    #[tokio::test]
    async fn switching_from_a_mocked_session_stops_idle_reads() {
        let (mut client, chrome) = fake_chrome(|call| {
            let result = if call["method"].as_str() == Some("Target.attachToTarget") {
                json!({"sessionId": "S2"})
            } else {
                json!({})
            };
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": result})],
            )
        })
        .await;
        client.persistent_session = Some("S1".into());
        client.persistent_target_id = Some("T1".into());
        client.fetch_may_be_enabled = true;
        client
            .ensure_persistent_session("T2")
            .await
            .expect("switch");
        assert!(
            !client.fetch_may_be_enabled && !client.wants_idle_reads(),
            "the old session's interception is gone"
        );
        drop(client);
        chrome.await.unwrap();
    }

    /// A tab whose detach was refused may still intercept. Ending a later tab
    /// must not stop idle reads while that earlier session is still live.
    #[tokio::test]
    async fn a_failed_detach_keeps_idle_reads_when_a_later_tab_ends() {
        let (mut client, chrome) = fake_chrome(|call| {
            let message = match call["method"].as_str() {
                Some("Target.detachFromTarget") if call["params"]["sessionId"] == "S1" => {
                    json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                }
                Some("Target.attachToTarget") if call["params"]["targetId"] == "T2" => {
                    json!({"id": call["id"], "result": {"sessionId": "S2"}})
                }
                Some("Target.attachToTarget") => {
                    json!({"id": call["id"], "result": {"sessionId": "S3"}})
                }
                _ => json!({"id": call["id"], "result": {}}),
            };
            (std::time::Duration::ZERO, vec![message])
        })
        .await;
        client.persistent_session = Some("S1".into());
        client.persistent_target_id = Some("T1".into());
        client.fetch_may_be_enabled = true;
        client.network_mocks = vec![crate::commands::network_mock::NetworkMock {
            pattern: "*".into(),
            resource_type: None,
            action: crate::commands::network_mock::MockAction::Abort,
            hits: 0,
        }];
        client
            .ensure_persistent_session("T2")
            .await
            .expect("switch to the second tab");
        assert!(client.sessions_may_intercept.contains("S1"));
        client
            .ensure_persistent_session("T3")
            .await
            .expect("switch to the third tab");
        assert!(
            client.wants_idle_reads(),
            "S2's successful detach must not clear S1's interception"
        );
        drop(client);
        chrome.await.unwrap();
    }

    /// The check that the last quiet read ends in is the final one: checking
    /// again would stretch the call by another 15s at most.
    #[tokio::test]
    async fn a_call_at_its_limit_checks_the_browser_once() {
        let (mut client, chrome) = fake_chrome(|call| match call["method"].as_str() {
            Some("Browser.getVersion") => (
                std::time::Duration::from_millis(200),
                vec![json!({"id": call["id"], "result": {}})],
            ),
            // The page never answers.
            _ => (std::time::Duration::ZERO, Vec::new()),
        })
        .await;
        let error = client
            .send_to_target_within(
                "S",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_millis(500),
            )
            .await
            .expect_err("the page never answers");
        assert!(
            error.downcast_ref::<PageUnresponsive>().is_some(),
            "{error:#}"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["DOM.getDocument", "Browser.getVersion"]);
    }

    /// A message the final ping set aside is handled by the owner, which
    /// re-enters the read. That check still counts for the call; otherwise
    /// each queued message earns another liveness ping past the limit.
    #[tokio::test]
    async fn a_queued_message_after_the_final_check_does_not_ping_again() {
        let (mut client, chrome) = fake_chrome(|call| match call["method"].as_str() {
            Some("Browser.getVersion") => (
                std::time::Duration::ZERO,
                vec![
                    json!({"method": "Page.javascriptDialogClosed", "sessionId": "S",
                           "params": {}}),
                    json!({"id": call["id"], "result": {}}),
                ],
            ),
            _ => (std::time::Duration::ZERO, Vec::new()),
        })
        .await;
        let error = client
            .send_to_target_within(
                "S",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_millis(500),
            )
            .await
            .expect_err("the page never answers");
        assert!(
            error.downcast_ref::<PageUnresponsive>().is_some(),
            "{error:#}"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["DOM.getDocument", "Browser.getVersion"]);
    }

    /// `while (true) alert()`: Chrome replies to each answer, then the script
    /// opens the next alert. Answering them all would hold the call forever.
    #[tokio::test]
    async fn dialogs_handled_count_against_the_calls_limit() {
        let (mut client, chrome) = fake_chrome(|call| {
            let reply = json!({"id": call["id"], "sessionId": "S", "result": {}});
            let closed = json!({"method": "Page.javascriptDialogClosed", "sessionId": "S",
                "params": {"result": true, "userInput": ""}});
            let messages = match call["method"].as_str() {
                Some("Runtime.evaluate") => vec![opening("again")],
                Some("Page.handleJavaScriptDialog") => vec![closed, reply, opening("again")],
                Some("Browser.getVersion") => vec![json!({"id": call["id"], "result": {}})],
                _ => Vec::new(),
            };
            (std::time::Duration::ZERO, messages)
        })
        .await;
        client.dialog_action = Some("accept".into());
        let call = client.send_to_target_within(
            "S",
            "Runtime.evaluate",
            json!({}),
            std::time::Duration::from_secs(1),
        );
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("the call ends at its limit")
            .expect_err("the script never returns");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        assert!(
            format!("{error:#}").starts_with(
                "Runtime.evaluate ran past its 1s limit, so this dialog is left open (`close-page` \
                 closes a tab that keeps opening them): A javascript dialog is open (alert: again)"
            ),
            "{error:#}"
        );
        // The next command on the page handles it or fails at once.
        let left_open = client.open_dialog.as_ref().map(|d| d.session_id.as_deref());
        assert_eq!(left_open, Some(Some("S")));
        drop(client);
        chrome.await.unwrap();
    }

    /// A dialog left open by an earlier command is handled within the limit
    /// of the call that finds it, not one of its own.
    #[tokio::test]
    async fn settling_an_open_dialog_counts_against_the_calls_limit() {
        let (mut client, chrome) = fake_chrome(|call| match call["method"].as_str() {
            Some("Browser.getVersion") => (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": {}})],
            ),
            // Neither the answer to the dialog nor the call itself comes back.
            _ => (std::time::Duration::ZERO, Vec::new()),
        })
        .await;
        client.note_dialog_event(&opening("earlier"));
        client.dialog_action = Some("accept".into());
        let call = client.send_to_target_within(
            "S",
            "DOM.getDocument",
            json!({}),
            std::time::Duration::from_secs(1),
        );
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .expect("the call ends at its limit")
            .expect_err("nothing answers");
        assert!(
            error.downcast_ref::<PageUnresponsive>().is_some(),
            "{error:#}"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Page.handleJavaScriptDialog", "Browser.getVersion"]);
    }

    /// A dialog that opened while the daemon was idle reaches whatever reads
    /// next. It holds up its page, not the browser: `new-page` failed on it
    /// although its tab was open, inviting a second one.
    #[tokio::test]
    async fn a_pages_dialog_does_not_fail_a_browser_level_call() {
        let (mut client, chrome) = fake_chrome(|call| {
            let reply = json!({"id": call["id"], "result": {"targetId": "T2"}});
            match call["method"].as_str() {
                Some("Target.createTarget") => {
                    (std::time::Duration::ZERO, vec![opening("idle"), reply])
                }
                _ => (std::time::Duration::ZERO, Vec::new()),
            }
        })
        .await;
        let tab = client.create_target("about:blank", false).await;
        assert_eq!(tab.expect("the tab is open"), "T2");
        // Kept for the page, whose next call fails at once instead of waiting.
        let error = client
            .send_to_target("S", "DOM.getDocument", json!({}))
            .await
            .expect_err("the page's dialog is still open");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Target.createTarget"]);
    }

    /// The events read before the connection dropped are not the whole
    /// window: they came back as if they were, and the daemon, which exits
    /// only on a lost connection, kept the dead one.
    #[tokio::test]
    async fn a_connection_lost_while_collecting_fails_the_collection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let log = json!({"method": "Runtime.consoleAPICalled", "sessionId": "S",
                "params": {"type": "log", "args": []}});
            ws.send(Message::Text(log.to_string())).await.unwrap();
            ws.close(None).await.unwrap();
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();

        let collecting = client.read_events_for(10_000, true);
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), collecting)
            .await
            .expect("the collection ends with the connection")
            .expect_err("a cut-short window is not a result");
        assert!(
            error.downcast_ref::<ConnectionLost>().is_some(),
            "{error:#}"
        );
        chrome.await.unwrap();
    }

    /// Chrome sends each CDP message as one frame, and a page can make one
    /// bigger than tungstenite's 16 MiB default frame limit. Reading it failed,
    /// the daemon took that for a lost connection and exited, and the next
    /// command had to ask the user to approve a new connection.
    #[tokio::test]
    async fn a_message_over_16_mib_still_arrives() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let size = (16 << 20) + 1;
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text("x".repeat(size))).await.unwrap();
            // Stay connected until the client hangs up.
            while let Some(Ok(_)) = ws.next().await {}
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();

        let text = client.read_text().await.expect("read the large message");
        assert_eq!(text.len(), size);
        drop(client);
        chrome.await.unwrap();
    }

    /// wss:// needs a TLS backend and a rustls crypto provider, and Cargo
    /// features choose both. Without the backend, connecting fails with "TLS
    /// support not compiled in"; without a provider, rustls panics. This
    /// server only speaks TCP, so the connect fails either way. What it checks
    /// is that the client's first byte opens a TLS handshake: an error that
    /// comes before one, such as an empty OS trust store, must not pass for it.
    #[tokio::test]
    async fn wss_gets_as_far_as_a_tls_handshake() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut first = [0u8; 1];
            tcp.read_exact(&mut first).await.map(|_| first[0])
        });

        let err = CdpClient::connect(&format!("wss://{addr}/devtools/browser"), "Chrome")
            .await
            .expect_err("a plain TCP server cannot complete a TLS handshake");
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the server task finishes once the client hangs up")
            .unwrap()
            .unwrap_or_else(|e| panic!("the client sent nothing ({e}); connect said: {err:#}"));
        // 0x16 is the TLS record type for a handshake, which the ClientHello opens.
        assert_eq!(first, 0x16, "connect said: {err:#}");
    }

    #[test]
    fn redact_endpoint_hides_keys_but_keeps_where() {
        assert_eq!(
            redact_endpoint("wss://cdp.example/devtools/browser?token=secret"),
            "wss://cdp.example/devtools/browser?***"
        );
        assert_eq!(
            redact_endpoint("wss://user:secret@cdp.example:9222"),
            "wss://***@cdp.example:9222"
        );
        assert_eq!(
            redact_endpoint("wss://cdp.example?apiKey=secret#x"),
            "wss://cdp.example?***"
        );
        // Nothing to hide in a local browser's URL, and its path is what
        // tells one browser session from another.
        let local = "ws://127.0.0.1:9222/devtools/browser/0b7c-uuid";
        assert_eq!(redact_endpoint(local), local);
    }

    #[tokio::test]
    async fn a_failed_connect_does_not_print_the_key() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let err = CdpClient::connect(&format!("ws://127.0.0.1:{port}/?token=secret"), "Chrome")
            .await
            .expect_err("nothing listens on a port whose listener was dropped");
        let msg = format!("{err:#}");
        assert!(!msg.contains("secret"), "{msg}");
        assert!(msg.contains(&format!("127.0.0.1:{port}")), "{msg}");
    }

    /// Setup can fail with an ordinary page error while the detach meant to
    /// clean it up finds the connection gone. The daemon retires a connection
    /// only on a typed `ConnectionLost`, so cleanup's error must win here.
    #[tokio::test]
    async fn a_connection_lost_while_cleaning_up_fails_the_setup() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let id = call["id"].clone();
                match call["method"].as_str().unwrap_or_default() {
                    "Target.attachToTarget" => {
                        ws.send(Message::Text(
                            json!({"id": id, "result": {"sessionId": "S"}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Network.enable" => {
                        // A CDP refusal, not a lost connection.
                        ws.send(Message::Text(
                            json!({"id": id, "error": {"code": -32601, "message": "no"}})
                                .to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Target.detachFromTarget" => {
                        ws.close(None).await.unwrap();
                        return;
                    }
                    other => panic!("unexpected call: {other}"),
                }
            }
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        let error = client
            .ensure_persistent_session("T")
            .await
            .expect_err("setup fails");
        assert!(
            error.downcast_ref::<ConnectionLost>().is_some(),
            "cleanup's lost connection must reach the daemon: {error:#}"
        );
        assert!(
            client.persistent_session.is_none(),
            "the failed session is gone"
        );
        chrome.await.unwrap();
    }

    /// A detach whose write leaves the socket unusable must stop the switch:
    /// attaching a new session on that socket would spend another full write
    /// budget before the daemon learns the connection is gone.
    #[tokio::test]
    async fn a_lost_detach_does_not_attach_on_a_dead_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let method = call["method"].as_str().unwrap_or_default().to_string();
                calls.push(method.clone());
                if method == "Target.detachFromTarget" {
                    ws.close(None).await.unwrap();
                }
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        client.persistent_session = Some("S-old".into());
        client.persistent_target_id = Some("T-old".into());
        let error = client
            .ensure_persistent_session("T-new")
            .await
            .expect_err("a lost detach fails the switch");
        assert!(
            error.downcast_ref::<ConnectionLost>().is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("WebSocket closed by server"),
            "the detach's own error must reach the caller, not a later write \
             failure on the dead socket: {error:#}"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Target.detachFromTarget"]);
    }

    /// A dialog on the persistent session is another page's when this command
    /// ran on a one-off session. It is recorded for that page, and B's call
    /// still gets its reply; B's --dialog-action is not spent on it.
    #[tokio::test]
    async fn another_pages_dialog_does_not_interrupt_the_call() {
        let (mut client, chrome) = fake_chrome(|call| {
            let reply = json!({"id": call["id"], "sessionId": "B", "result": {"value": 7}});
            let opening = json!({"method": "Page.javascriptDialogOpening", "sessionId": "A",
                                 "params": {"type": "alert", "message": "on A"}});
            (std::time::Duration::ZERO, vec![opening, reply])
        })
        .await;
        client.persistent_session = Some("A".into());
        client.persistent_target_id = Some("T1".into());
        client.dialog_action = Some("accept".into());
        let result = client
            .send_to_target_within(
                "B",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("B's call is not held up by A's dialog");
        assert_eq!(result["value"], 7);
        assert_eq!(
            client.open_dialog_for("A").map(|d| d.dialog.message),
            Some("on A".to_string())
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["DOM.getDocument"], "A's dialog was not answered");
    }

    /// A stale session tag on this page must fail the call at once (or the
    /// input would wait out its whole deadline), but must not apply the
    /// call's --dialog-action to a dialog another session has to handle.
    #[tokio::test]
    async fn another_sessions_dialog_does_not_borrow_this_calls_action() {
        let (mut client, chrome) = fake_chrome(|call| {
            let reply = json!({"id": call["id"], "sessionId": "B", "result": {"value": 7}});
            let opening = json!({"method": "Page.javascriptDialogOpening", "sessionId": "A",
                                 "params": {"type": "alert", "message": "on A"}});
            (std::time::Duration::ZERO, vec![opening, reply])
        })
        .await;
        client.dialog_action = Some("accept".into());
        let error = client
            .send_to_target_within(
                "B",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect_err("a dialog on this page pauses the call");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        assert_eq!(
            client.open_dialog_for("A").map(|d| d.dialog.message),
            Some("on A".to_string())
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["DOM.getDocument"], "A's dialog was not answered");
    }

    /// Each session has its own dialog: closing or settling one leaves the
    /// other's record intact, and only that session's dialog is settled.
    #[tokio::test]
    async fn dialogs_are_tracked_per_session() {
        let (mut client, chrome) = fake_chrome(|call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": {}})],
            )
        })
        .await;
        let opening = |session: &str, message: &str| {
            json!({"method": "Page.javascriptDialogOpening", "sessionId": session,
                   "params": {"type": "alert", "message": message}})
        };
        client.note_dialog_event(&opening("A", "a"));
        client.note_dialog_event(&opening("B", "b"));
        assert_eq!(client.open_dialog_for("A").unwrap().dialog.message, "a");
        assert_eq!(client.open_dialog_for("B").unwrap().dialog.message, "b");
        client.note_dialog_event(&json!({
            "method": "Page.javascriptDialogClosed", "sessionId": "B", "params": {}
        }));
        assert!(client.open_dialog_for("B").is_none());
        assert_eq!(client.open_dialog_for("A").unwrap().dialog.message, "a");
        assert_eq!(
            client.open_dialog.as_ref().unwrap().session_id.as_deref(),
            Some("A"),
            "the public mirror still points at a dialog that is open"
        );
        client
            .settle_open_dialog("B")
            .await
            .expect("B has no dialog to settle");
        let error = client
            .settle_open_dialog("A")
            .await
            .expect_err("A's dialog is still open");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        drop(client);
        chrome.await.unwrap();
    }

    /// `css_styles` writes the public field directly. That record must still
    /// be honored by the guarded send and settled by the dialog helpers.
    #[tokio::test]
    async fn a_directly_set_open_dialog_is_still_honored() {
        let (mut client, chrome) = fake_chrome(|call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": {}})],
            )
        })
        .await;
        client.open_dialog = Some(OpenDialog {
            session_id: Some("S".into()),
            dialog: DialogOpen {
                dialog_type: "alert".into(),
                message: "direct".into(),
            },
        });
        assert_eq!(
            client.open_dialog_for("S").unwrap().dialog.message,
            "direct"
        );
        let error = client
            .settle_open_dialog("S")
            .await
            .expect_err("a directly set dialog is still open");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        client.forget_dialog(Some("S"));
        assert!(client.open_dialog.is_none());
        drop(client);
        chrome.await.unwrap();
    }

    /// A `caffeinate` ping may leave the opening in `unread`. It has to be
    /// applied before the page request, without losing an unrelated reply
    /// that was queued ahead of it.
    #[tokio::test]
    async fn a_ping_queued_dialog_is_settled_before_the_page_request() {
        let (mut client, chrome) = fake_chrome(|call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": {}})],
            )
        })
        .await;
        client.dialog_action = Some("accept".into());
        client.awaiting_replies.insert(99);
        client
            .unread
            .push_back(json!({"id": 99, "sessionId": "B", "result": {"value": 1}}).to_string());
        client.unread.push_back(opening("hi").to_string());
        client
            .send_to_target("S", "DOM.getDocument", json!({}))
            .await
            .expect("the queued dialog was settled first");
        assert!(
            client.deferred_replies.iter().any(|(id, _)| *id == 99),
            "the unrelated reply is kept for its owner"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Page.handleJavaScriptDialog", "DOM.getDocument"]);
    }

    /// A queued dialog that refuses the write before it reaches Chrome must
    /// clear input progress. Otherwise the executor turns the skipped write
    /// into a success, inviting a retry that would perform the action twice.
    #[tokio::test]
    async fn a_pre_write_dialog_refusal_clears_input_sent() {
        let (mut client, chrome) = fake_chrome(|call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": {}})],
            )
        })
        .await;
        // A liveness ping set the dialog opening aside for this session.
        client.unread.push_back(opening("queued").to_string());
        client.input_sent = true;
        let error = client
            .send_to_target("S", "Input.insertText", json!({"text": "x"}))
            .await
            .expect_err("the queued dialog blocks the write");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        assert!(
            !client.input_sent,
            "a write that never reached Chrome is not input progress"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert!(
            !calls.iter().any(|c| c == "Input.insertText"),
            "the input was never written: {calls:?}"
        );
    }

    /// A nested dialog whose closing a ping set aside must be consumed before
    /// the handler answers it: otherwise the configured action answers a
    /// dialog that opened after the queued close.
    #[tokio::test]
    async fn a_nested_dialog_uses_its_queued_close_before_answering() {
        let (mut client, chrome) = fake_chrome(|call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": {}})],
            )
        })
        .await;
        let d1 = dialog_from_event(&opening("first"));
        // A ping set aside both the second dialog's opening and its closing.
        client.unread.push_back(opening("second").to_string());
        client.unread.push_back(
            json!({"method": "Page.javascriptDialogClosed", "sessionId": "S", "params": {}})
                .to_string(),
        );
        client
            .handle_dialog_helper(
                Some("S"),
                "accept".into(),
                d1,
                "call",
                tokio::time::Instant::now(),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("the first dialog is answered");
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(
            calls,
            ["Page.handleJavaScriptDialog"],
            "the queued close consumed the nested dialog, so no second answer was sent"
        );
    }

    /// Setup already enabled Network with the short setup budget. Enabling it
    /// again here would put the rest of setup on the ordinary call budget.
    #[tokio::test]
    async fn setup_does_not_re_enable_network_with_the_ordinary_budget() {
        let (mut client, chrome) = fake_chrome(|call| {
            let result = if call["method"].as_str() == Some("Target.attachToTarget") {
                json!({"sessionId": "S"})
            } else {
                json!({})
            };
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": result})],
            )
        })
        .await;
        client
            .ensure_persistent_session("T")
            .await
            .expect("setup succeeds");
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(
            calls,
            [
                "Target.attachToTarget",
                "Network.enable",
                "Runtime.enable",
                "Network.setBlockedURLs"
            ]
        );
    }

    /// A caller's own timeout can drop a call mid-wait, leaving its id
    /// awaited, so its late reply is parked for an owner that is gone. Each
    /// daemon request starts by forgetting both.
    #[tokio::test]
    async fn calls_dropped_mid_wait_are_forgotten_by_the_next_request() {
        let (mut client, chrome) = fake_chrome(|call| {
            let delay = if call["method"] == "Slow.call" {
                300
            } else {
                0
            };
            (
                std::time::Duration::from_millis(delay),
                vec![json!({"id": call["id"], "sessionId": "S", "result": {}})],
            )
        })
        .await;
        let dropped = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            client.send_to_target("S", "Slow.call", json!({})),
        )
        .await;
        assert!(dropped.is_err(), "the caller gave up first");
        client
            .send_to_target_within(
                "S",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("the next call gets its own reply");
        assert_eq!(client.deferred_replies.len(), 1, "the late reply is parked");
        client.forget_abandoned_calls();
        assert!(client.deferred_replies.is_empty());
        assert!(client.awaiting_replies.is_empty());
        drop(client);
        chrome.await.unwrap();
    }

    /// Fire-and-forget writes never register an awaited id, so their replies
    /// must not accumulate in `deferred_replies` for the life of the daemon.
    #[tokio::test]
    async fn replies_to_fire_and_forget_writes_are_not_parked() {
        let (mut client, chrome) = fake_chrome(|call| {
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "sessionId": "S", "result": {}})],
            )
        })
        .await;
        client
            .send_raw_no_wait(Some("S"), "DOM.enable", json!({}))
            .await
            .expect("the write succeeds");
        client
            .send_to_target_within(
                "S",
                "DOM.getDocument",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect("the awaited call gets its own reply");
        assert!(
            client.deferred_replies.is_empty(),
            "{:?}",
            client.deferred_replies
        );
        assert!(client.awaiting_replies.is_empty());
        drop(client);
        chrome.await.unwrap();
    }

    /// A setup call whose read finds the socket gone must return that typed
    /// error at once. A detach written to the same socket would spend the
    /// ordinary call budget and hide why the daemon has to retire it.
    #[tokio::test]
    async fn a_connection_lost_setup_does_not_try_to_detach() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                calls.push(call["method"].as_str().unwrap_or_default().to_string());
                match call["method"].as_str().unwrap_or_default() {
                    "Target.attachToTarget" => {
                        ws.send(Message::Text(
                            json!({"id": call["id"], "result": {"sessionId": "S"}}).to_string(),
                        ))
                        .await
                        .unwrap();
                    }
                    "Network.enable" => {
                        ws.close(None).await.unwrap();
                        return calls;
                    }
                    other => panic!("unexpected call: {other}"),
                }
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        let error = client
            .ensure_persistent_session("T")
            .await
            .expect_err("setup fails with a lost connection");
        assert!(
            error.downcast_ref::<ConnectionLost>().is_some(),
            "{error:#}"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Target.attachToTarget", "Network.enable"]);
    }

    #[tokio::test]
    async fn read_events_without_collect_keeps_pending_events() {
        let (mut client, chrome) = fake_chrome(|_| (std::time::Duration::ZERO, Vec::new())).await;
        let event = json!({"method": "Page.frameNavigated", "params": {"url": "https://a.test"}});
        client.events.push_back(event.clone());
        let collected = client
            .read_events_for(0, false)
            .await
            .expect("a zero-length window reads nothing");
        assert!(
            collected.is_empty(),
            "a non-collecting read returns nothing"
        );
        assert_eq!(
            client.events.front(),
            Some(&event),
            "pending events stay buffered for a collecting drain"
        );
        let collected = client.read_events_for(0, true).await.unwrap();
        assert_eq!(collected, vec![event]);
        assert!(client.events.is_empty());
        drop(client);
        chrome.await.unwrap();
    }

    /// A detach that fails leaves the old tab's interception in place. Its
    /// paused requests must be answered with that tab's mocks, not with the
    /// replacement tab's list once it is loaded.
    #[tokio::test]
    async fn a_paused_request_is_answered_with_its_own_tabs_mocks() {
        use base64::Engine;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let method = call["method"].as_str().unwrap_or_default().to_string();
                let reply = match method.as_str() {
                    "Target.detachFromTarget" => {
                        json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                    }
                    "Target.attachToTarget" => {
                        json!({"id": call["id"], "result": {"sessionId": "S-new"}})
                    }
                    _ => json!({"id": call["id"], "result": {}}),
                };
                calls.push(call);
                if ws.send(Message::Text(reply.to_string())).await.is_err() {
                    break;
                }
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        let mock = |body: &str, status: u16| crate::commands::network_mock::NetworkMock {
            pattern: "*".into(),
            resource_type: None,
            action: crate::commands::network_mock::MockAction::Fulfill {
                status,
                headers: vec![],
                body: body.as_bytes().to_vec(),
            },
            hits: 0,
        };
        client.persistent_session = Some("S-old".into());
        client.persistent_target_id = Some("T-old".into());
        client.network_mocks = vec![mock("old", 200)];
        client.paused_requests.push_back(json!({
            "method": "Fetch.requestPaused",
            "sessionId": "S-old",
            "params": {"requestId": "R", "request": {"url": "https://a.test/"},
                       "resourceType": "Fetch"}
        }));
        client.emulation_saved.insert(
            "T-new".into(),
            TabEmulation {
                network_mocks: vec![mock("new", 500)],
                ..Default::default()
            },
        );
        // Park the detach's refusal so no read runs during it that could
        // answer the old request with the old mocks.
        client.deferred_replies.push_back((
            1,
            json!({"id": 1, "error": {"code": -32000, "message": "busy"}}).to_string(),
        ));
        client
            .ensure_persistent_session("T-new")
            .await
            .expect("the switch succeeds despite the failed detach");
        assert!(
            client.paused_requests.is_empty(),
            "the old tab's request was answered"
        );
        assert_eq!(
            client.network_mocks[0].hits, 0,
            "the new tab's mock was not used"
        );
        assert_eq!(
            client.emulation_saved["T-old"].network_mocks[0].hits, 1,
            "the old tab's mock answered its own request"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        let answered = calls
            .iter()
            .find(|call| call["method"] == "Fetch.fulfillRequest")
            .expect("the paused request was answered");
        assert_eq!(answered["sessionId"], "S-old");
        assert_eq!(answered["params"]["responseCode"], 200);
        assert_eq!(
            answered["params"]["body"],
            base64::engine::general_purpose::STANDARD.encode("old")
        );
    }

    /// A request from an old session can pause only after the daemon has
    /// switched to a new tab (the old detach was refused, so its Fetch
    /// interception is still live). Its answer must come from the old tab's
    /// stashed mocks, not the new tab's, and charge the hit to the old tab.
    #[tokio::test]
    async fn a_late_paused_request_uses_the_mocks_of_its_own_session() {
        use base64::Engine;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let method = call["method"].as_str().unwrap_or_default().to_string();
                let reply = match method.as_str() {
                    "Target.detachFromTarget" => {
                        json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                    }
                    "Target.attachToTarget" => {
                        json!({"id": call["id"], "result": {"sessionId": "S-new"}})
                    }
                    _ => json!({"id": call["id"], "result": {}}),
                };
                calls.push(call);
                if ws.send(Message::Text(reply.to_string())).await.is_err() {
                    break;
                }
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        let mock = |body: &str, status: u16| crate::commands::network_mock::NetworkMock {
            pattern: "*".into(),
            resource_type: None,
            action: crate::commands::network_mock::MockAction::Fulfill {
                status,
                headers: vec![],
                body: body.as_bytes().to_vec(),
            },
            hits: 0,
        };
        client.persistent_session = Some("S-old".into());
        client.persistent_target_id = Some("T-old".into());
        client.network_mocks = vec![mock("old", 200)];
        client.emulation_saved.insert(
            "T-new".into(),
            TabEmulation {
                network_mocks: vec![mock("new", 500)],
                ..Default::default()
            },
        );
        client
            .ensure_persistent_session("T-new")
            .await
            .expect("the switch succeeds despite the failed detach");
        // The old session pauses a request only after the new tab's mocks have
        // been loaded into the active list.
        client.push_event(json!({
            "method": "Fetch.requestPaused",
            "sessionId": "S-old",
            "params": {"requestId": "R", "request": {"url": "https://a.test/"},
                       "resourceType": "Fetch"}
        }));
        client
            .answer_paused_requests()
            .await
            .expect("the late request is answered");

        assert!(client.paused_requests.is_empty());
        assert_eq!(
            client.network_mocks[0].hits, 0,
            "the new tab's mock was not used"
        );
        assert_eq!(
            client.emulation_saved["T-old"].network_mocks[0].hits, 1,
            "the old tab's mock answered its own request"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        let answered = calls
            .iter()
            .find(|call| call["method"] == "Fetch.fulfillRequest")
            .expect("the paused request was answered");
        assert_eq!(answered["sessionId"], "S-old");
        assert_eq!(answered["params"]["responseCode"], 200);
        assert_eq!(
            answered["params"]["body"],
            base64::engine::general_purpose::STANDARD.encode("old")
        );
    }

    /// A pause from the outgoing session can arrive while the new session's
    /// attach is still pending, after the old mocks were stashed and before
    /// `persistent_session` is set. It must still be answered by that tab's
    /// saved mock and charge the hit there.
    #[tokio::test]
    async fn a_pause_during_attach_uses_the_outgoing_sessions_saved_mocks() {
        use base64::Engine;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let method = call["method"].as_str().unwrap_or_default().to_string();
                let reply = match method.as_str() {
                    "Target.detachFromTarget" => {
                        json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                    }
                    "Target.attachToTarget" => {
                        // The old session pauses a request before this attach
                        // has a session of its own or a loaded mock list.
                        let pause = json!({"method": "Fetch.requestPaused", "sessionId": "S-old",
                            "params": {"requestId": "R",
                                "request": {"url": "https://a.test/"}, "resourceType": "Fetch"}});
                        ws.send(Message::Text(pause.to_string())).await.unwrap();
                        json!({"id": call["id"], "result": {"sessionId": "S-new"}})
                    }
                    _ => json!({"id": call["id"], "result": {}}),
                };
                calls.push(call);
                ws.send(Message::Text(reply.to_string())).await.unwrap();
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        client.persistent_session = Some("S-old".into());
        client.persistent_target_id = Some("T-old".into());
        client.network_mocks = vec![crate::commands::network_mock::NetworkMock {
            pattern: "*".into(),
            resource_type: None,
            action: crate::commands::network_mock::MockAction::Fulfill {
                status: 200,
                headers: vec![],
                body: b"old".to_vec(),
            },
            hits: 0,
        }];
        client
            .ensure_persistent_session("T-new")
            .await
            .expect("the switch succeeds despite the failed detach");
        assert!(client.paused_requests.is_empty());
        assert_eq!(
            client.emulation_saved["T-old"].network_mocks[0].hits, 1,
            "the outgoing tab's saved mock answered its own request"
        );
        drop(client);
        let calls = chrome.await.unwrap();
        let answered = calls
            .iter()
            .find(|call| call["method"] == "Fetch.fulfillRequest")
            .expect("the paused request was answered");
        assert_eq!(answered["sessionId"], "S-old");
        assert_eq!(
            answered["params"]["body"],
            base64::engine::general_purpose::STANDARD.encode("old")
        );
    }

    /// A queued closing applies before the same-session `--dialog-action`:
    /// answering a dialog that already closed could hit a replacement one.
    #[tokio::test]
    async fn a_queued_close_is_applied_before_the_dialog_action() {
        let (mut client, chrome) = fake_chrome(|_| (std::time::Duration::ZERO, Vec::new())).await;
        client.dialog_action = Some("accept".into());
        client.unread.push_back(
            json!({"method": "Page.javascriptDialogOpening",
                   "params": {"type": "alert", "message": "already closed"}})
            .to_string(),
        );
        client.unread.push_back(
            json!({"method": "Page.javascriptDialogClosed", "params": {"result": true}})
                .to_string(),
        );
        client
            .unread
            .push_back(json!({"id": 1, "result": {"value": 7}}).to_string());
        let result = client
            .send("DOM.getDocument", json!({}))
            .await
            .expect("the queued close lets the call read its reply");
        assert_eq!(result["value"], 7);
        assert!(client.open_dialog.is_none(), "the queued close was applied");
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(
            calls,
            ["DOM.getDocument"],
            "no action was sent for a dialog that had already closed"
        );
    }

    /// A handler that fails while the held-up call's reply is deferred must
    /// drop that reply: the call exits on the error and nobody will read it.
    #[tokio::test]
    async fn a_failed_call_drops_its_deferred_reply() {
        let (mut client, chrome) = fake_chrome(|call| {
            let messages = match call["method"].as_str() {
                Some("Runtime.evaluate") => vec![opening("hi")],
                Some("Page.handleJavaScriptDialog") => vec![
                    // The evaluation finishes; the handler parks its reply.
                    json!({"id": 1, "sessionId": "S", "result": {"value": 42}}),
                    json!({"id": call["id"], "error": {"code": -32000,
                           "message": "handler broke"}}),
                ],
                _ => Vec::new(),
            };
            (std::time::Duration::ZERO, messages)
        })
        .await;
        client.dialog_action = Some("accept".into());
        let error = client
            .send_to_target_within(
                "S",
                "Runtime.evaluate",
                json!({}),
                std::time::Duration::from_secs(5),
            )
            .await
            .expect_err("the dialog handler fails");
        assert!(format!("{error:#}").contains("handler broke"), "{error:#}");
        assert!(client.awaiting_replies.is_empty());
        assert!(
            client.deferred_replies.is_empty(),
            "{:?}",
            client.deferred_replies
        );
        drop(client);
        let calls = chrome.await.unwrap();
        assert_eq!(calls, ["Runtime.evaluate", "Page.handleJavaScriptDialog"]);
    }

    /// Detaching the session that saw a dialog does not close it. The record
    /// follows its tab, so returning to that tab fails at once (or settles it
    /// with an action) instead of waiting out a call deadline.
    #[tokio::test]
    async fn an_open_dialog_survives_a_session_switch() {
        let (mut client, chrome) = fake_chrome(|call| {
            let result = match call["method"].as_str() {
                Some("Target.attachToTarget") => json!({"sessionId": "S2"}),
                _ => json!({}),
            };
            (
                std::time::Duration::ZERO,
                vec![json!({"id": call["id"], "result": result})],
            )
        })
        .await;
        client.persistent_session = Some("S1".into());
        client.persistent_target_id = Some("T1".into());
        client.note_dialog_event(&json!({
            "method": "Page.javascriptDialogOpening", "sessionId": "S1",
            "params": {"type": "alert", "message": "still open"}
        }));

        client
            .ensure_persistent_session("T2")
            .await
            .expect("switch to the other tab");
        assert!(client.open_dialog_for("S1").is_none(), "S1 detached");
        assert_eq!(client.orphan_dialogs["T1"].message, "still open");
        assert!(client.open_dialog_for("S2").is_none(), "T2 has no dialog");

        client
            .ensure_persistent_session("T1")
            .await
            .expect("return to the first tab");
        assert_eq!(
            client.open_dialog_for("S2").map(|d| d.dialog.message),
            Some("still open".to_string())
        );
        let error = client
            .settle_open_dialog("S2")
            .await
            .expect_err("the dialog is reported at once");
        assert!(error.downcast_ref::<DialogOpen>().is_some(), "{error:#}");
        drop(client);
        chrome.await.unwrap();
    }

    /// A close arriving while the detach is in flight must invalidate the tab's
    /// saved record too: a later session there would otherwise report a dialog
    /// that no longer blocks the page.
    #[tokio::test]
    async fn a_dialog_closed_during_detach_does_not_follow_the_tab() {
        let (mut client, chrome) = fake_chrome(|call| {
            let messages = match call["method"].as_str() {
                Some("Target.detachFromTarget") if call["params"]["sessionId"] == "S1" => vec![
                    json!({
                        "method": "Page.javascriptDialogClosed",
                        "sessionId": "S1",
                        "params": {}
                    }),
                    json!({"id": call["id"], "result": {}}),
                ],
                Some("Target.attachToTarget") if call["params"]["targetId"] == "T2" => {
                    vec![json!({"id": call["id"], "result": {"sessionId": "S2"}})]
                }
                Some("Target.attachToTarget") => {
                    vec![json!({"id": call["id"], "result": {"sessionId": "S3"}})]
                }
                _ => vec![json!({"id": call["id"], "result": {}})],
            };
            (std::time::Duration::ZERO, messages)
        })
        .await;
        client.persistent_session = Some("S1".into());
        client.persistent_target_id = Some("T1".into());
        client.note_dialog_event(&json!({
            "method": "Page.javascriptDialogOpening", "sessionId": "S1",
            "params": {"type": "alert", "message": "closed meanwhile"}
        }));

        client
            .ensure_persistent_session("T2")
            .await
            .expect("switch to the other tab");
        assert!(
            !client.orphan_dialogs.contains_key("T1"),
            "the close during detach invalidated the saved dialog"
        );
        client
            .ensure_persistent_session("T1")
            .await
            .expect("return to the first tab");
        assert!(client.open_dialog_for("S3").is_none());
        client
            .settle_open_dialog("S3")
            .await
            .expect("no stale dialog blocks the page");
        drop(client);
        chrome.await.unwrap();
    }

    /// A failed setup whose Fetch.enable reached Chrome but whose detach did
    /// not complete must keep idle reads on and its mock policy. A setup that
    /// failed before that write must leave no record: nothing can clear it.
    #[tokio::test]
    async fn a_failed_setup_after_fetch_enable_keeps_its_interception() {
        let (mut client, chrome) = fake_chrome(|call| {
            let msg = match call["method"].as_str() {
                Some("Target.attachToTarget") => {
                    json!({"id": call["id"], "result": {"sessionId": "S-new"}})
                }
                Some("Fetch.enable") => {
                    json!({"id": call["id"], "error": {"code": -32000, "message": "refused"}})
                }
                Some("Target.detachFromTarget") => {
                    json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                }
                _ => json!({"id": call["id"], "result": {}}),
            };
            (std::time::Duration::ZERO, vec![msg])
        })
        .await;
        client.emulation_saved.insert(
            "T-new".to_string(),
            TabEmulation {
                network_mocks: vec![crate::commands::network_mock::NetworkMock {
                    pattern: "*".into(),
                    resource_type: None,
                    action: crate::commands::network_mock::MockAction::Abort,
                    hits: 0,
                }],
                ..Default::default()
            },
        );

        let err = client
            .ensure_persistent_session("T-new")
            .await
            .expect_err("setup fails after Fetch.enable");
        assert!(err.to_string().contains("refused"), "{err:#}");
        assert!(client.sessions_may_intercept.contains("S-new"));
        assert_eq!(
            client
                .retained_session_targets
                .get("S-new")
                .map(String::as_str),
            Some("T-new")
        );
        assert_eq!(client.mocks_for_session(Some("S-new")).len(), 1);
        assert!(client.wants_idle_reads());
        drop(client);
        chrome.await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_setup_before_fetch_enable_leaves_no_interception() {
        let (mut client, chrome) = fake_chrome(|call| {
            let msg = match call["method"].as_str() {
                Some("Target.attachToTarget") => {
                    json!({"id": call["id"], "result": {"sessionId": "S-new"}})
                }
                Some("Network.enable") => {
                    json!({"id": call["id"], "error": {"code": -32000, "message": "refused"}})
                }
                Some("Target.detachFromTarget") => {
                    json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                }
                _ => json!({"id": call["id"], "result": {}}),
            };
            (std::time::Duration::ZERO, vec![msg])
        })
        .await;

        let err = client
            .ensure_persistent_session("T-new")
            .await
            .expect_err("setup fails before Fetch.enable");
        assert!(err.to_string().contains("refused"), "{err:#}");
        assert!(!client.sessions_may_intercept.contains("S-new"));
        assert!(!client.wants_idle_reads());
        drop(client);
        chrome.await.unwrap();
    }

    /// After returning to a tab whose old session failed to detach, a late
    /// pause from that session must use its own mock policy. Consuming the
    /// tab's saved mocks for the new session must not let the request through
    /// unmocked.
    #[tokio::test]
    async fn a_late_pause_after_returning_uses_the_old_sessions_mocks() {
        use base64::Engine;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let chrome = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut calls = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let call: Value = serde_json::from_str(&text).unwrap();
                let method = call["method"].as_str().unwrap_or_default().to_string();
                let reply = match method.as_str() {
                    "Target.detachFromTarget" if call["params"]["sessionId"] == "S-old" => {
                        json!({"id": call["id"], "error": {"code": -32000, "message": "busy"}})
                    }
                    "Target.attachToTarget" if call["params"]["targetId"] == "T-old" => {
                        json!({"id": call["id"], "result": {"sessionId": "S-old-2"}})
                    }
                    "Target.attachToTarget" => {
                        json!({"id": call["id"], "result": {"sessionId": "S-new"}})
                    }
                    _ => json!({"id": call["id"], "result": {}}),
                };
                calls.push(call);
                if ws.send(Message::Text(reply.to_string())).await.is_err() {
                    break;
                }
            }
            calls
        });
        let mut client = CdpClient::connect(&format!("ws://{addr}"), "Chrome")
            .await
            .unwrap();
        let mock = |body: &str| crate::commands::network_mock::NetworkMock {
            pattern: "*".into(),
            resource_type: None,
            action: crate::commands::network_mock::MockAction::Fulfill {
                status: 200,
                headers: vec![],
                body: body.as_bytes().to_vec(),
            },
            hits: 0,
        };
        client.persistent_session = Some("S-old".into());
        client.persistent_target_id = Some("T-old".into());
        client.network_mocks = vec![mock("old")];

        client
            .ensure_persistent_session("T-new")
            .await
            .expect("switch despite the failed detach");
        client
            .ensure_persistent_session("T-old")
            .await
            .expect("return to the old tab");
        assert!(!client.emulation_saved.contains_key("T-old"));
        assert_eq!(client.network_mocks[0].hits, 0);

        client.push_event(json!({
            "method": "Fetch.requestPaused", "sessionId": "S-old",
            "params": {"requestId": "R", "request": {"url": "https://a.test/"},
                       "resourceType": "Fetch"}
        }));
        client
            .answer_paused_requests()
            .await
            .expect("the late request is answered");
        assert_eq!(client.network_mocks[0].hits, 1);

        // Removing the mock on the active tab keeps the retained session in
        // step: a subsequent pause from S-old passes through instead of using
        // a stale copy, and no hit is counted.
        client.network_mocks.clear();
        client.push_event(json!({
            "method": "Fetch.requestPaused", "sessionId": "S-old",
            "params": {"requestId": "R2", "request": {"url": "https://a.test/"},
                       "resourceType": "Fetch"}
        }));
        client
            .answer_paused_requests()
            .await
            .expect("removed mock lets the retained session continue");

        // When Chrome later reports the old session detached, its interception
        // state is dropped without clearing another live session's flag.
        client.fetch_may_be_enabled = true;
        client.push_event(json!({
            "method": "Target.detachedFromTarget",
            "params": {"sessionId": "S-old"}
        }));
        assert!(!client.sessions_may_intercept.contains("S-old"));
        assert!(!client.retained_session_targets.contains_key("S-old"));
        assert!(
            client.fetch_may_be_enabled,
            "detaching S-old must not clear the active session's fetch flag"
        );

        drop(client);
        let calls = chrome.await.unwrap();
        let answered = calls
            .iter()
            .find(|call| call["method"] == "Fetch.fulfillRequest")
            .expect("the paused request was mocked");
        assert_eq!(answered["sessionId"], "S-old");
        assert_eq!(
            answered["params"]["body"],
            base64::engine::general_purpose::STANDARD.encode("old")
        );
        assert!(
            calls
                .iter()
                .any(|call| call["method"] == "Fetch.continueRequest"
                    && call["sessionId"] == "S-old"
                    && call["params"]["requestId"] == "R2"),
            "removed mock must continue instead of fulfilling: {calls:?}"
        );
    }
}

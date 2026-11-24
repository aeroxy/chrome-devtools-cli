# Chrome DevTools CLI

High-performance rust CLI that connects to an existing Chrome browser via the DevTools Protocol. Auto-connects by default, no manual WebSocket URL needed.

[![crates.io](https://img.shields.io/crates/v/chrome-devtools-cli.svg)](https://crates.io/crates/chrome-devtools-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)

## Installation

### Homebrew (macOS, recommended)

```bash
brew install aeroxy/tap/chrome-devtools
```

### Cargo

```bash
cargo install chrome-devtools-cli
```

The installed binary is named `chrome-devtools`.

### Build from source

```bash
cargo build --release
# Binary: ./target/release/chrome-devtools
```

### Rust version

Building from source — including `cargo install` — requires **Rust 1.89 or
newer**. The daemon serializes its startup and cleanup with
`std::fs::File::{lock, try_lock}`, stabilized in 1.89; `rust-version` in
`Cargo.toml` makes Cargo say so instead of failing with a type error. The
Homebrew bottle is prebuilt and carries no toolchain requirement.

## Why this exists

Inspired by [chrome-devtools-mcp](https://github.com/ChromeDevTools/chrome-devtools-mcp) — the official MCP server for Chrome DevTools. It works well, but MCP-based browser tools consume a lot of token context: every interaction sends and receives large protocol payloads through the MCP layer.

99% of the time the browser being controlled is the user's own Chrome with their own credentials, so there is no need for a full headless browser stack like Puppeteer or Playwright, and no need for the MCP overhead.

This is a lightweight Rust binary that talks directly to Chrome's DevTools Protocol. One command in, one result out. No separate browser process, no credential handoff, no heavyweight runtime. The agent skill for this tool is a single `SKILL.md` file — the entire context overhead is this documentation.

## Architecture

```
chrome-devtools navigate https://example.com
        │
        ├─ Try daemon (Unix socket $TMPDIR/chrome-devtools-daemon-<uid>-<key>.sock;
        │              loopback TCP on Windows)
        │   └─ If running → send command → get result
        │
        ├─ If no daemon → spawn one (background process)
        │   └─ Daemon connects to Chrome WebSocket (one-time approval)
        │   └─ Listens on Unix socket, 5-min idle timeout
        │
        └─ Fallback → direct WebSocket connection (no daemon)
```

The daemon keeps a persistent WebSocket connection to Chrome, so the browser only prompts for DevTools access once. Subsequent commands reuse the connection.

## Prerequisites

Chrome must have remote debugging enabled:

1. Open Chrome
2. Go to `chrome://inspect/#remote-debugging`
3. Enable the remote debugging server

## Auto-connect

Enable remote debugging once per browser at `chrome://inspect/#remote-debugging`
(Edge: `edge://inspect/#remote-debugging`). It applies to the running process
immediately and persists across restarts — it is stored in the profile's
`Local State` as `devtools.remote_debugging.user-enabled`, so no launch flag is
needed. The toggle is per-browser: enabling it in Chrome does not enable it in
Edge. The `--remote-debugging-port` flag is the alternative, and is meant for a
throwaway instance rather than your everyday browser.

By default, the CLI reads `DevToolsActivePort` from Chrome's user data directory:

| OS | Default path |
|----|-------------|
| macOS | `~/Library/Application Support/Google/Chrome/` |
| Linux | `~/.config/google-chrome/` |
| Windows | `%LOCALAPPDATA%\Google\Chrome\User Data\` |

Override with `--user-data-dir`, `--browser` (chrome/edge), `--channel` (beta/canary/dev), `--ws-endpoint`, or `--port`. All five also read from environment variables:

| Environment Variable | Corresponding Flag |
|----------------------|--------------------|
| `CHROME_WS_ENDPOINT` | `--ws-endpoint` |
| `CHROME_PORT` | `--port` |
| `CHROME_USER_DATA_DIR` | `--user-data-dir` |
| `CHROME_BROWSER` | `--browser` |
| `CHROME_CHANNEL` | `--channel` |

`--ws-endpoint` also takes `wss://`, for a browser that runs somewhere else. See [Remote browser: the Kitesurf playground](#remote-browser-the-kitesurf-playground).

### Unreadable profile directory

Remote debugging can be on while the CLI still cannot read `DevToolsActivePort`, because the OS will not let it into the profile directory. On macOS this happens until the app running the CLI (your terminal or IDE) is allowed to access data from other apps: macOS shows a privacy prompt, and until it is approved every read fails with "Operation not permitted". Security software and file permissions can do the same.

When that happens to the default profile, the CLI tries the `chrome://inspect` server on port 9222 instead, and prints a warning saying so when it connects there. If the server shown at `chrome://inspect/#remote-debugging` uses a different port, pass it with `--port` (or set `CHROME_PORT`). Connecting by port skips the profile directory, and the warning with it:

```bash
chrome-devtools --port 9222 list-pages
export CHROME_PORT=9222   # later commands connect by port too
```

How `--port` and the fallback behave:

- `--port` works with both kinds of local server. It first asks the port for `/json/version`. A browser launched with `--remote-debugging-port` answers with its browser endpoint, and the CLI uses that. The `chrome://inspect` server answers 404, and the CLI connects to `/devtools/browser`, which that server accepts without the per-session UUID stored in `DevToolsActivePort`.
- The fallback only takes a `chrome://inspect` server. A browser launched with `--remote-debugging-port` on 9222 is a different instance, since Chrome ignores that flag on the default profile, so the fallback refuses it; pass `--port 9222` if it is the one you want.
- An explicit `--user-data-dir` that cannot be read is an error, not a fallback: it usually names a separate instance, and 9222 would reach your everyday browser instead.
- A daemon connected to the `chrome://inspect` server by port has no session UUID in its key. If the browser restarts while that daemon is alive, the next command fails with a WebSocket error and the daemon exits; the command after that connects afresh, and the browser asks for approval again.

### Microsoft Edge

Edge is Chromium and speaks the same DevTools Protocol, so every command works against it. Pass `--browser edge` to auto-connect to Edge's profile instead of Chrome's:

| OS | Default path |
|----|-------------|
| macOS | `~/Library/Application Support/Microsoft Edge/` |
| Linux | `~/.config/microsoft-edge/` |
| Windows | `%LOCALAPPDATA%\Microsoft\Edge\User Data\` |

`--channel` composes with it (`--browser edge --channel beta`). Edge Canary is not distributed for Linux, and that combination is rejected rather than pointed at a directory that cannot exist. An explicit `--ws-endpoint` or `--user-data-dir` already says where to connect, so `--browser` is not needed to *reach* Edge. Pass it anyway when you are targeting Edge: it is also the label recorded for the daemon (shown by `list-daemons`) and the browser named in connection errors. Without it both say Chrome, the default, whatever the endpoint actually points at.

Enterprise-managed Edge can have remote debugging disabled by policy; `DevToolsActivePort` then never appears and auto-connect fails with the message above. That is the same failure mode as Chrome under the equivalent policy, just more common on managed fleets.

### Remote browser: the Kitesurf playground

[Kitesurf](https://kitesurf.dev) is Cloudflare's experimental browser engine. It runs inside Workers, speaks the DevTools Protocol, and has a public playground that needs no account. Kitesurf's docs connect `chrome-devtools-mcp` to it. This CLI connects to it with `--ws-endpoint`, with no MCP server and no Chrome on your machine, so there is no consent dialog either:

```bash
export CHROME_WS_ENDPOINT=wss://kitesurf.dev/devtools/browser

# Kitesurf opens new tabs on about:blank, so navigate the tab new-page names
chrome-devtools new-page https://example.com
# Opened new page: https://example.com (target: steep-lynx)
chrome-devtools --target steep-lynx navigate https://example.com

# The daemon holds the connection, so the tab is still there for every command
chrome-devtools --target steep-lynx read-page                        # page as markdown
chrome-devtools --target steep-lynx screenshot --output example.png  # saved locally

# WebMCP works through page JavaScript: register a tool, then list and call it
chrome-devtools --target steep-lynx evaluate 'document.modelContext.registerTool({name: "add", description: "Add two numbers", inputSchema: {type: "object"}, execute: ({a, b}) => String(a + b)})'
chrome-devtools --target steep-lynx list-3p-tools
chrome-devtools --target steep-lynx execute-3p-tool add '{"a": 1, "b": 2}'
# Executed 'add' via WebMCP:
# "3"
```

- **Keep the endpoint on every command**, through `CHROME_WS_ENDPOINT` as above or `--ws-endpoint`. A command with neither auto-connects to your local browser instead.
- **The daemon is the session.** Each connection gets a fresh browser, so tabs last as long as the daemon holding the connection: 5 minutes idle, or as long as `caffeinate` runs. For a second, separate browser, vary the endpoint, for example `wss://kitesurf.dev/devtools/browser?s=2`. It gets its own daemon, and Kitesurf ignores the query string.
- **The playground is public.** Every page you open passes through Cloudflare, so open only public URLs, and don't sign in to anything there.
- The connection is made directly; `HTTP(S)_PROXY` is not used.

## Page targeting

Every page-level command outputs a friendly target name like `[target:red-snake]`. This is a deterministic word-pair derived from Chrome's internal target ID — same page always gets the same name.

```bash
# Navigate — note the target name
chrome-devtools navigate https://example.com
# Navigated to https://example.com
# [target:red-snake]

# Pin subsequent commands to the same page
chrome-devtools --target red-snake screenshot --output /tmp/page.png
chrome-devtools --target red-snake evaluate "document.title"
```

Without `--target`, commands default to page index 0, which may vary as Chrome reorders tabs. Always capture and reuse the target name.

`list-pages` shows all pages with their friendly names:

```
[0] (green-dog) My App — https://localhost:3000
[1] (red-snake) Example Domain — https://example.com
[2] (bold-stag) GitHub — https://github.com
```

You can also use `--page <index>` for quick one-offs, or pass the raw hex target ID.

## Commands

### Navigation

| Command | Description |
|---------|-------------|
| `navigate <url>` | Go to URL (waits for load) |
| `navigate --back` | Go back in history (an entry an iframe's navigation made moves only that iframe) |
| `navigate --forward` | Go forward |
| `navigate --reload` | Reload page |
| `new-page <url>` | Open new tab |
| `new-page <url> --background` | Open new tab without bringing it to the front (the tab the user is looking at keeps focus) |
| `close-page <index>` | Close tab by index |
| `select-page <index>` | Bring tab to front |
| `list-pages` | List all open tabs |

`navigate` and `new-page` accept atomic emulation flags (`--viewport`, `--mobile`, `--device-scale-factor`, `--geolocation`, `--accuracy`) and `--extra-headers` for custom HTTP headers, so you can navigate and emulate in a single call.

### Inspection

| Command | Description |
|---------|-------------|
| `screenshot --output <path>` | Save screenshot to file |
| `screenshot --full-page` | Capture full scrollable page |
| `screenshot --max-width <px> --max-height <px>` | Downscale screenshot to fit within dimensions |
| `read-page` | Read page content as clean Markdown (extracts main article) |
| `read-page --output <path>` | Save Markdown to file |
| `evaluate <expr> [--dialog-action <action>]` | Run JavaScript (optionally handle dialogs: accept, dismiss, or prompt text) |
| `snapshot` | Accessibility tree dump |
| `css-styles <selector> [--computed <prop>,...] [--ua-rules]` | CSS for the first matching element: computed values, matched rules (normal precedence order, highest first; `[overridden]` declarations tagged, `var()` values resolved, `file:line` sources), inline style, inherited rules |
| `take-heapsnapshot --output <path>` | Capture V8 heap snapshot (streamed via CDP) |
| `compare-heapsnapshots --base <path> --current <path> [--class-index N]` | Diff two snapshots per class, grouped as in the DevTools Memory panel; `--class-index` lists one class's added/deleted objects (offline) |
| `inspect-heapsnapshot-node --file-path <path> --node-id <id>` | Show a node's class, detachedness, direct retainers and a retaining path from the root, shortest through the page's roots before the GC roots (offline) |

The offline heap commands page their rows with `--limit` (default 50) and `--offset`, say how many rows there are in total, and cut names longer than `--max-name-length` (default 100) characters.

For `compare-heapsnapshots`, both captures must come from the same uninterrupted daemon session on the same tab: moving to another tab, letting the daemon exit, or another debugger detaching resets V8's heap IDs, so the diff can silently pair unrelated objects. See the [heap-debugging guide](skill/chrome-devtools/SKILL.md#pattern-15-memory-leak-debugging-heap-snapshots) for the events that invalidate a snapshot series.

### Interaction

| Command | Description |
|---------|-------------|
| `click <selector>` | Click element by CSS selector (fails, naming it, when another element covers it) |
| `click-at <x> <y>` | Click at specific coordinates |
| `fill <selector> <value>` | Fill input field, dropdown (`<select>`), or toggle checkbox/radio (`"true"`/`"false"`). On a file input, the value is a local file path (relative to where you run the CLI), and that file is selected on the input as if picked in the file dialog, one per call |
| `type-text <text> [--submit-key <key>]` | Type into focused element (optionally press key after) |
| `press-key <key>` | Press key (e.g. `Enter`, `Control+A`) |
| `hover <selector>` | Hover over element (fails, naming it, when another element covers it) |

### Third-party developer tools

| Command | Description |
|---------|-------------|
| `list-3p-tools` | List developer tools the page exposes to agents |
| `execute-3p-tool <name> <params>` | Execute a tool by name with a JSON params string |

These commands ask the page through WebMCP first (`document.modelContext.getTools()` / `executeTool()`), which needs a browser that has it: Chrome with `--enable-features=WebMCP`, or Kitesurf. WebMCP reports the tools of the page's own origin, same-origin iframes included, but not those of cross-origin iframes. `list-3p-tools` falls back to the legacy `window.__dtmcp.toolGroup` only when the browser has no WebMCP, so in a WebMCP browser a page that only sets `window.__dtmcp` lists nothing. `execute-3p-tool` also falls back to `window.__dtmcp.executeTool` when no WebMCP tool has the name.

### Other

| Command | Description |
|---------|-------------|
| `emulate` | Get/set emulation overrides (viewport, geolocation, CSS media, URL blocking) |
| `emulate --viewport 1280x720` | Set viewport size (per-tab, persists across navigation) |
| `emulate --geolocation 37.77,-122.41` | Set geolocation (per-tab, persists across navigation) |
| `emulate --media print` | Emulate the CSS media type (`print` or `screen`) |
| `emulate --media-feature prefers-color-scheme=dark` | Emulate a CSS media feature (repeatable): `prefers-color-scheme`, `prefers-reduced-motion`, `prefers-reduced-transparency`, `prefers-contrast`, `forced-colors`, `color-gamut` |
| `emulate --clear-media` | Clear CSS media emulation |
| `emulate --block-url <pattern>` | Block URL pattern on subsequent requests (glob; per-tab) |
| `emulate --unblock-url <pattern>` | Un-block a previously blocked pattern |
| `emulate --clear-blocks` | Clear all blocked URL patterns |
| `emulate --clear-all` | Clear all overrides (viewport, geolocation, CSS media, blocks) |
| `configure-webauthn` | Add a virtual WebAuthn authenticator (ctap2, internal, passkey-capable) so `navigator.credentials.create()`/`get()` work without a device; again with no flags shows it and its credentials |
| `configure-webauthn --protocol u2f --transport usb` | Choose the authenticator (`--protocol ctap2\|u2f`, `--transport internal\|usb\|nfc\|ble\|hybrid`, `--has-resident-key true\|false`, `--has-user-verification true\|false`, `--is-user-verified true\|false`); u2f can't use hybrid, resident keys or user verification |
| `configure-webauthn --remove` | Remove the virtual authenticator |
| `wait-for <text> [--timeout ms]` | Wait for text to appear (default 30s) |

`emulate` with no flags shows the active tab's overrides. Viewport, geolocation, CSS media, and URL blocks are all **per-tab** — each page keeps its own. They persist across navigation within that tab and do **not** leak to other tabs, so you can hold (say) a mobile viewport with images blocked on one tab and a desktop baseline on another at the same time. They persist until you clear them (`--clear-viewport`, `--clear-geolocation`, `--clear-media`, `--clear-blocks`, or `--clear-all`), the tab closes, or the daemon exits. `--media-feature` values add to the ones already set (setting a feature again replaces its value), and unknown names or values are rejected before anything is applied, because Chrome would accept and ignore them. CSS media needs the daemon's session for the tab: without the daemon, or when it could not attach, `--media` and `--media-feature` fail rather than set an override that ends with the command. When it could not attach, the tab's saved media override is shown as not applied, and `--clear-media` still clears it.

The virtual authenticator lives on the daemon's session for the tab, because Chrome removes it when the session that added it detaches; it therefore lasts across commands on that tab until `--remove`, the tab closes, or the daemon exits. When the CLI works on another tab and comes back, the authenticator is re-created with the same options but without the credentials registered before. Chrome only answers WebAuthn calls from the front tab, so `select-page` the tab first; `configure-webauthn` says so when the tab is in the background. Changing `--protocol`, `--transport`, `--has-resident-key`, or `--has-user-verification` replaces the authenticator and deletes its credentials; `--is-user-verified` changes in place.

### Network and console inspection

The daemon keeps a persistent CDP session on the active page that continuously collects Network and Runtime events. `console` and `network` **drain** whatever has accumulated since the last call (or since the session attached, if never drained).

| Command | Description |
|---------|-------------|
| `console` | Drain accumulated console messages |
| `console --type error --type warning` | Filter by level (`log`, `warn`, `info`, `error`, `debug`, `exception`) |
| `console --duration 5000` | Live collection for 5 s (consumes events — they won't reappear on a later drain) |
| `network` | Drain accumulated network requests |
| `cookies [--url <url>]... [--name <name>]...` | The cookies Chrome stores for the page's URL (or each `--url`), HttpOnly and partitioned ones included (a lookup by the page URL may omit partitioned cookies set by a cross-site iframe), read over the daemon's approved connection: name, domain, path, expiry, flags and value length, values hidden. A lookup by URL: what one request sent, after `SameSite` rules, is in `network --request-headers` |
| `cookies --header` | The same cookies as one `Cookie` request header (`name=value; ...`), values included, for one URL (the page's or a single `--url`); `--json` / `--toon` print every field |
| `network --type Fetch --type XHR` | Filter by resource type (`Document`, `Script`, `Stylesheet`, `Image`, `Font`, `XHR`, `Fetch`, `Manifest`, `Media`, `Other`) |
| `network --duration 5000` | Live collection for 5 s |
| `network --request-headers --response-headers` | Also show each request's headers (`>` lines) and each response's (`<` lines): what went over the wire, `Cookie` and `Set-Cookie` included, when Chrome reports it, else what the page sees; one set per redirect hop; `requestHeaders` / `responseHeaders` in `--json` |
| `sw-logs [--duration 2000]` | Collect console logs from extension service workers (2 s default) |
| `sw-logs --extension-id <id>` | Filter service-worker logs to one extension |

A drain without a `--duration` returns instantly. Adding `--duration N` switches the command to *live mode* and blocks for `N` ms.

Other page commands list the errors the page raised while they ran (uncaught exceptions and `console.error`, first line each, at most 10) on stderr as `[page error] …`, without draining them.

### Network mocks

| Command | Description |
|---------|-------------|
| `mock-network <url-glob> [--status N] [--header "Name: value"] [--body <text> \| --body-file <path>]` | Answer matching requests on this tab from the CLI instead of the network (`--body-file`: a regular file, up to 64 MiB; body text that starts with a dash goes attached, as in `--body=-1`) |
| `mock-network <url-glob> --abort` | Fail matching requests with a network error |
| `mock-network <url-glob> ... --resource-type <type>` | Only requests of one type (`Document`, `Script`, `Image`, `XHR`, `Fetch`, ...; Chrome reports `fetch()` as XHR when it intercepts, so `XHR` and `Fetch` each match both) |
| `mock-network` | List the tab's mocks with hit counts |
| `remove-network-mock [<url-glob>]` | Remove all mocks with this glob (including mocks of different resource types), or all of them on the tab when no glob is given |

Globs use `*` for any characters and `?` for exactly one. The latest matching mock wins, and the HTTP cache is off while any is set so cached responses can't bypass it. A service worker can answer a matching request from its cache before Fetch interception sees it, so mocks may not apply to a page it controls. Mocks apply to requests on the tab's own session; cross-site iframe requests run in another process and are not paused. Mocks live on the daemon's session for the tab: they pause while the CLI works on another tab, and end when the daemon exits. A daemon with mocks keeps reading from the browser between commands, since a request waits for its answer, so it stays up while the page keeps making requests.

### Daemon

| Command | Description |
|---------|-------------|
| `kill-daemon` | Stop the background daemon cleanly |
| `caffeinate` | Keep the daemon's approved connection alive until it is lost |

On Unix, `kill-daemon` signals the targeted daemon with `SIGTERM`, removes its socket, info and PID files, and exits. It's a no-op when the target endpoint resolves but has no daemon attached. It is *not* a no-op when the endpoint cannot be resolved at all — a scoped kill needs an endpoint to identify its daemon, so if `DevToolsActivePort` is missing (the browser has already exited, say) it fails and says so; use `--all` there, which needs no endpoint. Prefer this over `pkill -f __daemon__` — the process name is shared by legitimate Chrome children processes. On Windows it is not supported: it prints that and exits without signalling anything or removing any files (see **Kill (Windows)** below).

`caffeinate` checks the daemon's browser connection every 10 seconds and runs until you stop it (Ctrl-C). Each check also restarts the daemon's idle timer, so an approved connection stays open with no fixed end, and the daemon already running needs no restart (unlike a new `DAEMON_IDLE_TIMEOUT_SECS`). When the connection is lost, it exits with an error that says "Lost the connection to <browser>": exit code 9, or 6 if the daemon had already exited (stopped with `kill-daemon`, say). It never reconnects on its own, because a new connection can put the approval prompt back up with nobody there to answer it; only its first check starts a daemon, if none is running. Pass it the same `--browser`/`--user-data-dir`/`--port` flags as your other commands, since it keeps only that endpoint's daemon alive. It does not keep the computer awake, and a sleep can drop the connection.

## Global options

| Flag | Description |
|------|-------------|
| `--target <name>` | Target page by friendly name or raw ID |
| `--page <index>` | Target page by index |
| `--json` | JSON output |
| `--toon` | TOON output (compact tabular encoding for LLM agents; mutually exclusive with `--json`) |
| `--block-url <pattern>` | Add a URL pattern to the active tab's block list (repeatable; persists until un-blocked or cleared) |
| `--unblock-url <pattern>` | Remove a URL pattern from the active tab's block list (repeatable) |
| `--ws-endpoint <url>` | Explicit WebSocket URL, `ws://` or `wss://` |
| `--port <n>` | Port of a local DevTools server, e.g. the one shown at `chrome://inspect` (skips reading the profile directory) |
| `--user-data-dir <path>` | Custom browser profile directory |
| `--browser <name>` | Browser to auto-connect to (chrome/edge) |
| `--channel <ch>` | Browser release channel (stable/beta/canary/dev) |

Commands: `list-daemons` shows every daemon this user has on-disk state for — running ones and, on Unix, stale entries left by a daemon that died without cleaning up; `kill-daemon [--all]` stops one or all.

Global `--block-url` and `--unblock-url` update the **active tab's** block list and apply via `Network.setBlockedURLs`; the daemon re-applies each tab's list when that tab is in use, so blocking is isolated per tab. **Note:** Chrome only blocks *subresources* (images, scripts, fetch/XHR, stylesheets, CDN, trackers, fonts). The top-level navigation document itself is never blocked — e.g. `--block-url "*example.com*"` then `navigate https://example.com` still loads the page, but any `*.png`, `*.woff2`, etc. subresources on it are blocked.

## Daemon details

- **Instance identity**: one daemon per browser endpoint. The socket/PID/info filenames carry a 16-hex-digit key derived from the resolved `ws://` URL, so Chrome, Edge, every channel and every headless instance get a daemon of their own, and a command aimed at one browser can never be served by a daemon attached to another. The URL's browser GUID changes on every browser launch, so a restarted browser gets a fresh daemon instead of inheriting a dead connection; the orphan exits on its idle timeout.
- **Endpoint (Unix)**: socket at `$TMPDIR/chrome-devtools-daemon-<uid>-<key>.sock` (uid-suffixed so users on a shared machine don't collide)
- **Endpoint (Windows)**: loopback TCP listener; its address is written to `%TEMP%\chrome-devtools-daemon-<key>.addr` (`%TEMP%` is already per-user, so there is no uid suffix — but the instance key is still present, since one daemon per endpoint applies on every platform)
- **PID file**: `$TMPDIR/chrome-devtools-daemon-<uid>-<key>.pid` (Windows: `%TEMP%\chrome-devtools-daemon-<key>.pid`)
- **Info file**: `$TMPDIR/chrome-devtools-daemon-<uid>-<key>.info` — JSON naming the browser, endpoint, PID and start time, so `list-daemons` can label rows. Best-effort: a daemon with no info file still lists, with `?` columns.
- **Lock file**: `$TMPDIR/chrome-devtools-daemon-<uid>.lock` (Windows: `%TEMP%\chrome-devtools-daemon.lock`) — **not** keyed per instance: one lock covers all of them, because it only serializes the brief write-pid-then-bind window, while a per-instance lock would accumulate a never-removed file per browser session. Serializes daemon startup/cleanup; intentionally never removed automatically. Locks bind to the inode, not the name: deleting the file while any daemon process is still starting, running, or shutting down lets a new process lock a fresh replacement inode and bypass the serialization entirely. Only delete it once no daemon process exists at all — and there's rarely a reason to, since a leftover lock file is harmless.
- **Idle timeout**: 5 minutes by default (auto-exits, cleans up its files). `DAEMON_IDLE_TIMEOUT_SECS` changes it — e.g. `3600` keeps an approved connection alive through gaps of up to an hour between commands, so a break doesn't cost a fresh approval. The daemon takes it from the environment of the command that spawns it, so a new value applies to the next daemon, not one already running. `0` and non-numbers fall back to the default. A longer timeout also keeps that connection usable without a prompt, by anything running as you, for longer. To keep the daemon that is already running alive, with no fixed end, run `caffeinate` instead.
- **Cleanup**: endpoint + PID files are also removed on panics, and on Unix on SIGTERM/SIGINT; Windows Ctrl-C cleanup is best-effort only (a background daemon has no console to receive it). SIGQUIT, SIGHUP and SIGKILL skip cleanup by design — the leftover files are harmless and are reclaimed by the next daemon start.
- **Protocol**: Length-prefixed JSON over the Unix socket / loopback TCP. Responses carry the daemon's version, and the CLI warns when it differs from its own (an older daemon keeps serving after an upgrade until it idles out).
- **Peer check (Unix)**: daemon and CLI each check that the other end of the socket runs as the same user, since on Linux the socket sits in the shared `/tmp`.
- **Call timeouts**: a CDP call on a page gives up after 180 s (`CHROME_CDP_TIMEOUT_SECS`, read when the daemon starts), counted from before the request is written, so one stuck page can't hold the daemon for every other caller. A timed-out `Runtime.*` call asks Chrome to stop its JavaScript (`Runtime.terminateExecution`), but Chrome does not confirm the stop; other calls make no such request. Writing that stop request has its own 10 s budget (`SETUP_CALL_TIMEOUT`), starting after the original call's deadline and liveness check; a write that doesn't finish retires the connection. That budget is separate from the 180 s limit on the original call. The JavaScript dialogs a call answers (`--dialog-action`) count against its limit too: one that opens after it is left open, and the call fails. Calls that set a session up — the domain enables and blocklist write — give up after 10 s (restoring saved CSS media, WebAuthn, or mocks on a returning tab uses the ordinary call limit). When the socket is quiet for 30 s mid-call, and once more when a call's limit runs out, the daemon pings the browser; no answer within 15 s counts as a lost connection and the daemon exits, so a call can run up to 15 s past its limit, in addition to the bounded write of a termination request. `caffeinate` uses the same bounded check, and a write to the browser that doesn't finish within 180 s also counts as a lost connection.
- **Spawned by**: First CLI invocation for a given endpoint (transparent to user)
- **List**: `chrome-devtools list-daemons` — PID, browser, endpoint, uptime and state for every daemon this user owns. `--json` (or `--toon`) for machine-readable output. Reads only on-disk state, so it works when every browser has exited. On Unix, rows whose PID no longer exists are marked `stale`; on Windows liveness is not probed, so every row's state is `?` and `stale` never appears.
- **Kill**: `chrome-devtools kill-daemon` stops only the daemon for the endpoint its flags resolve to, so `--browser edge kill-daemon` cannot stop your Chrome daemon. Add `--all` to stop every daemon for this user — which is also the only way to clear one whose browser has already exited, since a scoped kill has no endpoint left to resolve (it fails and says so). `--all` also sweeps the pre-key `chrome-devtools-daemon-<uid>.pid` name left by older versions. (Or delete the socket + PID file by hand; leave the lock file — see above.) It sends SIGTERM and returns once the signal is delivered, not once the process is gone: the daemon exits *between* requests, so one that is mid-command finishes it and answers that client first. Expect up to one command's latency, and note that a daemon wedged inside a CDP call outlives the command that stopped it.
- **Kill (Windows)**: not supported — `kill-daemon` says so and exits, and a backgrounded daemon has no console for Ctrl-C. Run `chrome-devtools list-daemons` to get the PID of the daemon you want (there may be several, one per endpoint) and pass it to `taskkill /PID <pid>`, or wait out the idle timeout. Reading `%TEMP%\chrome-devtools-daemon-<key>.pid` directly works too, but only if you already know which key you want.

The daemon keeps a persistent CDP session on the current page to:
- Continuously collect `Network.*` and `Runtime.consoleAPICalled`/`exceptionThrown` events for `console` and `network` drains.
- Re-apply `Network.setBlockedURLs` and emulation state across page-level commands.
- Re-attach to a new target when `--target` changes (the previous target's event buffers are discarded on the switch).

## Source layout

```text
src/
├── main.rs           # Entry point + daemon dispatch
├── lib.rs            # CLI (clap) + command routing
├── cdp.rs            # Raw CDP over WebSocket (JSON-RPC) + persistent session
├── browser.rs        # Auto-connect (DevToolsActivePort, --port fallback)
├── daemon.rs         # Background daemon (persistent connection)
├── client.rs         # Talks to daemon via Unix socket
├── protocol.rs       # IPC message types (DaemonRequest / DaemonResponse)
├── remote_object.rs  # Console values and exceptions as DevTools prints them
├── friendly.rs       # Target ID → word-pair names
├── format.rs         # OutputFormat (text/json/toon) + format_structured helper
├── result.rs         # Command result types
├── error.rs          # CLI error types and codes
├── constants.rs      # Shared constants
├── telemetry.rs      # Logging and telemetry
└── commands/
    ├── mod.rs
    ├── navigate.rs
    ├── pages.rs      # list/new/close/select/wait-for
    ├── screenshot.rs
    ├── snapshot.rs
    ├── read_page.rs  # read-page (Readability extraction + HTML→Markdown)
    ├── memory.rs     # take-heapsnapshot (CDP streaming) + compare-heapsnapshots / inspect-heapsnapshot-node (offline)
    ├── evaluate.rs
    ├── executor.rs   # Command dispatch + persistent-session reuse
    ├── input.rs      # click/fill/type/press/hover
    ├── emulation.rs  # emulate (viewport/geolocation/CSS media/blocklist get/set/clear)
    ├── css.rs        # css-styles (matched rules, cascade status, var() resolution, computed)
    ├── webauthn.rs   # configure-webauthn (virtual authenticator on the persistent session)
    ├── console.rs    # console drain / live collection
    ├── cookies.rs    # cookies (Network.getCookies over the page's session)
    ├── network.rs    # network drain / live collection
    ├── network_mock.rs # mock-network / remove-network-mock (Fetch domain)
    ├── sw_logs.rs    # extension service-worker log collection
    └── third_party.rs # list-3p-tools/execute-3p-tool
```

## Typical workflow

```bash
# 1. Navigate — capture the [target:name]
chrome-devtools navigate https://example.com
# [target:red-snake]

# 2. Understand the page
chrome-devtools --target red-snake snapshot
chrome-devtools --target red-snake screenshot --output /tmp/page.png

# 3. Interact
chrome-devtools --target red-snake fill "#email" "user@example.com"
chrome-devtools --target red-snake click "#submit"

# 4. Extract data
chrome-devtools --target red-snake evaluate "document.title"
chrome-devtools --target red-snake read-page                    # full article as markdown
chrome-devtools --target red-snake read-page --json             # with metadata (title, byline, url)

# 5. Inspect what the page did under the hood
chrome-devtools --target red-snake network      # drain accumulated requests
chrome-devtools --target red-snake console       # drain console + exceptions
```

Always pass `--target` from step 2 onward to stay on the same page.

## Custom scripts

`run-script` runs your own JavaScript file inside the page. `adapter` runs a named function from a site-specific file, and first navigates the tab to the file's `@domain` if it is on another site. See the [custom scripting guide](skill/chrome-devtools/CUSTOM_SCRIPTING.md), and the [`run-script`](wiki/run-script.md) and [`adapter`](wiki/adapter.md) pages.

Ready-made scripts live in [aeroxy/chrome-devtools-cli-scripts](https://github.com/aeroxy/chrome-devtools-cli-scripts), one file per site. Use them as they are, or read them to learn how an adapter is put together. So far there is a read-only Discord helper (channels, messages, scroll-back history) that runs in a tab you already have open.

```bash
git clone https://github.com/aeroxy/chrome-devtools-cli-scripts
chrome-devtools --target red-snake adapter chrome-devtools-cli-scripts/discord.js messages
```

To keep scripts where the agent skill finds them, copy the ones you want into the `scripts/` folder next to `SKILL.md`. It ships empty, and the skill tells the agent to check it before writing a one-off script.

## Agent skill

`skill/chrome-devtools/SKILL.md` is a Claude Code skill that teaches the agent how to use this binary. Drop it into any Claude Code plugin's `skills/` directory and set `chrome-devtools` to the binary path. The skill covers the full workflow, all commands, and the `--target` pinning pattern — everything needed to reliably automate Chrome without large context overhead.

## License

MIT

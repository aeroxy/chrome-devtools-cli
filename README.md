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

Override with `--user-data-dir`, `--browser` (chrome/edge), `--channel` (beta/canary/dev), or `--ws-endpoint`. All four also read from environment variables:

| Environment Variable | Corresponding Flag |
|----------------------|--------------------|
| `CHROME_WS_ENDPOINT` | `--ws-endpoint` |
| `CHROME_USER_DATA_DIR` | `--user-data-dir` |
| `CHROME_BROWSER` | `--browser` |
| `CHROME_CHANNEL` | `--channel` |

### Microsoft Edge

Edge is Chromium and speaks the same DevTools Protocol, so every command works against it. Pass `--browser edge` to auto-connect to Edge's profile instead of Chrome's:

| OS | Default path |
|----|-------------|
| macOS | `~/Library/Application Support/Microsoft Edge/` |
| Linux | `~/.config/microsoft-edge/` |
| Windows | `%LOCALAPPDATA%\Microsoft\Edge\User Data\` |

`--channel` composes with it (`--browser edge --channel beta`). Edge Canary is not distributed for Linux, and that combination is rejected rather than pointed at a directory that cannot exist. An explicit `--ws-endpoint` or `--user-data-dir` needs no `--browser` — it only selects the default profile location.

Enterprise-managed Edge can have remote debugging disabled by policy; `DevToolsActivePort` then never appears and auto-connect fails with the message above. That is the same failure mode as Chrome under the equivalent policy, just more common on managed fleets.

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
| `navigate --back` | Go back in history |
| `navigate --forward` | Go forward |
| `navigate --reload` | Reload page |
| `new-page <url>` | Open new tab |
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
| `take-heapsnapshot --output <path>` | Capture V8 heap snapshot (streamed via CDP) |
| `inspect-heapsnapshot-node --file-path <path> --node-id <id>` | Inspect a node in a local `.heapsnapshot` file (offline, no Chrome needed) |

### Interaction

| Command | Description |
|---------|-------------|
| `click <selector>` | Click element by CSS selector |
| `click-at <x> <y>` | Click at specific coordinates |
| `fill <selector> <value>` | Fill input field, dropdown (`<select>`), or toggle checkbox/radio (`"true"`/`"false"`) |
| `type-text <text> [--submit-key <key>]` | Type into focused element (optionally press key after) |
| `press-key <key>` | Press key (e.g. `Enter`, `Control+A`) |
| `hover <selector>` | Hover over element |

### Third-party developer tools

| Command | Description |
|---------|-------------|
| `list-3p-tools` | List custom developer tools exposed via `window.__dtmcp` |
| `execute-3p-tool <name> <params>` | Execute a custom tool by name with a JSON params string |

These commands interact with tools injected into the page via `window.__dtmcp.toolGroup` / `window.__dtmcp.executeTool`.

### Other

| Command | Description |
|---------|-------------|
| `emulate` | Get/set emulation overrides (viewport, geolocation, URL blocking) |
| `emulate --viewport 1280x720` | Set viewport size (per-tab, persists across navigation) |
| `emulate --geolocation 37.77,-122.41` | Set geolocation (per-tab, persists across navigation) |
| `emulate --block-url <pattern>` | Block URL pattern on subsequent requests (glob; per-tab) |
| `emulate --unblock-url <pattern>` | Un-block a previously blocked pattern |
| `emulate --clear-blocks` | Clear all blocked URL patterns |
| `emulate --clear-all` | Clear all overrides (viewport, geolocation, blocks) |
| `wait-for <text> [--timeout ms]` | Wait for text to appear (default 30s) |

`emulate` with no flags shows the active tab's overrides. Viewport, geolocation, and URL blocks are all **per-tab** — each page keeps its own. They persist across navigation within that tab and do **not** leak to other tabs, so you can hold (say) a mobile viewport with images blocked on one tab and a desktop baseline on another at the same time. They persist until you clear them (`--clear-viewport`, `--clear-geolocation`, `--clear-blocks`, or `--clear-all`), the tab closes, or the daemon exits.

### Network and console inspection

The daemon keeps a persistent CDP session on the active page that continuously collects Network and Runtime events. `console` and `network` **drain** whatever has accumulated since the last call (or since the session attached, if never drained).

| Command | Description |
|---------|-------------|
| `console` | Drain accumulated console messages |
| `console --type error --type warning` | Filter by level (`log`, `warn`, `info`, `error`, `debug`, `exception`) |
| `console --duration 5000` | Live collection for 5 s (consumes events — they won't reappear on a later drain) |
| `network` | Drain accumulated network requests |
| `network --type Fetch --type XHR` | Filter by resource type (`Document`, `Script`, `Stylesheet`, `Image`, `Font`, `XHR`, `Fetch`, `Manifest`, `Media`, `Other`) |
| `network --duration 5000` | Live collection for 5 s |
| `sw-logs [--duration 2000]` | Collect console logs from extension service workers (2 s default) |
| `sw-logs --extension-id <id>` | Filter service-worker logs to one extension |

A drain without a `--duration` returns instantly. Adding `--duration N` switches the command to *live mode* and blocks for `N` ms.

### Daemon

| Command | Description |
|---------|-------------|
| `kill-daemon` | Stop the background daemon cleanly |

On Unix, `kill-daemon` signals the targeted daemon with `SIGTERM`, removes its socket, info and PID files, and exits. It's a no-op if no daemon is running. Prefer this over `pkill -f __daemon__` — the process name is shared by legitimate Chrome children processes. On Windows it is not supported: it prints that and exits without signalling anything or removing any files (see **Kill (Windows)** below).

## Global options

| Flag | Description |
|------|-------------|
| `--target <name>` | Target page by friendly name or raw ID |
| `--page <index>` | Target page by index |
| `--json` | JSON output |
| `--toon` | TOON output (compact tabular encoding for LLM agents; mutually exclusive with `--json`) |
| `--block-url <pattern>` | Add a URL pattern to the active tab's block list (repeatable; persists until un-blocked or cleared) |
| `--unblock-url <pattern>` | Remove a URL pattern from the active tab's block list (repeatable) |
| `--ws-endpoint <url>` | Explicit WebSocket URL |
| `--user-data-dir <path>` | Custom browser profile directory |
| `--browser <name>` | Browser to auto-connect to (chrome/edge) |
| `--channel <ch>` | Browser release channel (stable/beta/canary/dev) |

Commands: `list-daemons` shows every running daemon; `kill-daemon [--all]` stops one or all.

Global `--block-url` and `--unblock-url` update the **active tab's** block list and apply via `Network.setBlockedURLs`; the daemon re-applies each tab's list when that tab is in use, so blocking is isolated per tab. **Note:** Chrome only blocks *subresources* (images, scripts, fetch/XHR, stylesheets, CDN, trackers, fonts). The top-level navigation document itself is never blocked — e.g. `--block-url "*example.com*"` then `navigate https://example.com` still loads the page, but any `*.png`, `*.woff2`, etc. subresources on it are blocked.

## Daemon details

- **Instance identity**: one daemon per browser endpoint. The socket/PID/info filenames carry a 16-hex-digit key derived from the resolved `ws://` URL, so Chrome, Edge, every channel and every headless instance get a daemon of their own, and a command aimed at one browser can never be served by a daemon attached to another. The URL's browser GUID changes on every browser launch, so a restarted browser gets a fresh daemon instead of inheriting a dead connection; the orphan exits on its idle timeout.
- **Endpoint (Unix)**: socket at `$TMPDIR/chrome-devtools-daemon-<uid>-<key>.sock` (uid-suffixed so users on a shared machine don't collide)
- **Endpoint (Windows)**: loopback TCP listener; its address is written to `%TEMP%\chrome-devtools-daemon-<key>.addr` (`%TEMP%` is already per-user, so there is no uid suffix — but the instance key is still present, since one daemon per endpoint applies on every platform)
- **PID file**: `$TMPDIR/chrome-devtools-daemon-<uid>-<key>.pid` (Windows: `%TEMP%\chrome-devtools-daemon-<key>.pid`)
- **Info file**: `$TMPDIR/chrome-devtools-daemon-<uid>-<key>.info` — JSON naming the browser, endpoint, PID and start time, so `list-daemons` can label rows. Best-effort: a daemon with no info file still lists, with `?` columns.
- **Lock file**: `$TMPDIR/chrome-devtools-daemon-<uid>.lock` (Windows: `%TEMP%\chrome-devtools-daemon.lock`) — **not** keyed per instance: one lock covers all of them, because it only serializes the brief write-pid-then-bind window, while a per-instance lock would accumulate a never-removed file per browser session. Serializes daemon startup/cleanup; intentionally never removed automatically. Locks bind to the inode, not the name: deleting the file while any daemon process is still starting, running, or shutting down lets a new process lock a fresh replacement inode and bypass the serialization entirely. Only delete it once no daemon process exists at all — and there's rarely a reason to, since a leftover lock file is harmless.
- **Idle timeout**: 5 minutes (auto-exits, cleans up its files)
- **Cleanup**: endpoint + PID files are also removed on panics, and on Unix on SIGTERM/SIGINT; Windows Ctrl-C cleanup is best-effort only (a background daemon has no console to receive it). SIGQUIT, SIGHUP and SIGKILL skip cleanup by design — the leftover files are harmless and are reclaimed by the next daemon start.
- **Protocol**: Length-prefixed JSON over the Unix socket / loopback TCP
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
├── browser.rs        # Auto-connect (DevToolsActivePort)
├── daemon.rs         # Background daemon (persistent connection)
├── client.rs         # Talks to daemon via Unix socket
├── protocol.rs       # IPC message types (DaemonRequest / DaemonResponse)
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
    ├── memory.rs     # take-heapsnapshot (CDP streaming) + inspect-heapsnapshot-node (offline)
    ├── evaluate.rs
    ├── executor.rs   # Command dispatch + persistent-session reuse
    ├── input.rs      # click/fill/type/press/hover
    ├── emulation.rs  # emulate (viewport/geolocation/blocklist get/set/clear)
    ├── console.rs    # console drain / live collection
    ├── network.rs    # network drain / live collection
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

## Agent skill

`skill/chrome-devtools/SKILL.md` is a Claude Code skill that teaches the agent how to use this binary. Drop it into any Claude Code plugin's `skills/` directory and set `chrome-devtools` to the binary path. The skill covers the full workflow, all commands, and the `--target` pinning pattern — everything needed to reliably automate Chrome without large context overhead.

## License

MIT

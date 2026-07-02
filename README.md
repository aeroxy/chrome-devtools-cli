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

## Why this exists

Inspired by [chrome-devtools-mcp](https://github.com/ChromeDevTools/chrome-devtools-mcp) — the official MCP server for Chrome DevTools. It works well, but MCP-based browser tools consume a lot of token context: every interaction sends and receives large protocol payloads through the MCP layer.

99% of the time the browser being controlled is the user's own Chrome with their own credentials, so there is no need for a full headless browser stack like Puppeteer or Playwright, and no need for the MCP overhead.

This is a lightweight Rust binary that talks directly to Chrome's DevTools Protocol. One command in, one result out. No separate browser process, no credential handoff, no heavyweight runtime. The agent skill for this tool is a single `SKILL.md` file — the entire context overhead is this documentation.

## Architecture

```
chrome-devtools navigate https://example.com
        │
        ├─ Try daemon (Unix socket /tmp/chrome-devtools-daemon.sock)
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

By default, the CLI reads `DevToolsActivePort` from Chrome's user data directory:

| OS | Default path |
|----|-------------|
| macOS | `~/Library/Application Support/Google/Chrome/` |
| Linux | `~/.config/google-chrome/` |
| Windows | `%LOCALAPPDATA%\Google\Chrome\User Data\` |

Override with `--user-data-dir`, `--channel` (beta/canary/dev), or `--ws-endpoint`. All three also read from environment variables:

| Environment Variable | Corresponding Flag |
|----------------------|--------------------|
| `CHROME_WS_ENDPOINT` | `--ws-endpoint` |
| `CHROME_USER_DATA_DIR` | `--user-data-dir` |
| `CHROME_CHANNEL` | `--channel` |

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

`kill-daemon` signals the running daemon with `SIGTERM`, removes the socket and PID file, and exits. It's a no-op if no daemon is running. Prefer this over `pkill -f __daemon__` — the process name is shared by legitimate Chrome children processes.

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
| `--user-data-dir <path>` | Custom Chrome profile directory |
| `--channel <ch>` | Chrome channel (stable/beta/canary/dev) |

Global `--block-url` and `--unblock-url` update the **active tab's** block list and apply via `Network.setBlockedURLs`; the daemon re-applies each tab's list when that tab is in use, so blocking is isolated per tab. **Note:** Chrome only blocks *subresources* (images, scripts, fetch/XHR, stylesheets, CDN, trackers, fonts). The top-level navigation document itself is never blocked — e.g. `--block-url "*example.com*"` then `navigate https://example.com` still loads the page, but any `*.png`, `*.woff2`, etc. subresources on it are blocked.

## Daemon details

- **Socket**: `/tmp/chrome-devtools-daemon.sock`
- **PID file**: `/tmp/chrome-devtools-daemon.pid`
- **Idle timeout**: 5 minutes (auto-exits, cleans up socket)
- **Protocol**: Length-prefixed JSON over Unix socket
- **Spawned by**: First CLI invocation (transparent to user)
- **Kill**: `chrome-devtools kill-daemon` (or delete the socket + PID file)

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

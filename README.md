# Chrome DevTools CLI

High-performance rust CLI that connects to an existing Chrome or Edge browser via the DevTools Protocol. Auto-connects by default, no manual WebSocket URL needed.

[![crates.io](https://img.shields.io/crates/v/chrome-devtools-cli.svg)](https://crates.io/crates/chrome-devtools-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/aeroxy/chrome-devtools-cli)

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
        ├─ Try daemon (Unix socket on macOS/Linux, TCP localhost on Windows)
        │   └─ If running → send command → get result
        │
        ├─ If no daemon → spawn one (background process)
        │   └─ Daemon connects to Chrome/Edge WebSocket (one-time approval)
        │   └─ Listens for IPC connections, 5-min idle timeout
        │
        └─ Fallback → direct WebSocket connection (no daemon)
```

The daemon keeps a persistent WebSocket connection to Chrome, so the browser only prompts for DevTools access once. Subsequent commands reuse the connection.

## Prerequisites

Chrome or Edge must have remote debugging enabled:

1. Open Chrome/Edge
2. Go to `chrome://inspect/#remote-debugging` (or `edge://inspect/#remote-debugging`)
3. Enable the remote debugging server

## Auto-connect

By default, the CLI reads `DevToolsActivePort` from the browser's user data directory:

| OS | Chrome (default) | Edge (`--channel edge`) |
|----|-----------------|------------------------|
| macOS | `~/Library/Application Support/Google/Chrome/` | `~/Library/Application Support/Microsoft Edge/` |
| Linux | `~/.config/google-chrome/` | `~/.config/microsoft-edge/` |
| Windows | `%LOCALAPPDATA%\Google\Chrome\User Data\` | `%LOCALAPPDATA%\Microsoft\Edge\User Data\` |

Override with `--user-data-dir`, `--channel` (stable/beta/canary/dev/edge/edge-beta/edge-canary/edge-dev), or `--ws-endpoint`. All three also read from environment variables:

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
| `evaluate <expr> [--dialog-action <action>]` | Run JavaScript (optionally handle dialogs: accept, dismiss, or prompt text) |
| `snapshot` | Accessibility tree dump |

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
| `emulate` | Get/set page-based emulation overrides (viewport, geolocation) |
| `emulate --viewport 1280x720` | Set viewport size (page-based, persists) |
| `emulate --geolocation 37.77,-122.41` | Set geolocation (page-based, persists) |
| `emulate --clear-all` | Clear all emulation overrides |
| `wait-for <text> [--timeout ms]` | Wait for text to appear (default 30s) |

`emulate` with no flags shows all active overrides. Viewport and geolocation overrides are **page-based** — they persist until cleared or the page is closed.

## Global options

| Flag | Description |
|------|-------------|
| `--target <name>` | Target page by friendly name or raw ID |
| `--page <index>` | Target page by index |
| `--json` | JSON output |
| `--ws-endpoint <url>` | Explicit WebSocket URL |
| `--user-data-dir <path>` | Custom Chrome profile directory |
| `--channel <ch>` | Browser channel (stable/beta/canary/dev/edge/edge-beta/edge-canary/edge-dev) |

## Daemon details

- **IPC (macOS/Linux)**: Unix socket at `/tmp/chrome-devtools-daemon.sock`
- **IPC (Windows)**: TCP on `127.0.0.1` with address stored in `%TEMP%\chrome-devtools-daemon.addr`
- **PID file**: `/tmp/chrome-devtools-daemon.pid` (Unix) or `%TEMP%\chrome-devtools-daemon.pid` (Windows)
- **Idle timeout**: 5 minutes (auto-exits, cleans up)
- **Protocol**: Length-prefixed JSON
- **Spawned by**: First CLI invocation (transparent to user)
- **Kill manually**: `pkill -f __daemon__` (Unix) or terminate via PID file (Windows)

## Source layout

```
src/
├── main.rs           # Entry point + daemon dispatch
├── lib.rs            # CLI (clap) + command routing
├── cdp.rs            # Raw CDP over WebSocket (JSON-RPC)
├── browser.rs        # Auto-connect (DevToolsActivePort)
├── daemon.rs         # Background daemon (persistent connection)
├── client.rs         # Talks to daemon via Unix socket
├── protocol.rs       # IPC message types
├── friendly.rs       # Target ID → word-pair names
├── result.rs         # Command result types
├── error.rs          # CLI error types and codes
├── constants.rs      # Shared constants
├── telemetry.rs      # Logging and telemetry
└── commands/
    ├── mod.rs
    ├── navigate.rs
    ├── pages.rs      # list/new/close/select/wait-for
    ├── screenshot.rs
    ├── evaluate.rs
    ├── executor.rs   # Command dispatch
    ├── input.rs      # click/fill/type/press/hover
    ├── snapshot.rs
    ├── emulation.rs  # emulate (viewport/geolocation get/set/clear)
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
```

Always pass `--target` from step 2 onward to stay on the same page.

## Agent skill

`skill/chrome-devtools/SKILL.md` is a Claude Code skill that teaches the agent how to use this binary. Drop it into any Claude Code plugin's `skills/` directory and set `chrome-devtools` to the binary path. The skill covers the full workflow, all commands, and the `--target` pinning pattern — everything needed to reliably automate Chrome without large context overhead.

## License

MIT

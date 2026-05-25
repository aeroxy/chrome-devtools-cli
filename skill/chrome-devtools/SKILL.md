---
name: chrome-devtools
description: Use when the user asks to "take a screenshot of a website", "navigate to a URL", "fill a form in the browser", "interact with Chrome", or when a chrome automation task is needed.
user-invocable: true
---

# Chrome DevTools CLI

A lightweight Rust binary for controlling an existing Chrome browser via the DevTools Protocol. Use this instead of MCP-based browser tools or Puppeteer-style solutions — it connects to the user's own Chrome with their own credentials, requires no headless browser or separate process, and consumes a fraction of the token context.

## Prerequisites

Chrome must have remote debugging enabled:
1. Open Chrome
2. Go to `chrome://inspect/#remote-debugging`
3. Enable the remote debugging server

The binary auto-connects by reading Chrome's `DevToolsActivePort` file — no WebSocket URL needed.

## Core workflow

Every page-level command prints a `[target:word-pair]` line. Capture it and pass `--target` to all subsequent commands to stay on the same tab.

```bash
# Step 1: navigate, capture target name
chrome-devtools navigate https://example.com
# Output includes: [target:red-snake]

# Step 2 onward: pin to that page
chrome-devtools --target red-snake snapshot
chrome-devtools --target red-snake screenshot --output /tmp/page.png
chrome-devtools --target red-snake click "#submit"
chrome-devtools --target red-snake evaluate "document.title"
```

Without `--target`, commands default to tab index 0, which may not be the right page if Chrome reorders tabs.

## Commands

### Navigation
```bash
chrome-devtools navigate <url>          # Go to URL, wait for load
chrome-devtools navigate <url> -o /tmp/res.txt # Save success/result to file
chrome-devtools navigate --back
chrome-devtools navigate --forward
chrome-devtools navigate --reload
chrome-devtools navigate <url> --extra-headers '{"Authorization":"Bearer token"}'  # Set custom HTTP headers
chrome-devtools navigate <url> --latitude 37.7749 --longitude -122.4194            # Set geolocation then navigate
chrome-devtools navigate <url> --latitude 37.7749 --longitude -122.4194 --accuracy 10
chrome-devtools navigate <url> --clear-geolocation                                  # Clear geolocation override
chrome-devtools new-page <url>          # Open new tab
chrome-devtools close-page <index>
chrome-devtools select-page <index>
chrome-devtools list-pages              # List all tabs with friendly names
```

### Inspection
```bash
chrome-devtools --target <name> screenshot --output /tmp/page.png
chrome-devtools --target <name> screenshot --full-page --output /tmp/page.png
chrome-devtools --target <name> evaluate "document.title"
chrome-devtools --target <name> evaluate "document.title" --output /tmp/title.txt
chrome-devtools --target <name> evaluate "window.location.href = 'https://example.com'" -t # -t/--track-navigation tracks URL changes
chrome-devtools --target <name> evaluate "alert('hello')" --dialog-action accept
chrome-devtools --target <name> snapshot   # Accessibility tree — use to understand page structure
chrome-devtools --target <name> snapshot --output /tmp/tree.txt
```

### Interaction
```bash
chrome-devtools --target <name> click "#selector"
chrome-devtools --target <name> click-at 100 200
chrome-devtools --target <name> fill "#selector" "value"
chrome-devtools --target <name> type-text "Hello world" --submit-key Enter
chrome-devtools --target <name> press-key Enter
chrome-devtools --target <name> press-key Control+A
chrome-devtools --target <name> hover ".menu-item"
```

### Third-party developer tools
```bash
chrome-devtools list-3p-tools                          # List tools from window.__dtmcp
chrome-devtools execute-3p-tool <name> '<json-params>' # Execute a custom tool
```

### Utilities
```bash
chrome-devtools --target <name> wait-for "Success" --timeout 10000

# Emulation — page-based overrides (persist until cleared or page closed)
chrome-devtools --target <name> emulate                              # Show active overrides
chrome-devtools --target <name> emulate --viewport 1280x720          # Set viewport
chrome-devtools --target <name> emulate --geolocation 37.77,-122.41  # Set geolocation
chrome-devtools --target <name> emulate --geolocation 37.77,-122.41 --accuracy 10
chrome-devtools --target <name> emulate --viewport 1280x720 --geolocation 37.77,-122.41
chrome-devtools --target <name> emulate --clear-viewport             # Clear viewport
chrome-devtools --target <name> emulate --clear-geolocation          # Clear geolocation
chrome-devtools --target <name> emulate --clear-all                  # Clear everything
```

## Global flags & Environment Variables

| Flag / Env Var | Description |
|------|-------------|
| `--target <name>` | Target page by friendly name or raw Chrome target ID |
| `--page <index>` | Target page by index (for quick one-offs) |
| `--json` | Machine-readable JSON output (includes `navigated_to` and `error_code`) |
| `--ws-endpoint <url>` | Explicit WebSocket endpoint (overrides auto-connect) — env: `CHROME_WS_ENDPOINT` |
| `--user-data-dir <path>` | Custom Chrome profile directory — env: `CHROME_USER_DATA_DIR` |
| `--channel <ch>` | Chrome channel: stable / beta / canary / dev — env: `CHROME_CHANNEL` |
| `DAEMON_WAIT_TIMEOUT_SECS` | Env var: Max seconds to wait for a daemon to spawn (default: 5) |
| `DAEMON_IDLE_TIMEOUT_SECS` | Env var: Max idle seconds before daemon terminates (default: 300) |

## Typical task pattern

1. `list-pages` — see what tabs are open
2. `navigate <url>` — go to target, **note the `[target:name]`**
3. `--target name snapshot` — understand the page structure (accessibility tree is compact and token-efficient vs. a screenshot)
4. `--target name click` / `fill` / `type-text` / `press-key` — interact
5. `--target name evaluate` — extract data or verify state
6. `--target name screenshot --output /tmp/result.png` — capture final state if needed

Use `snapshot` before `screenshot` when trying to understand page structure — it returns text, not an image, and costs far fewer tokens.

## Daemon behavior

The binary automatically manages a background daemon that holds a persistent WebSocket connection to Chrome. Chrome prompts for DevTools access once; all subsequent commands reuse the connection silently. No manual daemon management is needed.

To stop it manually: `pkill -f __daemon__`

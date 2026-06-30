---
name: chrome-devtools
description: Use when the user asks to "take a screenshot of a website", "navigate to a URL", "fill a form in the browser", "interact with Chrome", or when a chrome automation task is needed.
user-invocable: true
---

# Chrome DevTools CLI

A CLI that talks directly to your running Chrome via the DevTools Protocol.

## Prerequisites

Chrome must have remote debugging enabled:
1. Open Chrome
2. Go to `chrome://inspect/#remote-debugging`
3. Enable the remote debugging server

The CLI auto-connects — no URL needed. A daemon is spawned on first invocation and reused across commands (5-minute idle timeout).

## ⚠️ Critical: How Page Targeting Works

**Targets are NOT arbitrary strings.** You cannot use `--target main`, `--target page1`, or any made-up name.

Targets are **friendly word-pair names** (like `warm-squid`, `pink-hen`) that the CLI derives from Chrome's internal target IDs. You **get them from command output** — never invent them.

Names are **stable for the lifetime of a tab** — navigating within the same tab keeps the same target name; closing and reopening the tab gives a new name.

### The Correct Workflow

**Step 1: Run `list-pages` to see what's open and get target names**
```bash
chrome-devtools list-pages
```

Output:
```text
[0] (warm-squid) Your Repositories — https://github.com/aeroxy
[1] (pink-hen) Gmail — https://mail.google.com
[2] (hazy-vole) Example — https://example.com
```

**Step 2: Use the friendly name from the output in subsequent commands**
```bash
chrome-devtools --target warm-squid navigate https://example.com
chrome-devtools --target pink-hen screenshot --output screenshot.png
```

**Alternative: Use `--page <index>` for numeric indexing (0-based)**
```bash
chrome-devtools --page 0 navigate https://example.com
```

**If both `--target` and `--page` are omitted, the command runs on page 0 (leftmost tab).** This is fine for single-tab workflows but should generally be avoided — always pin to a known page.

## Core Capabilities

- **Navigation**: `navigate`, `navigate --back`, `navigate --forward`, `navigate --reload`
- **Page management**: `list-pages`, `new-page`, `close-page`, `select-page`
- **Extraction**: `screenshot`, `snapshot` (accessibility tree), `evaluate` (JavaScript), `read-page` (page content as markdown), `run-script` (run local JS file), `adapter` (run site adapter)
- **Interaction**: `click`, `fill`, `type-text`, `press-key`, `hover`, `click-at`
- **Emulation**: `emulate` (viewport, mobile, geolocation, URL blocking)
- **Inspection**: `console` (logs), `network` (requests), `sw-logs` (extension service workers)
- **Third-party tools**: `list-3p-tools`, `execute-3p-tool` (tools exposed by `window.__dtmcp`)
- **Synchronization**: `wait-for` (wait for text on page)
- **Daemon control**: `kill-daemon`

## Standard Patterns

### Pattern 1: Navigate and Interact

`navigate` and `new-page` print the target name at the end — capture it to pin subsequent commands.

```bash
# 1. List pages to find target
chrome-devtools list-pages

# 2. Navigate (target name shown at end of output)
chrome-devtools --target warm-squid navigate https://example.com
# stdout: Navigated to https://example.com
# stderr: [navigated to: https://example.com]
# stderr: [target:warm-squid]

# 3. Pin all subsequent commands to this page
chrome-devtools --target warm-squid screenshot --output page.png
chrome-devtools --target warm-squid evaluate "document.title"

# 4. Open a new tab — capture the NEW target name from output
chrome-devtools new-page https://github.com
# stdout: Opened: https://github.com
# stderr: [target:icy-goat]  ← new tab, new target
chrome-devtools --target icy-goat snapshot
```

**Note**: The `[navigated to: ...]` and `[target:...]` lines go to **stderr**, not stdout. The stdout contains only the main command output ("Navigated to …", "Opened: …").

### Pattern 2: Emulation (Viewport & Geolocation)
Overrides are per-tab: each page keeps its own viewport/geolocation/URL-blocks, persisting across navigation within that tab and isolated from other tabs (until cleared, the tab closes, or the daemon exits). `emulate` with no flags shows the active tab's state.

```bash
# Set viewport and geolocation
chrome-devtools --target warm-squid emulate --viewport 1920x1080 --geolocation 40.71,-74.00

# Emulate mobile device
chrome-devtools --target warm-squid emulate --viewport 375x812 --mobile --device-scale-factor 3

# Navigate with emulation (emulation applied before URL loads)
chrome-devtools --target warm-squid navigate https://example.com --viewport 375x812 --mobile

# Open new tab with emulation
chrome-devtools new-page https://example.com --viewport 375x812

# Show current overrides
chrome-devtools --target warm-squid emulate
# Output: No emulation overrides active.  (or lists current blocks/viewport/etc.)

# Clear emulation overrides
chrome-devtools --target warm-squid emulate --clear-all        # clears everything
chrome-devtools --target warm-squid emulate --clear-viewport   # clears viewport only
chrome-devtools --target warm-squid emulate --clear-geolocation
```

### Pattern 3: URL Blocking (Network Debugging)
Block URL patterns using simple `*` wildcards: `*.png` (all PNG files), `cdn.example.com/*` (a domain path), `*analytics*` (any URL containing "analytics"). Patterns persist in the daemon until cleared.

> **Scope:** blocking applies to **subresources** the page loads (images, scripts, fetch/XHR, stylesheets, CDN, trackers). It does **not** block the top-level navigation document itself — e.g. `--block-url "*example.com*"` then `navigate https://example.com` still loads the page, but any `example.com` subresources are blocked. This is a Chrome `Network.setBlockedURLs` limitation, not a CLI bug.

```bash
# Add block patterns
chrome-devtools --target warm-squid emulate --block-url "*.png"
chrome-devtools --target warm-squid emulate --block-url "*.ico" --block-url "*.svg"

# Block while navigating (inline)
chrome-devtools --target warm-squid --block-url "*.png" navigate https://example.com

# Show current blocks
chrome-devtools --target warm-squid emulate
# Output: Blocked URLs:
#           *.png
#           *.ico
#           *.svg

# Remove a specific pattern from the blocklist
chrome-devtools --target warm-squid emulate --unblock-url "*.png"
# (Note: --unblock-url REMOVES that pattern from the blocklist. There is no separate "allowlist".)

# Clear all blocks
chrome-devtools --target warm-squid emulate --clear-blocks
chrome-devtools --target warm-squid emulate --clear-all
```

### Pattern 4: Form Interaction

Two ways to fill inputs — choose based on what the site expects:

- `fill` sets the value directly via `element.value = ...`. Fast, no key events. Works for text inputs, textareas, `<select>`, checkboxes, radio buttons. **Often breaks React/Vue apps** because these frameworks rely on real input events.
- `type-text` dispatches individual keyboard events. Slower but triggers all the `input`, `compositionstart/end`, etc. events that frameworks listen for. **Use this when `fill` seems to "not work"** on interactive frameworks.

```bash
# Click an element by CSS selector
chrome-devtools --target warm-squid click "button.submit"

# Click at viewport-relative coordinates (0,0 is top-left of the visible viewport)
chrome-devtools --target warm-squid click-at 100 200

# Fill a text input (fast, no key events)
chrome-devtools --target warm-squid fill "input.search" "search query"

# Type text one char at a time (use for React/Vue/form-validation sites)
chrome-devtools --target warm-squid type-text "search query" --submit-key Enter

# Press a key or key combo. Examples: Enter, Tab, Escape, ArrowDown,
# Control+A, Meta+C, Shift+Tab, Backspace, Space.
chrome-devtools --target warm-squid press-key Enter
chrome-devtools --target warm-squid press-key Control+A

# Hover over an element
chrome-devtools --target warm-squid hover ".menu-item"

# Wait for text to appear (default timeout: 30 seconds = 30000 ms)
chrome-devtools --target warm-squid wait-for "Results" --timeout 10000
```

### Pattern 5: Console & Network Inspection

The daemon maintains a persistent session for the current page that continuously collects network and console events across commands.

`console` and `network` **return accumulated events and clear the buffer** (drain). A second call immediately after returns empty unless new events arrived. Use this for inspecting what happened; use `--duration` for live monitoring.

```bash
# Navigate (generates network + console events)
chrome-devtools --target warm-squid navigate https://example.com

# Drain accumulated network requests (instant, non-blocking)
chrome-devtools --target warm-squid network

# Drain console messages
chrome-devtools --target warm-squid console

# Filter by console type: log, warning, error, info, debug, exception
chrome-devtools --target warm-squid console --type error --type warning

# Filter network by resource type (case-sensitive): Document, Script, Stylesheet,
# Image, Font, XHR, Fetch, Manifest, Media, WebSocket, Other
chrome-devtools --target warm-squid network --type Fetch --type XHR

# Live monitoring: blocks for N milliseconds, collecting events during that window
chrome-devtools --target warm-squid console --duration 5000
chrome-devtools --target warm-squid network --duration 3000

# Drain instantly (default): returns whatever has accumulated so far
chrome-devtools --target warm-squid console --duration 0
```

**Valid `--type` values for `network`**: `Document`, `Script`, `Stylesheet`, `Image`, `Media`, `Font`, `WebSocket`, `Manifest`, `XHR`, `Fetch`, `Other`.

### Pattern 6: JavaScript Evaluation

`evaluate` runs a JavaScript expression and returns the result. It automatically `await`s promises and serializes the return value (objects become JSON, primitives come back as plain text).

```bash
# Get the page title
chrome-devtools --target warm-squid evaluate "document.title"

# Await a promise automatically
chrome-devtools --target warm-squid evaluate "fetch('/api/user').then(r => r.json())"

# Get a value as JSON (forces JSON serialization)
chrome-devtools --target warm-squid --json evaluate "performance.navigation"

# Handle a JS dialog (alert, confirm, prompt). Without --dialog-action, eval hangs.
chrome-devtools --target warm-squid evaluate "alert('hi')" --dialog-action accept
chrome-devtools --target warm-squid evaluate "confirm('sure?')" --dialog-action dismiss
chrome-devtools --target warm-squid evaluate "prompt('name')" --dialog-action "my-answer"
# Valid --dialog-action values: "accept", "dismiss", or any prompt-text string.

# Save JS output to a file
chrome-devtools --target warm-squid evaluate "JSON.stringify(performance.timing)" -o /tmp/perf.json
```

**Avoid `evaluate` for DOM traversal.** Use `snapshot` to read page structure and `click`/`fill` to interact.

### Pattern 7: Output Formats
All commands default to human-readable text output. Use `--json` or `--toon` (compact, LLM-friendly) for structured output.

```bash
chrome-devtools list-pages                    # human-readable table (default)
chrome-devtools list-pages --json             # JSON
chrome-devtools list-pages --toon             # TOON (compact)

chrome-devtools --target warm-squid snapshot --toon
chrome-devtools --target warm-squid network --toon --type Fetch
```

`--json` and `--toon` are mutually exclusive.

### Pattern 8: Snapshot (Accessibility Tree)
Use snapshot instead of `evaluate document.querySelector(...)` for understanding page structure.

```bash
chrome-devtools --target warm-squid snapshot
chrome-devtools --target warm-squid snapshot --output /tmp/ax-tree.txt
chrome-devtools --target warm-squid snapshot --toon  # compact output
```

### Pattern 9: Screenshots
```bash
# Default: viewport-only PNG
chrome-devtools --target warm-squid screenshot --output page.png

# Full scrollable page (can be very tall)
chrome-devtools --target warm-squid screenshot --full-page --output full-page.png

# Save to a specific path
chrome-devtools --target warm-squid screenshot --output /tmp/whatever.jpg
```

### Pattern 10: Extension Service Worker Logs
Browser-level command — no `--target` needed.

```bash
# Collect logs from ALL extension service workers (2s window)
chrome-devtools sw-logs --duration 2000

# Filter by extension ID (from sw-logs output)
chrome-devtools sw-logs --duration 2000 --extension-id abcdef123456
```

### Pattern 11: Third-party Developer Tools
For pages that expose tools via `window.__dtmcp`.

```bash
chrome-devtools --target warm-squid list-3p-tools
chrome-devtools --target warm-squid execute-3p-tool "<tool-name>" '<json-params>'
```

### Pattern 12: Reading Page Content as Markdown
Extract the main article content of a page as clean markdown. Uses Readability to identify the article body and converts it to LLM-friendly markdown with metadata (title, byline, excerpt, URL). Non-article pages (SPAs, dashboards) fall back to converting the full page.

```bash
# Read the current page as markdown (title prepended as H1)
chrome-devtools --target warm-squid read-page

# Save to a file
chrome-devtools --target warm-squid read-page --output /tmp/article.md

# JSON output includes metadata fields (title, byline, excerpt, site_name, url)
chrome-devtools --target warm-squid read-page --json
```

**When to use `read-page` vs `snapshot`:**
- `read-page` — you want the page's textual content as readable markdown (articles, docs, wiki pages). Best for summarization, extraction, or feeding content to an LLM.
- `snapshot` — you need the full accessibility tree with element IDs, roles, and interactive elements. Best for understanding page structure and finding elements to click/fill.

### Pattern 13: Local JS Scripting (run-script)

Evaluate a local JavaScript file inside the page context. Dynamic arguments passed via `-a/--arg` are automatically typed and injected into the execution context as `ctx.args`. Standard helper functions are also injected.

```bash
# Run a script with dynamic arguments
chrome-devtools --target warm-squid run-script skill/chrome-devtools/examples/search_deepwiki.js --arg query="aeroxy/ast-bro"
```

### Pattern 14: Custom Domain-Aware Adapters (adapter)

Run site-specific adapter actions. If the browser is not currently on a matching domain (as defined by `@domain` comments in the JSDoc header), the CLI auto-navigates to that domain first.

```bash
# Run an adapter function with automatic domain protection and navigation
chrome-devtools --target warm-squid adapter skill/chrome-devtools/examples/deepwiki_adapter.js ask --arg query="how to write adapter"
```

## Complete Command Reference

### Navigation
```bash
chrome-devtools list-pages
chrome-devtools navigate <url> [--viewport WxH] [--mobile] [--device-scale-factor N] [--geolocation lat,lon]
chrome-devtools navigate <url> --extra-headers '{"Authorization":"Bearer ..."}'
chrome-devtools navigate --back
chrome-devtools navigate --forward
chrome-devtools navigate --reload
chrome-devtools new-page <url> [--viewport WxH] [--mobile]
chrome-devtools close-page [index_or_target_name]
chrome-devtools select-page [index_or_target_name]
```

### Inspection
```bash
chrome-devtools --target <name> screenshot [--output <path>] [--full-page]
chrome-devtools --target <name> snapshot
chrome-devtools --target <name> read-page [--output <path>]
chrome-devtools --target <name> evaluate "<js-expression>" [--dialog-action accept|dismiss|text]
chrome-devtools --target <name> network [--duration <ms>] [--type <resource>]
chrome-devtools --target <name> console [--duration <ms>] [--type <level>]
chrome-devtools sw-logs [--duration <ms>] [--extension-id <id>]
```

### Interaction
```bash
chrome-devtools --target <name> click "<css-selector>"
chrome-devtools --target <name> click-at <x> <y>
chrome-devtools --target <name> fill "<css-selector>" "<value>"
chrome-devtools --target <name> type-text "<text>" [--submit-key <key>]
chrome-devtools --target <name> press-key <key>
chrome-devtools --target <name> hover "<css-selector>"
chrome-devtools --target <name> wait-for "<text>" [--timeout <ms>]   # default 30000
```

### Emulation
```bash
chrome-devtools --target <name> emulate [--viewport WxH] [--mobile] [--geolocation lat,lon]
chrome-devtools --target <name> emulate                                  # show current state
chrome-devtools --target <name> emulate --block-url "<pattern>" [--block-url ...]
chrome-devtools --target <name> emulate --unblock-url "<pattern>"          # remove from blocklist
chrome-devtools --target <name> emulate --clear-blocks
chrome-devtools --target <name> emulate --clear-viewport
chrome-devtools --target <name> emulate --clear-geolocation
chrome-devtools --target <name> emulate --clear-all                      # clears everything
```

### Third-party Tools
```bash
chrome-devtools --target <name> list-3p-tools
chrome-devtools --target <name> execute-3p-tool <name> '<json-params>'
```

### Custom Scripting & Adapters
```bash
chrome-devtools --target <name> run-script <file-path> [--arg key=value] [--output <path>] [--track-navigation]
chrome-devtools --target <name> adapter <file-path> <function-name> [--arg key=value] [--output <path>] [--track-navigation]
```

### Daemon
```bash
chrome-devtools kill-daemon        # stop the background daemon process
```

## Critical Gotchas

### ✗ WRONG: Using invented target names
```bash
chrome-devtools --target main navigate https://example.com        # ❌ WRONG
chrome-devtools --target page1 screenshot                         # ❌ WRONG
chrome-devtools --target "my-page" evaluate "..."                 # ❌ WRONG
```

### ✓ CORRECT: Get target from command output
```bash
chrome-devtools list-pages                                        # ✓ First: list pages
# [0] (warm-squid) Example — https://example.com
chrome-devtools --target warm-squid navigate https://github.com   # ✓ Use the name from output
```

### ✗ WRONG: Running commands without first finding a target
```bash
chrome-devtools screenshot --output page.png                      # ❌ WRONG: unpredictable page
```

### ✓ CORRECT: Always identify the page first
```bash
chrome-devtools list-pages                                        # ✓ First
chrome-devtools --target warm-squid screenshot --output page.png  # ✓ Pinned to known page
```

### ✗ WRONG: Using evaluate for DOM traversal or interaction
```bash
chrome-devtools --target warm-squid evaluate "document.querySelector('...').click()"   # ❌ WRONG
```

### ✓ CORRECT: Use snapshot for structure, click/fill for interaction
```bash
chrome-devtools --target warm-squid snapshot                      # ✓ Read structure
chrome-devtools --target warm-squid click "button.submit"         # ✓ Interact
```

### ✗ WRONG: Expecting `fill` to update React/Vue forms
```bash
chrome-devtools --target warm-squid fill "input" "value"          # ❌ value set, but framework state unchanged
```

### ✓ CORRECT: Use `type-text` for stateful frameworks
```bash
chrome-devtools --target warm-squid type-text "value"             # ✓ real key events → framework state updates
```

### ✗ WRONG: Expecting `console`/`network` to remember events after drain
```bash
chrome-devtools --target warm-squid console
# [error] foo
# [warning] bar
chrome-devtools --target warm-squid console
# No console messages collected.  ← the buffer was drained on the first call
```

### ✓ CORRECT: Each drain is a fresh window
Run `console` / `network` right after the action that produces events, OR use `--duration` to collect for a window of time.

## Output Format Summary

| Flag | Description |
|------|-------------|
| *(none)* | Human-readable text (default) |
| `--json` | Pretty-printed JSON |
| `--toon` | TOON — compact tabular format (fewer tokens for LLM agents) |

`--json` and `--toon` are mutually exclusive.

## Stdout vs Stderr

- **stdout** contains the primary command output (table, text, JSON, TOON).
- **stderr** contains informational lines (`[target:...]`, `[navigated to: ...]`) and errors.

When parsing command output programmatically, read only stdout to get the main result.

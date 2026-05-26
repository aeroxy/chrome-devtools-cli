---
name: chrome-devtools
description: Use when the user asks to "take a screenshot of a website", "navigate to a URL", "fill a form in the browser", "interact with Chrome", or when a chrome automation task is needed.
user-invocable: true
---

## Prerequisites

Chrome must have remote debugging enabled:
1. Open Chrome
2. Go to `chrome://inspect/#remote-debugging`
3. Enable the remote debugging server

The CLI auto-connects by reading Chrome's `DevToolsActivePort` file — no WebSocket URL needed.

## Core Capabilities

- **Navigation**: `navigate`, `back`, `forward`, `reload`.
- **Emulation**: `emulate` (viewport, mobile, device-scale-factor, geolocation).
- **Extraction**: `screenshot`, `snapshot` (accessibility tree), `list-pages`.
- **Interaction**: `click`, `click-at`, `fill`, `type-text`, `press-key`, `hover`.
- **Execution**: `evaluate` (JS), `execute-3p-tool`.
- **Synchronization**: `wait-for` (text on page).

## Usage Guide

### Page Selection
Most commands require a page target. Use `--page <index>` (0-based) or `--target <id>`.

```bash
chrome-devtools list-pages
chrome-devtools --target main navigate https://example.com
```

### Emulation (Viewport & Geolocation)
Overrides are persistent per page. You can set them standalone or during navigation.

```bash
# Set viewport and geolocation standalone
chrome-devtools --target main emulate --viewport 1920x1080 --geolocation 40.71,-74.00

# Emulate mobile device
chrome-devtools --target main emulate --viewport 375x812 --mobile --device-scale-factor 3

# Navigate with atomic emulation (sets environment before loading URL)
chrome-devtools navigate https://geotargetly.com --geolocation 51.50,-0.12

# Navigate as mobile device
chrome-devtools navigate https://example.com --viewport 375x812 --mobile

# Open new tab with atomic emulation
chrome-devtools new-page https://example.com --viewport 375x812

# Clear emulation overrides
chrome-devtools --target main emulate --clear-all
chrome-devtools --target main emulate --clear-viewport
chrome-devtools --target main emulate --clear-geolocation
```

### Interaction Patterns
```bash
# Search and submit
chrome-devtools fill "input.search" "Rust programming"
chrome-devtools press-key Enter
chrome-devtools wait-for "The Rust Programming Language"

# Take a full-page screenshot
chrome-devtools screenshot --full-page --output search_results.png
```

### Advanced Evaluation
```bash
# Evaluate and get return value
chrome-devtools evaluate "document.title"

# Handle potential dialogs automatically
chrome-devtools evaluate "alert('hi')" --dialog-action accept
```

## Command Reference

### Navigation
```bash
chrome-devtools navigate <url> [--viewport WxH] [--mobile] [--device-scale-factor N] [--geolocation lat,lon] [--accuracy M]
chrome-devtools navigate <url> --extra-headers '{"Authorization":"Bearer ..."}'
chrome-devtools navigate <url> -o /tmp/result.txt
chrome-devtools navigate --back
chrome-devtools navigate --forward
chrome-devtools navigate --reload
chrome-devtools new-page <url> [--viewport WxH] [--mobile] [--device-scale-factor N] [--geolocation lat,lon]
chrome-devtools new-page <url> --extra-headers '{"X-Debug":"1"}'
chrome-devtools close-page [id_or_index]
chrome-devtools select-page [id_or_index]
```

### Emulation
```bash
chrome-devtools emulate [--viewport WxH] [--mobile] [--device-scale-factor N] [--geolocation lat,lon] [--accuracy M]
chrome-devtools emulate --clear-all
chrome-devtools emulate --clear-viewport
chrome-devtools emulate --clear-geolocation
```

### Utilities
```bash
chrome-devtools --target <name> wait-for "Success" --timeout 10000
chrome-devtools list-pages
```

### Third-party Developer Tools
```bash
chrome-devtools list-3p-tools
chrome-devtools execute-3p-tool <name> '<json-params>'
```

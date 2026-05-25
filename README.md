# chrome-devtools-cli

A high-performance, developer-friendly CLI for interacting with Chrome via the DevTools Protocol (CDP).

## Key Features

- **Page Emulation**: Manage viewport size and geolocation overrides in one place.
- **Smart Navigation**: URL navigation, back/forward, and reload with automatic page-load waiting.
- **Visual Tools**: High-quality screenshots (including full-page) and accessibility tree snapshots.
- **Interaction**: Click, fill, type, and hover using CSS selectors or coordinates.
- **JS Evaluation**: Run JavaScript on the page with support for handling dialogs.
- **3rd Party Integration**: Access tools exposed by pages via custom protocol extensions.

## Installation

```bash
cargo install --path .
```

## Quick Start

### General Usage
```bash
chrome-devtools list-pages
chrome-devtools --page 0 navigate https://google.com
chrome-devtools --target main screenshot --output screenshot.png
```

### Emulation (Page-level Overrides)
Overrides like viewport size and geolocation are persistent per page.

```bash
# Set viewport and geolocation
chrome-devtools emulate --viewport 1280x720 --geolocation 37.77,-122.41

# Clear overrides
chrome-devtools emulate --clear-all
```

### Interaction
```bash
chrome-devtools click "button.submit"
chrome-devtools fill "input[name='q']" "searching for something"
chrome-devtools type-text "submitting now" --submit-key Enter
```

## Global Options

- `--ws-endpoint`: Use an explicit WebSocket URL.
- `--user-data-dir`: Auto-connect to a running Chrome instance.
- `--page <index>`: Select page by 0-based index.
- `--target <id>`: Select page by friendly name or ID.
- `--json`: Format output as JSON.

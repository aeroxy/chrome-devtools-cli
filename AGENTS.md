# Chrome DevTools CLI — Agent Guide

## Overview

High-performance Rust CLI that connects to a running Chrome browser via the
DevTools Protocol. Talks directly to Chrome's CDP WebSocket — no MCP overhead,
no headless browser stack. One command in, one result out.

## Repository Structure

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
├── format.rs         # OutputFormat (text/json/toon) + format_structured
├── result.rs         # CommandResult type
├── error.rs          # CLI error types and codes
├── constants.rs      # Shared constants
├── telemetry.rs      # Logging and telemetry
└── commands/
    ├── executor.rs   # Command dispatch + persistent-session reuse
    ├── navigate.rs
    ├── pages.rs      # list/new/close/select/wait-for
    ├── screenshot.rs
    ├── snapshot.rs
    ├── read_page.rs  # read-page (Readability + HTML→Markdown)
    ├── evaluate.rs
    ├── input.rs      # click/fill/type/press/hover
    ├── emulation.rs  # emulate (viewport/geolocation/blocklist)
    ├── console.rs    # console drain / live collection
    ├── network.rs    # network drain / live collection
    ├── sw_logs.rs    # extension service-worker log collection
    └── third_party.rs # list-3p-tools/execute-3p-tool
```

## Wiki

Detailed documentation for individual commands:

- [read-page](wiki/read-page.md) — page content extraction as markdown

## Key Concepts

### Daemon Architecture

A background daemon (`/tmp/chrome-devtools-daemon.sock`) keeps a persistent CDP
WebSocket connection. First CLI invocation spawns it; subsequent commands reuse
it. 5-minute idle timeout.

### Page Targeting

Every page gets a deterministic friendly name (e.g. `warm-squid`) derived from
Chrome's internal target ID. Commands should always use `--target <name>` to pin
to a specific page — page indices shift as tabs are opened/closed.

### Persistent Session

The daemon maintains a persistent CDP session on the active page that
continuously collects `Network.*` and `Runtime.*` events. `console` and
`network` commands drain whatever has accumulated since the last call.

### Output Formats

All commands default to human-readable text. `--json` and `--toon` (compact,
LLM-friendly) produce structured output. Mutually exclusive.

## Build & Test

```bash
cargo build --release          # Binary: ./target/release/chrome-devtools
cargo test                      # Run all tests
cargo test commands::read_page  # Run tests for a specific module
```

## Coding Conventions

- Comments explain **why**, not what
- Each command is a pure async function taking `&mut CdpClient`, `session_id`,
  `OutputFormat`, and command-specific args
- Pure conversion/formatting logic is extracted into testable functions
- Tests live in `#[cfg(test)] mod tests` within the same file
- Error handling uses `anyhow::Result` with descriptive messages
- CDP calls go through `CdpClient::send_to_target()`

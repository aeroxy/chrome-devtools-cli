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
    ├── memory.rs     # take-heapsnapshot (CDP streaming) + inspect-heapsnapshot-node / compare-heapsnapshots (offline)
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

## Agent Skill

`skill/chrome-devtools/SKILL.md` is the **source of truth** for the agent-facing
skill/documentation — it's what teaches an agent (e.g. Claude Code, opencode) how
to use this CLI (targeting, standard patterns, gotchas, failure handling, etc.).
It gets installed/copied into an agent's skills directory (e.g.
`~/.config/opencode/skills/chrome-devtools/SKILL.md`); that installed copy is a
**deployed artifact**, not the source — always edit `skill/chrome-devtools/SKILL.md`
in this repo, then re-sync/reinstall it, rather than editing the installed copy
directly. `skill/chrome-devtools/CUSTOM_SCRIPTING.md` documents `run-script` and
`adapter` in more depth.

## Key Concepts

### Daemon Architecture

A background daemon (`/tmp/chrome-devtools-daemon.sock`) keeps a persistent CDP
WebSocket connection. First CLI invocation spawns it; subsequent commands reuse
it. 5-minute idle timeout.

`CdpClient::connect` (`cdp.rs`) bounds the WebSocket handshake with a timeout
(`CHROME_CONNECT_TIMEOUT_SECS`, default 10s). Without it, a pending Chrome
remote-debugging consent dialog would hang the handshake indefinitely — and
since the daemon binds its socket *before* connecting to Chrome (see comments
in `daemon.rs`), the CLI's `wait_for_daemon()` would succeed immediately while
the actual request silently hung forever waiting on the daemon's response, with
no error and no way for a caller (especially an unattended agent) to tell what
was wrong. The timeout error message is written to be agent-actionable: retry
at most once, then stop and ask a human rather than looping or calling
`kill-daemon`.

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

### Offline Commands

`inspect-heapsnapshot-node`, `compare-heapsnapshots`, and `kill-daemon` are
intercepted early in `run()` before any Chrome connection or daemon spawn.
`inspect-heapsnapshot-node` and `compare-heapsnapshots` parse local
`.heapsnapshot` files purely offline. Note that snapshot diffing matches nodes
by V8 heap object ID, which is only stable within a single Chrome session —
both snapshots must come from the same session to produce a meaningful diff.

`kill-daemon` drops the daemon's already-approved Chrome connection, so it's
guarded (`kill_daemon_decision` in `lib.rs`): interactive (TTY) callers get a
`[y/N]` confirmation prompt; non-interactive callers (agents, scripts) are
refused outright unless `--force` is passed. It must never be used as a
"retry" step for connection failures — see the timeout note above.

### Path Resolution

The daemon retains its startup CWD, so the CLI resolves all relative file-path
arguments (`--output`, `--file-path`) to absolute paths in `build_request`
before sending them to the daemon.

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

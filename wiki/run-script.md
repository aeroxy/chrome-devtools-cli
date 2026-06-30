# run-script

Evaluate a local JavaScript file inside the current page context with injected helper utilities and parsed dynamic arguments.

## Synopsis

```bash
chrome-devtools [--target <name>] run-script <file_path> [--arg key=value] [raw_args...] [--output <path>] [--track-navigation]
```

## Description

`run-script` reads a local JavaScript file off-disk, wraps it inside an Immediately Invoked Function Expression (IIFE), and evaluates it directly inside the target browser's page context.

### Flexible Argument Syntax

Dynamic arguments passed to the script can be specified in several clean and intuitive styles:

1. **Pure Positional Style (Recommended for single queries):**
   Simply append raw positional strings at the end of the command. If a single argument is passed, it is automatically mapped to `ctx.args.query` (as well as `ctx.args._0`):
   ```bash
   chrome-devtools run-script search_hn.js "Rust"
   ```
2. **Hybrid Style (Positional + Named):**
   For scripts with multiple parameters, you can pass the main parameter positionally, and other options as explicit `key=value` pairs:
   ```bash
   chrome-devtools run-script search_hn.js "Rust" limit=10 safeSearch=true
   ```
3. **Pure Named Style:**
   Specify named options as explicit key-value pairs at the end of the command or via the `-a/--arg` flag:
   ```bash
   chrome-devtools run-script search_hn.js query="Rust" limit=10
   ```

All values are automatically parsed into their appropriate JavaScript types (e.g. `10` to number, `true` to boolean, etc.) and made available inside `ctx.args`.

### Auto-Navigation and Page Opening

By declaring a standard `// @url <target_url>` or `// @navigate <target_url>` comment marker at the top of your script file, the CLI will check the active tab's current URL before executing your script. If the active tab is not currently on a domain matching the target URL, **the CLI will automatically navigate the tab to the target URL first**, wait for the page to load, and then execute your script. This allows you to run automated scripts without needing to pre-open or pre-navigate the page manually!

```javascript
// @url https://hn.algolia.com
```

### Injected Helper Context (`ctx`)

Before executing your script, `run-script` injects a globally-accessible helper `ctx` object with several standard automation wrappers:

*   `ctx.args`: Object containing key-value arguments parsed from CLI flags.
*   `ctx.wait(ms)`: Sleep/delay helper (`await ctx.wait(1000)`).
*   `ctx.waitForText(text, timeout_ms)`: Polls the page body text until the string is present (defaults to 30s).
*   `ctx.waitForSelector(selector, timeout_ms)`: Polls until an element matching the CSS selector exists in the DOM.
*   `ctx.click(selector)`: Clicks an element by CSS selector.
*   `ctx.fill(selector, value)`: Fills an input field with the value and fires standard input and change events.

## Real-World Example: Search Hacker News

This script searches `hn.algolia.com` (Hacker News Search) for a query and extracts the results dynamically without triggering a full page reload.

`run-script` already runs your file inside an async context, so use the `ctx`
helpers at the top level and `return` the result directly — no IIFE wrapper is
needed. Because we have defined the `@url` metadata tag, the CLI will automatically
navigate to `https://hn.algolia.com` if the browser is not already on that site.

### Script file (`skill/chrome-devtools/examples/search_hn.js`)
```javascript
// @url https://hn.algolia.com

// search_hn.js
// Run with: chrome-devtools run-script skill/chrome-devtools/examples/search_hn.js -a query="Rust"
//
// run-script injects `ctx` and runs this file inside an async context.
// Setting `@url` above tells the CLI to automatically navigate to the target site first!

const query = ctx.args.query;
if (!query) {
  throw new Error("Query argument is required. Pass it with '-a query=...'");
}

// Fill in search input (the SPA will fetch and render results dynamically)
await ctx.fill("input.SearchInput", query);

// Wait for results to update/load
await ctx.waitForSelector("article.Story", 10000);

// Extract results
const results = Array.from(document.querySelectorAll("article.Story")).map(el => {
  const titleEl = el.querySelector(".Story_title a");
  const metaEl = el.querySelector(".Story_meta");
  return {
    title: titleEl?.innerText.trim() || "",
    meta: metaEl?.innerText.trim() || "",
    url: titleEl?.href || ""
  };
});

return results;
```

### CLI Execution
```bash
# Execute the script directly — the CLI handles the auto-navigation seamlessly!
chrome-devtools --target warm-squid run-script skill/chrome-devtools/examples/search_hn.js --arg query="Rust" --json
```

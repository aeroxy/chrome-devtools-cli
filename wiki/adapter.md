# adapter

Run site-specific custom JavaScript adapter functions with built-in domain protection, auto-navigation, and injected automation helpers.

## Synopsis

```bash
chrome-devtools [--target <name>] adapter <file_path> <function_name> [--arg key=value] [raw_args...] [--output <path>] [--track-navigation]
```

## Description

`adapter` reads a local custom JS adapter file, parses the target `@domain` markers, and ensures the browser is on a matching domain before invoking a specific named function exported or defined inside the script.

### Flexible Argument Syntax

Dynamic arguments passed to the adapter function can be specified in several clean and intuitive styles:

1. **Pure Positional Style (Recommended for single queries):**
   Simply append raw positional strings at the end of the command. If a single argument is passed, it is automatically mapped to `ctx.args.query` (as well as `ctx.args._0`):
   ```bash
   chrome-devtools adapter hn_adapter.js search "Rust"
   ```
2. **Hybrid Style (Positional + Named):**
   For functions with multiple parameters, you can pass the main parameter positionally, and other options as explicit `key=value` pairs:
   ```bash
   chrome-devtools adapter hn_adapter.js search "Rust" limit=10 safeSearch=true
   ```
3. **Pure Named Style:**
   Specify named options as explicit key-value pairs at the end of the command or via the `-a/--arg` flag:
   ```bash
   chrome-devtools adapter hn_adapter.js search query="Rust" limit=10
   ```

All values are automatically parsed into their appropriate JavaScript types (e.g. `10` to number, `true` to boolean, etc.) and made available inside `ctx.args`.

### Domain Protection and Auto-Navigation

By declaring standard `@domain` markers at the top of your adapter file, the CLI checks the current page URL before executing your function. If the active tab is not on the target domain, **it automatically navigates the tab to the first target domain**, waits for it to load, and then runs your adapter.

```javascript
// ==UserAdapter==
// @name         My Custom Adapter
// @domain       wikipedia.org
// ==/UserAdapter==
```

### Injected Helper Context (`ctx`)

Like `run-script`, your adapter function is passed a `ctx` context containing helper utilities:

*   `ctx.args`: Object containing typed key-value arguments.
*   `ctx.wait(ms)`: Delay utility.
*   `ctx.waitForText(text, timeout_ms)`: Text matching polling utility.
*   `ctx.waitForSelector(selector, timeout_ms)`: CSS selector matching polling utility.
*   `ctx.click(selector)`: DOM clicking helper.
*   `ctx.fill(selector, value)`: DOM value input helper.

## Real-World Example: Hacker News Search Adapter

This adapter has a target domain of `hn.algolia.com` and exposes a `search` function.

### Adapter file (`skill/chrome-devtools/examples/hn_adapter.js`)
```javascript
// ==UserAdapter==
// @name         Hacker News Search Adapter
// @domain       hn.algolia.com
// ==/UserAdapter==

// Run with: chrome-devtools adapter skill/chrome-devtools/examples/hn_adapter.js search -a query="Rust"

async function search(ctx) {
  const query = ctx.args.query;
  if (!query) throw new Error("query argument is required");

  // Fill search input (the SPA will fetch and render results dynamically)
  await ctx.fill("input.SearchInput", query);

  // Wait for results to update/load
  await ctx.waitForSelector("article.Story", 10000);

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
}
```

### CLI Execution
```bash
# Executing 'search' on hn.algolia.com (will auto-navigate there if not already open)
chrome-devtools --target warm-squid adapter skill/chrome-devtools/examples/hn_adapter.js search --arg query="Rust" --json
```

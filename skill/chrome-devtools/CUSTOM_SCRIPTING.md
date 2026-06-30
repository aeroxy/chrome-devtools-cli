# Custom Scripting & Adapters Guide

This guide details how to create and execute custom JavaScript scripts (`run-script`) and custom domain-aware adapters (`adapter`) using the Chrome DevTools CLI.

---

## 1. Custom Scripts (`run-script`)

`run-script` reads a local JavaScript file, wraps it inside an Immediately Invoked Function Expression (IIFE), and evaluates it directly inside the target browser's page context.

### Flexible Argument Syntax
Dynamic arguments passed to the script can be specified in several styles and are automatically parsed and made available inside `ctx.args`:

1. **Pure Positional Style (Recommended for single queries):**
   Simply append raw positional strings at the end of the command. A single trailing positional argument is automatically mapped to `ctx.args.query` (as well as `ctx.args._0`):
   ```bash
   chrome-devtools run-script search_hn.js "Rust"
   ```
2. **Hybrid Style (Positional + Named):**
   ```bash
   chrome-devtools run-script search_hn.js "Rust" limit=10 safeSearch=true
   ```
3. **Pure Named Style:**
   ```bash
   chrome-devtools run-script search_hn.js query="Rust" limit=10
   ```

### Comment-based Auto-Navigation
By declaring a standard `// @url <target_url>` or `// @navigate <target_url>` comment marker at the top of your script file, the CLI will check the active tab's current URL before executing your script. 

If the active tab is not currently on a domain matching the target URL, **the CLI will automatically navigate the tab to the target URL first**, wait for the page to load, and then execute your script. You can use `{arg_name}` placeholders inside the `@url` template to interpolate CLI arguments dynamically:
```javascript
// @url https://hn.algolia.com/?query={query}
```

---

## 2. Custom Domain-Aware Adapters (`adapter`)

`adapter` reads a local custom JS adapter file, parses the target `@domain` JSDoc markers, and ensures the browser is on a matching domain before invoking a specific named function inside the script.

### Domain Protection and Auto-Navigation
By declaring standard `@domain` markers at the top of your adapter file, the CLI checks the active page URL before executing your function. If the active tab is not on the target domain, **it automatically navigates the tab to the first target domain**, waits for it to load, and then runs your adapter.

```javascript
// ==UserAdapter==
// @name         Hacker News Search Adapter
// @domain       hn.algolia.com
// ==/UserAdapter==
```

---

## 3. Injected Helper Context (`ctx`)

Both `run-script` and `adapter` functions are passed an injected `ctx` context containing standard helper utilities:

* `ctx.args`: Object containing typed key-value arguments.
* `ctx.wait(ms)`: Sleep/delay utility (`await ctx.wait(1000)`).
* `ctx.waitForText(text, timeout_ms)`: Polls the page body text until the string is present (defaults to 30s).
* `ctx.waitForSelector(selector, timeout_ms)`: Polls until an element matching the CSS selector exists in the DOM.
* `ctx.click(selector)`: DOM clicking helper.
* `ctx.fill(selector, value)`: DOM value input helper. Highly compatible with stateful frameworks (like React, Vue, and Angular) as it overrides standard value setters and fires appropriate events.

---

## 4. Real-World SPA Example (Hacker News Search)

These real-world examples work on `hn.algolia.com`.

### Script file (`skill/chrome-devtools/examples/search_hn.js`)
```javascript
// @url https://hn.algolia.com/?query={query}

// search_hn.js
// Run with: chrome-devtools run-script skill/chrome-devtools/examples/search_hn.js "Rust"

const query = ctx.args.query;
if (!query) {
  throw new Error("Query argument is required.");
}

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

### Adapter file (`skill/chrome-devtools/examples/hn_adapter.js`)
```javascript
// ==UserAdapter==
// @name         Hacker News Search Adapter
// @domain       hn.algolia.com
// ==/UserAdapter==

// Run with: chrome-devtools adapter skill/chrome-devtools/examples/hn_adapter.js search "Rust"

async function search(ctx) {
  const query = ctx.args.query;
  if (!query) throw new Error("query argument is required");

  // Fill search input (the SPA will fetch and render results dynamically)
  await ctx.fill("input.SearchInput", query);

  // Wait a brief moment for React and the network request to resolve and update the DOM
  await ctx.wait(1500);

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

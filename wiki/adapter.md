# adapter

Run site-specific custom JavaScript adapter functions with built-in domain protection, auto-navigation, and injected automation helpers.

## Synopsis

```bash
chrome-devtools [--target <name>] adapter <file_path> <function_name> [--arg key=value] [--output <path>] [--track-navigation]
```

## Description

`adapter` reads a local custom JS adapter file, parses the target `@domain` markers, and ensures the browser is on a matching domain before invoking a specific named function exported or defined inside the script.

### Domain Protection and Auto-Navigation

By declaring standard `@domain` markers at the top of your adapter file, the CLI checks the current page URL before executing your function. If the active tab is not on the target domain, **it automatically navigates the tab to the first target domain**, waits for it to load, and then runs your adapter.

```javascript
// ==UserAdapter==
// @name         My Custom Adapter
// @domain       deepwiki.com
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

## Real-World Example: DeepWiki AI Q&A Adapter

This adapter has a target domain of `deepwiki.com` and exposes an `ask` Q&A function and a `readWiki` document reader function.

### Adapter file (`skill/chrome-devtools/examples/deepwiki_adapter.js`)
```javascript
// ==UserAdapter==
// @name         DeepWiki Adapter
// @domain       deepwiki.com
// ==/UserAdapter==

async function ask(ctx) {
  const query = ctx.args.query;
  if (!query) throw new Error("query argument is required");

  // Fill search input and click ask/search
  await ctx.fill("input.ask-input, input[placeholder*='Ask']", query);
  await ctx.click("button.ask-btn, button[type='submit']");

  // Wait for AI response to finish streaming/loading
  await ctx.waitForSelector(".answer-box, .ai-response", 15000);
  await ctx.wait(2000); // Allow text to settle

  const answer = document.querySelector(".answer-box, .ai-response")?.innerText.trim() || "";
  const sources = Array.from(document.querySelectorAll(".sources-list a, .citation-link")).map(el => ({
    title: el.innerText.trim(),
    url: el.href
  }));

  return { query, answer, sources };
}

async function readWiki(ctx) {
  const wikiUrl = ctx.args.url;
  if (!wikiUrl) throw new Error("url argument is required");

  window.location.href = wikiUrl;
  await ctx.waitForSelector("article, .wiki-content", 10000);

  return {
    title: document.querySelector("h1, .wiki-title")?.innerText.trim() || "",
    content: document.querySelector("article, .wiki-content")?.innerText.trim() || ""
  };
}
```

### CLI Execution
```bash
# Executing 'ask' on deepwiki.com (will auto-navigate there if not already open)
chrome-devtools --target warm-squid adapter skill/chrome-devtools/examples/deepwiki_adapter.js ask --arg query="how to write adapter" --json
```

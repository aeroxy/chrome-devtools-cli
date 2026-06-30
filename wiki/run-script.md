# run-script

Evaluate a local JavaScript file inside the current page context with injected helper utilities and parsed dynamic arguments.

## Synopsis

```bash
chrome-devtools [--target <name>] run-script <file_path> [--arg key=value] [--output <path>] [--track-navigation]
```

## Description

`run-script` reads a local JavaScript file off-disk, wraps it inside an Immediately Invoked Function Expression (IIFE), and evaluates it directly inside the target browser's page context.

Dynamic arguments passed with `-a` / `--arg` are automatically typed (strings, integers, floats, booleans) and made available to the script.

### Injected Helper Context (`ctx`)

Before executing your script, `run-script` injects a globally-accessible helper `ctx` object with several standard automation wrappers:

*   `ctx.args`: Object containing key-value arguments parsed from CLI flags.
*   `ctx.wait(ms)`: Sleep/delay helper (`await ctx.wait(1000)`).
*   `ctx.waitForText(text, timeout_ms)`: Polls the page body text until the string is present (defaults to 30s).
*   `ctx.waitForSelector(selector, timeout_ms)`: Polls until an element matching the CSS selector exists in the DOM.
*   `ctx.click(selector)`: Clicks an element by CSS selector.
*   `ctx.fill(selector, value)`: Fills an input field with the value and fires standard input and change events.

## Real-World Example: Search DeepWiki

This script searches `deepwiki.com` for a repository name and extracts the results.

### Script file (`skill/chrome-devtools/examples/search_deepwiki.js`)
```javascript
(async () => {
  const query = ctx.args.query;
  if (!query) {
    throw new Error("Query argument is required. Pass it with '-a query=...'");
  }

  // Navigate to deepwiki if not already there
  if (!window.location.href.includes("deepwiki.com")) {
    window.location.href = "https://deepwiki.com";
    await ctx.wait(2000);
  }

  // Fill in search input and submit
  await ctx.fill("input[placeholder*='search']", query);
  await ctx.click("button[type='submit']");
  
  // Wait for results list to load
  await ctx.waitForSelector(".search-results-list, .repo-card", 10000);

  // Extract titles, descriptions, and URLs
  const results = Array.from(document.querySelectorAll(".repo-card, .wiki-page-item")).map(el => {
    return {
      title: el.querySelector(".title, h3")?.innerText.trim() || "",
      description: el.querySelector(".description, p")?.innerText.trim() || "",
      url: el.querySelector("a")?.href || ""
    };
  });

  return results;
})();
```

### CLI Execution
```bash
chrome-devtools --target warm-squid run-script skill/chrome-devtools/examples/search_deepwiki.js --arg query="aeroxy/ast-bro" --json
```

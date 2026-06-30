// search_deepwiki.js
// Run with: chrome-devtools run-script skill/chrome-devtools/examples/search_deepwiki.js -a query="aeroxy/ast-bro"
//
// run-script injects `ctx` and runs this file inside an async context, so use
// the ctx helpers directly at the top level and `return` the result. Navigating
// would tear down the evaluation context, so this script requires the page to
// already be on deepwiki.com (use the `navigate` command first).

const query = ctx.args.query;
if (!query) {
  throw new Error("Query argument is required. Pass it with '-a query=...'");
}

if (!window.location.href.includes("deepwiki.com")) {
  throw new Error("Not on deepwiki.com — navigate there first: chrome-devtools navigate https://deepwiki.com");
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

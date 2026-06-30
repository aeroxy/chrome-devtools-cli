// @url https://hn.algolia.com/?query={query}

// search_hn.js
// Run with: chrome-devtools run-script skill/chrome-devtools/examples/search_hn.js -a query="Rust"
//
// run-script injects `ctx` and runs this file inside an async context.
// Setting `@url` above tells the CLI to automatically navigate to the pre-rendered query URL first!

const query = ctx.args.query;
if (!query) {
  throw new Error("Query argument is required. Pass it with '-a query=...'");
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

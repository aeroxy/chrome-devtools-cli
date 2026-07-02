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

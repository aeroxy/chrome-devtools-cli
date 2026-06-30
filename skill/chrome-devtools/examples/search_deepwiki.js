// search_deepwiki.js
// Run with: chrome-devtools run-script skill/chrome-devtools/examples/search_deepwiki.js -a query="aeroxy/ast-bro"

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

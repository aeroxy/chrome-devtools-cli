// ==UserAdapter==
// @name         DeepWiki Adapter
// @domain       deepwiki.com
// ==/UserAdapter==

// Run with: chrome-devtools adapter skill/chrome-devtools/examples/deepwiki_adapter.js ask -a query="how to write adapter"

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
  // Navigation must happen before running this adapter (e.g. via the `navigate`
  // command). Changing window.location here would tear down the Runtime.evaluate
  // context mid-execution, so readWiki only scrapes the already-loaded page.
  return {
    title: document.querySelector("h1, .wiki-title")?.innerText.trim() || "",
    content: document.querySelector("article, .wiki-content")?.innerText.trim() || ""
  };
}

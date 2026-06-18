# read-page

Extract the current page's main content as clean markdown.

## Synopsis

```bash
chrome-devtools [--target <name>] read-page [--output <path>]
```

## Description

`read-page` serializes the rendered DOM, extracts the main article using a
Readability algorithm (via [dom_smoothie](https://crates.io/crates/dom_smoothie)),
then converts the cleaned HTML to markdown (via [htmd](https://crates.io/crates/htmd)).

Non-article pages (SPAs, dashboards, search results) fall back to converting the
full page, so content is never silently dropped.

## Output

### Text mode (default)

Returns the article content as markdown with the page title prepended as an H1
heading (unless the body already starts with one).

```md
# Will Serfort

**Will Serfort** is the protagonist of *Wistoria: Wand and Sword*...

## Appearance

Will has purple/blue eyes and messy black hair...
```

### JSON mode (`--json`)

Returns a JSON object with the markdown body and any metadata extracted by
Readability:

```json
{
  "markdown": "**Will Serfort** is the protagonist...",
  "title": "Will Serfort",
  "excerpt": "Will Serfort is the protagonist of...",
  "url": "https://wistoria.fandom.com/wiki/Will_Serfort"
}
```

Fields that are not available are omitted (not emitted as `null`).

### TOON mode (`--toon`)

Same fields as JSON in compact TOON encoding.

## Pipeline

```text
CDP Runtime.evaluate (HTML + URL as JSON)
  → dom_smoothie Readability extraction
    → unwrap <iframe> tags (innermost-first for nested support)
      → htmd HTML→Markdown (skip inert elements only)
        → format output (text with H1, or structured with metadata)
```

### Content extraction

Uses `dom_smoothie` (a Rust port of Mozilla Readability) with DomSmoothie
candidate selection, which captures more meaningful content than the stock
Readability heuristic on pages with few competing candidates.

### Title resolution

1. Readability extracts a title from meta tags or headings
2. Falls back to the `<title>` tag (with HTML entity decoding)
3. If neither produces a title, no H1 is prepended

### Inert element stripping

Only truly inert elements are stripped:

| Stripped | Reason |
|----------|--------|
| `head` | Document metadata container |
| `script` | Executable code, no visible content |
| `style` | CSS rules, no visible content |
| `noscript` | Fallback for disabled JS |
| `link` | External resource references |
| `meta` | Document metadata |
| `title` | Already captured by Readability |
| `svg` | Visual-only, no markdown equivalent |
| `canvas` | Pixel graphics, empty in serialized DOM |

Structural containers (`nav`, `footer`, `aside`) and interactive elements
(`form`, `button`, `select`) are **preserved** — they contain navigable links
and actionable elements the LLM can interact with.

### Iframe handling

`<iframe>` tags are unwrapped before conversion, promoting their inner HTML to
the parent level. This works around `html5ever` treating `<iframe>` as a
raw-text element per the HTML spec (inner HTML stored as escaped text, not
parsed DOM). Nested iframes are processed innermost-first to prevent tag
mismatch.

## When to use `read-page` vs `snapshot`

| Use case | Command |
|----------|---------|
| Read article/docs content | `read-page` |
| Feed page text to an LLM | `read-page` |
| Extract structured metadata (title, author) | `read-page --json` |
| Understand page structure | `snapshot` |
| Find elements to click/fill | `snapshot` |
| Get element IDs and roles | `snapshot` |

## Examples

```bash
# Read a wiki article
chrome-devtools --target warm-squid navigate https://en.wikipedia.org/wiki/Rust_(programming_language)
chrome-devtools --target warm-squid read-page

# Save to file
chrome-devtools --target warm-squid read-page --output /tmp/article.md

# Get metadata as JSON
chrome-devtools --target warm-squid read-page --json

# Read a non-article page (falls back to full-page conversion)
chrome-devtools --target warm-squid navigate https://github.com
chrome-devtools --target warm-squid read-page
```

## Dependencies

- [dom_smoothie](https://crates.io/crates/dom_smoothie) — Rust port of Mozilla Readability
- [htmd](https://crates.io/crates/htmd) — HTML to Markdown converter

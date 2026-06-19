use anyhow::Result;
use dom_smoothie::{CandidateSelectMode, Config, Readability};
use htmd::HtmlToMarkdown;
use serde_json::{json, Value};

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// Inline JS that returns the page HTML and current URL in a single evaluation,
/// avoiding a second CDP round trip for URL resolution.
const GET_HTML_AND_URL_JS: &str =
    "JSON.stringify({html: document.documentElement.outerHTML, url: window.location.href})";

/// Case-insensitive substring search without allocating a lowercase copy.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle_bytes = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle_bytes.len())
        .position(|w| {
            w.iter()
                .zip(needle_bytes)
                .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
        })
}

/// Extract the `<title>` text from raw HTML via case-insensitive string search.
/// Used as a fallback when Readability doesn't produce a title.
fn extract_title_from_html(html: &str) -> Option<String> {
    let open_tag = "<title";
    let close_tag = "</title>";

    let open_pos = find_ci(html, open_tag)?;
    let tag_end = html[open_pos..].find('>')?;
    let start = open_pos + tag_end + 1;
    let end = find_ci(&html[start..], close_tag)? + start;
    let title = html[start..end].trim();

    if title.is_empty() {
        None
    } else {
        Some(decode_html_entities(title))
    }
}

/// Decode common HTML entities.
fn decode_html_entities(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// Run Mozilla-Readability-style extraction over `html`, returning the cleaned
/// article HTML plus its detected title. Falls back to the raw page HTML when
/// the document is not article-like (apps, dashboards, search results) or when
/// extraction fails, so we never silently drop content.
fn extract_content(html: &str, url: Option<&str>) -> (String, ReadableMeta) {
    let cfg = Config {
        // DomSmoothie candidate selection captures more of the meaningful
        // content than the stock Readability heuristic, which sometimes
        // discards sections when there are few competing candidates.
        candidate_select_mode: CandidateSelectMode::DomSmoothie,
        ..Default::default()
    };

    let fallback_title = extract_title_from_html(html);

    match Readability::new(html, url, Some(cfg)) {
        Ok(mut r) if r.is_probably_readable() => match r.parse() {
            Ok(article) => {
                let title = if article.title.is_empty() {
                    fallback_title
                } else {
                    Some(article.title.clone())
                };
                let meta = ReadableMeta {
                    title,
                    byline: article.byline.clone(),
                    excerpt: article.excerpt.clone(),
                    site_name: article.site_name.clone(),
                };
                (article.content.to_string(), meta)
            }
            // Readable but extraction failed: hand back the whole page.
            Err(_) => (
                html.to_string(),
                ReadableMeta {
                    title: fallback_title,
                    byline: None,
                    excerpt: None,
                    site_name: None,
                },
            ),
        },
        // Not article-like, or the document couldn't be loaded: don't let
        // readability mangle it — convert the full page instead.
        _ => (
            html.to_string(),
            ReadableMeta {
                title: fallback_title,
                byline: None,
                excerpt: None,
                site_name: None,
            },
        ),
    }
}

/// Metadata surfaced by readability, used to enrich text/structured output.
struct ReadableMeta {
    title: Option<String>,
    byline: Option<String>,
    excerpt: Option<String>,
    site_name: Option<String>,
}

/// Unwrap `<iframe>` tags, promoting their inner HTML to the parent level.
///
/// `html5ever` (used by `htmd`) treats `<iframe>` as a raw-text element per the
/// HTML spec, so inner HTML is stored as escaped text rather than parsed DOM.
/// Pre-stripping the tags lets the inner content flow through as normal HTML
/// for proper markdown conversion.
///
/// Processes innermost iframes first (an `<iframe>` whose content contains no
/// nested `<iframe>`), preventing tag-mismatch on deeply nested cases. Loops
/// until all iframes are unwrapped.
fn unwrap_iframes(html: &str) -> String {
    let mut result = html.to_string();
    loop {
        // Find innermost iframe: an <iframe> with no nested <iframe> inside.
        let mut search_from = 0;
        let mut best_open = None;
        let mut best_close = None;

        while let Some(open) = find_ci(&result[search_from..], "<iframe") {
            let open = search_from + open;
            let tag_end = match result[open..].find('>') {
                Some(i) => open + i + 1,
                None => {
                    // Malformed tag: no closing '>' found. Skip past this '<' and continue
                    // searching for the next potential tag.
                    search_from = open + 1;
                    continue;
                }
            };

            if let Some(close) = find_ci(&result[tag_end..], "</iframe") {
                let close = tag_end + close;
                let inner = &result[tag_end..close];

                if find_ci(inner, "<iframe").is_none() {
                    // Innermost — no nested <iframe> between open and close.
                    best_open = Some((open, tag_end));
                    best_close = Some(close);
                    break;
                }

                // Has nested <iframe> inside — advance past this open tag
                // and look for the inner one.
                search_from = tag_end;
                continue;
            }

            // No closing tag found — strip the opening tag and retry.
            result = format!("{}{}", &result[..open], &result[tag_end..]);
            best_open = None;
            best_close = None;
            continue;
        }

        match (best_open, best_close) {
            (Some((open, tag_end)), Some(close)) => {
                let close_end = match result[close..].find('>') {
                    Some(i) => {
                        // Check if there's a '<' before the '>' to avoid crossing tag boundaries
                        if result[close..close + i].contains('<') {
                            break;
                        }
                        close + i + 1
                    }
                    None => break,
                };
                let inner = &result[tag_end..close];
                result = format!("{}{}{}", &result[..open], inner, &result[close_end..]);
            }
            _ => break,
        }
    }
    result
}

/// Convert HTML to markdown and format the output for display.
///
/// Only strips truly inert elements — scripts, styles, metadata, and visual-only
/// tags (svg/canvas) that produce no useful markdown. Structural containers
/// (nav, footer, aside) are preserved: they hold navigable links the LLM can
/// act on. Iframe tags are unwrapped before conversion so their inner HTML is
/// parsed and converted as normal content.
fn format_output(
    content_html: &str,
    meta: &ReadableMeta,
    url: Option<&str>,
    format: OutputFormat,
) -> Result<String> {
    let preprocessed = unwrap_iframes(content_html);

    let converter = HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "noscript", "link", "meta", "title", "svg", "canvas",
        ])
        .build();
    let body_md = converter.convert(&preprocessed)?;

    if format.is_text() {
        match &meta.title {
            Some(t) if !t.is_empty() && !body_md.trim_start().starts_with("# ") => {
                Ok(format!("# {}\n\n{}", t, body_md))
            }
            _ => Ok(body_md),
        }
    } else {
        let mut obj = json!({ "markdown": body_md });
        if let Some(t) = &meta.title {
            obj["title"] = json!(t);
        }
        if let Some(b) = &meta.byline {
            obj["byline"] = json!(b);
        }
        if let Some(e) = &meta.excerpt {
            obj["excerpt"] = json!(e);
        }
        if let Some(s) = &meta.site_name {
            obj["site_name"] = json!(s);
        }
        if let Some(u) = url {
            obj["url"] = json!(u);
        }
        format_structured(&obj, format)
    }
}

/// Read the current page as markdown.
///
/// Serializes the rendered DOM, extracts the main article with a Readability
/// port (`dom_smoothie`), then converts the cleaned HTML to markdown via
/// `htmd`. Non-article pages fall back to converting the full page so content
/// is never silently dropped. Output is LLM-friendly by default.
pub async fn read_page(
    client: &mut CdpClient,
    session_id: &str,
    format: OutputFormat,
    output: Option<&str>,
) -> Result<CommandResult> {
    // Single CDP call fetches both HTML and URL — avoids a second round trip
    // that a separate `current_url()` call would require.
    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": GET_HTML_AND_URL_JS,
                "returnByValue": true,
            }),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception["text"]
            .as_str()
            .or_else(|| exception["exception"]["description"].as_str())
            .unwrap_or("Unknown error evaluating page content");
        anyhow::bail!("{text}");
    }

    let raw = result["result"]["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Failed to read page content"))?;

    let (html, url) = match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => (
            v["html"].as_str().unwrap_or("").to_string(),
            v["url"].as_str().map(|s| s.to_string()),
        ),
        // Fallback: treat as plain HTML string (shouldn't happen with our JS).
        _ => (raw.to_string(), None),
    };

    let (content_html, meta) = extract_content(&html, url.as_deref());
    let content = format_output(&content_html, &meta, url.as_deref(), format)?;

    Ok(CommandResult::output(content).save_output(output).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // -- extract_title_from_html --

    #[test]
    fn extract_title_normal() {
        let html = "<html><head><title>My Page Title</title></head><body></body></html>";
        assert_eq!(
            extract_title_from_html(html),
            Some("My Page Title".to_string())
        );
    }

    #[test]
    fn extract_title_case_insensitive() {
        let html = "<html><head><TITLE>Upper Title</TITLE></head></html>";
        assert_eq!(
            extract_title_from_html(html),
            Some("Upper Title".to_string())
        );
    }

    #[test]
    fn extract_title_missing() {
        assert_eq!(extract_title_from_html("<html><body>Hello</body></html>"), None);
    }

    #[test]
    fn extract_title_empty() {
        assert_eq!(
            extract_title_from_html("<html><head><title></title></head></html>"),
            None
        );
    }

    #[test]
    fn extract_title_whitespace_trimmed() {
        let html = "<html><head><title>  Trimmed Title  </title></head></html>";
        assert_eq!(
            extract_title_from_html(html),
            Some("Trimmed Title".to_string())
        );
    }

    #[test]
    fn extract_title_decodes_html_entities() {
        let html = "<html><head><title>Foo &amp; Bar</title></head></html>";
        assert_eq!(
            extract_title_from_html(html),
            Some("Foo & Bar".to_string())
        );
    }

    #[test]
    fn extract_title_decodes_multiple_entities() {
        let html = "<html><head><title>&lt;script&gt; &quot;alert&quot;</title></head></html>";
        assert_eq!(
            extract_title_from_html(html),
            Some("<script> \"alert\"".to_string())
        );
    }

    #[test]
    fn extract_title_no_double_decode() {
        // &amp;lt; should decode to &lt; (not <)
        let html = "<html><head><title>&amp;lt;escaped&amp;gt;</title></head></html>";
        assert_eq!(
            extract_title_from_html(html),
            Some("&lt;escaped&gt;".to_string())
        );
    }

    // -- decode_html_entities --

    #[test]
    fn decode_entities_ampersand() {
        assert_eq!(decode_html_entities("A &amp; B"), "A & B");
    }

    #[test]
    fn decode_entities_all_named() {
        assert_eq!(
            decode_html_entities("&lt;&gt;&quot;&#39;&apos;&amp;"),
            "<>\"''&"
        );
    }

    #[test]
    fn decode_entities_no_double_decode() {
        assert_eq!(decode_html_entities("&amp;lt;"), "&lt;");
    }

    #[test]
    fn decode_entities_plain_text_unchanged() {
        assert_eq!(decode_html_entities("no entities here"), "no entities here");
    }

    // -- unwrap_iframes --

    #[test]
    fn unwrap_iframes_basic() {
        assert_eq!(
            unwrap_iframes("<p>A</p><iframe src=\"x\"><p>B</p></iframe><p>C</p>"),
            "<p>A</p><p>B</p><p>C</p>"
        );
    }

    #[test]
    fn unwrap_iframes_preserves_attributes_in_inner_html() {
        assert_eq!(
            unwrap_iframes("<iframe src=\"x\"><a href=\"/link\">click</a></iframe>"),
            "<a href=\"/link\">click</a>"
        );
    }

    #[test]
    fn unwrap_iframes_mixed_case() {
        assert_eq!(
            unwrap_iframes("<IFRAME src=\"x\"><P>Content</P></IFRAME>"),
            "<P>Content</P>"
        );
    }

    #[test]
    fn unwrap_iframes_multiple_siblings() {
        assert_eq!(
            unwrap_iframes("<iframe src=\"a\">A</iframe><iframe src=\"b\">B</iframe>"),
            "AB"
        );
    }

    #[test]
    fn unwrap_iframes_nested() {
        assert_eq!(
            unwrap_iframes("<iframe src=\"outer\">O<iframe src=\"inner\">I</iframe>O</iframe>"),
            "OIO"
        );
    }

    #[test]
    fn unwrap_iframes_deeply_nested() {
        assert_eq!(
            unwrap_iframes(
                "<iframe src=\"1\">A<iframe src=\"2\">B<iframe src=\"3\">C</iframe>B</iframe>A</iframe>"
            ),
            "ABCBA"
        );
    }

    #[test]
    fn unwrap_iframes_empty() {
        assert_eq!(
            unwrap_iframes("<iframe src=\"x\"></iframe>"),
            ""
        );
    }

    #[test]
    fn unwrap_iframes_no_iframes() {
        assert_eq!(unwrap_iframes("<p>plain html</p>"), "<p>plain html</p>");
    }

    #[test]
    fn unwrap_iframes_unclosed_strips_open_tag() {
        assert_eq!(
            unwrap_iframes("<p>A</p><iframe src=\"x\"><p>B</p>"),
            "<p>A</p><p>B</p>"
        );
    }

    #[test]
    fn unwrap_iframes_unclosed_followed_by_well_formed() {
        assert_eq!(
            unwrap_iframes("<iframe src=\"broken\"><iframe src=\"ok\"><p>B</p></iframe>"),
            "<p>B</p>"
        );
    }

    // -- extract_content --

    #[test]
    fn extract_content_non_article_returns_raw_html_with_title() {
        let html = "<html><head><title>Login Page</title></head><body><h1>Log In</h1><form><input type='text'/></form></body></html>";
        let (content, meta) = extract_content(html, None);
        assert_eq!(content, html);
        assert_eq!(meta.title, Some("Login Page".to_string()));
        assert!(meta.byline.is_none());
        assert!(meta.excerpt.is_none());
        assert!(meta.site_name.is_none());
    }

    #[test]
    fn extract_content_no_title_anywhere() {
        let html = "<html><body><p>Just some content with no title tag.</p></body></html>";
        let (content, meta) = extract_content(html, None);
        assert_eq!(content, html);
        assert!(meta.title.is_none());
    }

    #[test]
    fn extract_content_passes_url_for_relative_link_resolution() {
        let html = "<html><head><title>Test</title></head><body><a href='/page'>link</a></body></html>";
        let url = "https://example.com/article";
        let (_, _) = extract_content(html, Some(url));
        // Smoke test: URL is accepted without panic. Readability may or may not
        // use it depending on whether the page is deemed article-like.
    }

    #[test]
    fn extract_content_readability_article() {
        // Enough content to pass Readability's is_probably_readable() threshold.
        let html = r#"<html><head><title>Readability Test</title></head><body>
            <article>
                <h1>Article Heading</h1>
                <p>This is the first paragraph of a test article. It contains enough text to be
                considered substantial by the Readability algorithm which typically requires
                paragraphs of reasonable length to score well.</p>
                <p>The second paragraph adds more weight to the article scoring algorithm.
                Readability uses text density, paragraph count, and other signals to determine
                whether a page is article-like.</p>
                <p>A third paragraph for good measure ensures the content density is high enough
                to trigger the readability detection. Without sufficient text the algorithm
                would classify this as a non-article page.</p>
                <p>The fourth paragraph further strengthens the article signal. Real articles
                typically have multiple paragraphs of substantive text which is what the
                algorithm is designed to detect.</p>
                <p>A fifth paragraph makes this unambiguously article-like content. The
                Readability algorithm should have no trouble identifying this as an article
                worth extracting.</p>
            </article>
        </body></html>"#;
        let (content, meta) = extract_content(html, None);
        // Should extract article content (not return raw HTML).
        assert_ne!(content, html);
        assert!(content.contains("first paragraph"));
        // Title should come from Readability, falling back to <title> tag.
        assert!(meta.title.is_some());
    }

    // -- format_output --

    #[test]
    fn format_output_text_prepends_title_as_h1() {
        let html = "<p>Hello world</p>";
        let meta = ReadableMeta {
            title: Some("My Article".to_string()),
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(result.starts_with("# My Article\n\n"));
    }

    #[test]
    fn format_output_text_no_double_h1_when_body_starts_with_heading() {
        let html = "<h1>Already Has Heading</h1><p>Content here</p>";
        let meta = ReadableMeta {
            title: Some("Duplicate Title".to_string()),
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        // Should NOT prepend "# Duplicate Title" since body starts with "# "
        assert!(!result.starts_with("# Duplicate Title\n\n# Already"));
    }

    #[test]
    fn format_output_text_no_title_no_prepend() {
        let html = "<p>Just content</p>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(!result.starts_with("# "));
    }

    #[test]
    fn format_output_structured_no_nulls_for_missing_metadata() {
        let html = "<p>Hello world</p>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Json).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("markdown").is_some());
        assert!(parsed.get("title").is_none());
        assert!(parsed.get("byline").is_none());
        assert!(parsed.get("excerpt").is_none());
        assert!(parsed.get("site_name").is_none());
    }

    #[test]
    fn format_output_structured_includes_url() {
        let html = "<p>Content</p>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(
            html,
            &meta,
            Some("https://example.com"),
            OutputFormat::Json,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["url"], "https://example.com");
    }

    #[test]
    fn format_output_structured_includes_present_metadata() {
        let html = "<p>Content</p>";
        let meta = ReadableMeta {
            title: Some("Article".to_string()),
            byline: Some("Jane Doe".to_string()),
            excerpt: None,
            site_name: Some("Example".to_string()),
        };
        let result = format_output(html, &meta, None, OutputFormat::Json).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["title"], "Article");
        assert_eq!(parsed["byline"], "Jane Doe");
        assert_eq!(parsed["site_name"], "Example");
        // excerpt is None → must not appear
        assert!(parsed.get("excerpt").is_none());
    }

    #[test]
    fn format_output_skip_tags_strips_scripts_and_styles() {
        let html =
            "<head><style>body{color:red}</style></head><script>alert(1)</script><p>Real content</p>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(!result.contains("color:red"));
        assert!(!result.contains("alert"));
        assert!(result.contains("Real content"));
    }

    #[test]
    fn format_output_preserves_nav_footer_aside() {
        let html = "<nav>Navigation</nav><main><p>Main content</p></main><footer>Footer</footer><aside>Sidebar</aside>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(result.contains("Navigation"));
        assert!(result.contains("Main content"));
        assert!(result.contains("Footer"));
        assert!(result.contains("Sidebar"));
    }

    #[test]
    fn format_output_preserves_form_elements() {
        let html = "<form><button>Submit</button><select><option>A</option></select></form>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(result.contains("Submit"));
    }

    #[test]
    fn format_output_recursively_converts_iframe_inner_content() {
        let html = r#"<p>Before</p><iframe src="https://example.com"><p>Embedded <strong>bold</strong> text</p><a href="/link">A link</a></iframe><p>After</p>"#;
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(result.contains("Embedded **bold** text"));
        assert!(result.contains("[A link](/link)"));
        assert!(result.contains("Before"));
        assert!(result.contains("After"));
    }

    #[test]
    fn format_output_recursively_converts_nested_iframes() {
        let html = r#"<iframe src="/outer"><p>Outer</p><iframe src="/inner"><p>Inner content</p></iframe></iframe>"#;
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert!(result.contains("Outer"));
        assert!(result.contains("Inner content"));
    }

    #[test]
    fn format_output_empty_iframe_produces_no_output() {
        let html = "<p>Before</p><iframe src=\"https://example.com\"></iframe><p>After</p>";
        let meta = ReadableMeta {
            title: None,
            byline: None,
            excerpt: None,
            site_name: None,
        };
        let result = format_output(html, &meta, None, OutputFormat::Text).unwrap();
        assert_eq!(result.trim(), "Before\n\nAfter");
    }
}

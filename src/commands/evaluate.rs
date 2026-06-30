use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::constants::POLL_INTERVAL_MS;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// Evaluate a JavaScript expression in the page and return the result.
pub async fn evaluate(
    client: &mut CdpClient,
    session_id: &str,
    expression: &str,
    format: OutputFormat,
    output: Option<&str>,
    track_navigation: bool,
) -> Result<CommandResult> {
    // Handle JavaScript dialogs (alert, confirm, prompt) during evaluation.
    // `client.dialog_action` must be set to "accept", "dismiss", or a prompt
    // response string before calling this function. The underlying
    // `send_to_target` call will then automatically handle any
    // `Page.javascriptDialogOpening` events that occur.

    let initial_url = if track_navigation {
        Some(client.current_url(session_id).await?)
    } else {
        None
    };

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception["text"].as_str().unwrap_or("Unknown error");
        let desc = exception["exception"]["description"]
            .as_str()
            .unwrap_or(text);
        anyhow::bail!("{desc}");
    }

    let value = &result["result"];
    let val_type = value["type"].as_str().unwrap_or("undefined");

    let output_hint = if format.is_text() {
        let text = match val_type {
            "undefined" => "undefined".to_string(),
            "string" => value["value"].as_str().unwrap_or("").to_string(),
            _ => {
                if let Some(v) = value.get("value") {
                    serde_json::to_string_pretty(v)?
                } else {
                    value["description"].as_str().unwrap_or("").to_string()
                }
            }
        };

        text
    } else {
        let v = value.get("value").unwrap_or(value);
        format_structured(v, format)?
    };

    if let Some(initial_url) = initial_url {
        let new_url = client.current_url(session_id).await?;
        let result = CommandResult::output(output_hint)
            .with_navigated_to_if_changed(new_url.clone(), initial_url.clone());
        Ok(result
            .save_output(output)
            .await?
            .with_navigated_to_if_changed(new_url, initial_url))
    } else {
        Ok(CommandResult::output(output_hint)
            .save_output(output)
            .await?)
    }
}

/// Build the injected `ctx` automation-helper object shared by `run-script` and
/// `adapter`.
///
/// The returned snippet declares `const ctx = {...}` and is meant to be embedded
/// at the top of an async IIFE, before user code runs. Both call sites reuse it
/// so the helper surface stays in lockstep.
fn build_ctx_object(args_str: &str) -> String {
    format!(
        r#"const ctx = {{
                args: {args_str},
                wait: async (ms) => new Promise(r => setTimeout(r, ms)),
                waitForText: async (text, timeout = 30000) => {{
                    const start = Date.now();
                    while (Date.now() - start < timeout) {{
                        if (document.body && document.body.innerText.includes(text)) return;
                        await new Promise(r => setTimeout(r, {POLL_INTERVAL_MS}));
                    }}
                    throw new Error("Timeout waiting for text: " + text);
                }},
                waitForSelector: async (selector, timeout = 30000) => {{
                    const start = Date.now();
                    while (Date.now() - start < timeout) {{
                        if (document.querySelector(selector)) return;
                        await new Promise(r => setTimeout(r, {POLL_INTERVAL_MS}));
                    }}
                    throw new Error("Timeout waiting for selector: " + selector);
                }},
                click: async (selector) => {{
                    const el = document.querySelector(selector);
                    if (!el) throw new Error("Element not found: " + selector);
                    el.click();
                }},
                fill: async (selector, value) => {{
                    const el = document.querySelector(selector);
                    if (!el) throw new Error("Element not found: " + selector);
                    if (el.type === 'checkbox' || el.type === 'radio') {{
                        // Checkboxes/radios toggle via `checked`, not `value`. A
                        // boolean (or "true"/"false") sets the state directly; any
                        // other value selects the input whose `value` it matches.
                        if (value === true || value === false) {{
                            el.checked = value;
                        }} else if (value === 'true' || value === 'false') {{
                            el.checked = value === 'true';
                        }} else {{
                            el.checked = String(value) === el.value;
                        }}
                    }} else if (el.isContentEditable) {{
                        el.innerText = value;
                    }} else {{
                        const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value')?.set
                            || Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')?.set;
                        if (setter) {{
                            setter.call(el, value);
                        }} else {{
                            el.value = value;
                        }}
                    }}
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }}
            }};"#
    )
}

fn url_encode(input: &str) -> String {
    let mut encoded = String::new();
    for b in input.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char);
            }
            b' ' => {
                encoded.push('+');
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", b));
            }
        }
    }
    encoded
}

/// Run a local JavaScript file inside the page context
pub async fn run_script(
    client: &mut CdpClient,
    session_id: &str,
    file_path: &str,
    script_args: &serde_json::Value,
    format: OutputFormat,
    output: Option<&str>,
    track_navigation: bool,
) -> Result<CommandResult> {
    let script_content = std::fs::read_to_string(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read script file '{}': {}", file_path, e))?;

    // Perform auto-navigation if @url or @navigate comments exist at the top of the file
    let mut target_url = None;
    for line in script_content.lines() {
        let trimmed = line.trim_start();
        if let Some(comment) = trimmed.strip_prefix("//").or_else(|| trimmed.strip_prefix('*')) {
            let comment = comment.trim_start();
            if let Some(rest) = comment.strip_prefix("@url") {
                target_url = Some(rest.trim().to_string());
                break;
            } else if let Some(rest) = comment.strip_prefix("@navigate") {
                target_url = Some(rest.trim().to_string());
                break;
            }
        }
    }

    if let Some(ref url) = target_url {
        // Interpolate {arg_name} placeholders from script_args
        let mut interpolated_url = url.clone();
        if let Some(obj) = script_args.as_object() {
            for (key, val) in obj {
                let placeholder = format!("{{{}}}", key);
                let val_str = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let encoded_val = url_encode(&val_str);
                interpolated_url = interpolated_url.replace(&placeholder, &encoded_val);
            }
        }

        let nav_url = if interpolated_url.starts_with("http://") || interpolated_url.starts_with("https://") {
            interpolated_url.clone()
        } else if is_local_host(&interpolated_url) {
            format!("http://{}", interpolated_url)
        } else {
            format!("https://{}", interpolated_url)
        };

        let current_url = client.current_url(session_id).await?;
        if current_url != nav_url {
            eprintln!("[script] Current URL '{}' does not match target URL '{}'. Auto-navigating...", current_url, nav_url);

            crate::commands::navigate::navigate(
                client,
                session_id,
                Some(&nav_url),
                false,
                false,
                false,
                None,
                None,
            )
            .await?;

            let post_nav_url = client.current_url(session_id).await?;
            if post_nav_url != nav_url {
                anyhow::bail!(
                    "Auto-navigation to '{}' resulted in URL '{}' which does not match target URL",
                    nav_url,
                    post_nav_url
                );
            }
        }
    }

    let args_str = serde_json::to_string(script_args)?;
    let ctx = build_ctx_object(&args_str);

    let iife = format!(
        r#"(async () => {{
            {ctx}

            {script_content}
        }})()"#
    );

    evaluate(client, session_id, &iife, format, output, track_navigation).await
}

/// Extract `@domain` JSDoc comments from a script.
///
/// Only genuine metadata comment lines (`// @domain ...` or the `* @domain ...`
/// JSDoc continuation form) are honored. Matching a bare `@domain` substring
/// would otherwise pick up the marker from string literals or prose elsewhere
/// in the adapter source.
fn parse_adapter_domains(content: &str) -> Vec<String> {
    let mut domains = Vec::new();
    for line in content.lines() {
        // Require the line to be a comment before looking for the marker.
        let trimmed = line.trim_start();
        let comment = match trimmed.strip_prefix("//").or_else(|| trimmed.strip_prefix('*')) {
            Some(rest) => rest.trim_start(),
            None => continue,
        };

        // The marker must lead the comment body and be followed by whitespace so
        // tokens like `@domainname` or `foo@domain.com` do not match.
        let Some(rest) = comment.strip_prefix("@domain") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }

        let domain = rest.split_whitespace().next().unwrap_or("");
        if !domain.is_empty() {
            domains.push(domain.to_string());
        }
    }
    domains
}

/// Strip scheme, path, and port from a raw URL/host string, returning the bare
/// lowercased hostname.
fn normalize_host(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    let without_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let host = without_scheme.split('/').next().unwrap_or(without_scheme);
    let host = if host.starts_with('[') {
        if let Some(idx) = host.rfind(']') {
            &host[..=idx]
        } else {
            host
        }
    } else if host.matches(':').count() > 1 {
        host
    } else {
        host.split(':').next().unwrap_or(host)
    };
    host.to_string()
}

/// Detect loopback / local-dev hosts that should default to plain HTTP during
/// auto-navigation, since they typically don't serve HTTPS.
fn is_local_host(domain: &str) -> bool {
    let host = normalize_host(domain);
    host == "localhost"
        || host == "127.0.0.1"
        || host == "0.0.0.0"
        || host == "[::1]"
        || host == "::1"
        || host.ends_with(".localhost")
}

/// Check if a URL matches a domain pattern.
///
/// Both sides are normalized to a bare hostname first, so an adapter `@domain`
/// written as `https://example.com` or `example.com/path` still matches the
/// page host instead of forcing a spurious auto-navigation.
fn url_matches_domain(url: &str, domain: &str) -> bool {
    let host = normalize_host(url);
    let domain = normalize_host(domain);
    if domain.is_empty() {
        return false;
    }

    host == domain || host.ends_with(&format!(".{}", domain))
}

/// True for characters allowed inside a JavaScript identifier (after the first).
fn is_js_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// Validate that `name` is a plain JavaScript identifier.
///
/// The adapter function name is interpolated directly into the injected IIFE, so
/// rejecting anything that isn't an identifier prevents both syntax errors and
/// code injection through a crafted `function_name`.
fn is_valid_js_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        // A leading digit (or any non-identifier-start char) is invalid.
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(is_js_ident_char)
}

/// Normalize ES-module `export` keywords out of adapter source.
///
/// Adapters are injected as statements into an async IIFE, where a top-level
/// `export` is a SyntaxError. The supported adapter format is plain function
/// declarations; this strips a leading `export` / `export default` so the common
/// authoring habit parses instead of failing before the function-existence check.
///
/// The prefix is only stripped when it directly precedes a declaration keyword.
/// This avoids corrupting `export { ... }` re-export blocks or stray `export`
/// text inside multi-line strings/comments.
fn strip_export_keywords(content: &str) -> String {
    const DECL_KEYWORDS: [&str; 6] = ["function", "async", "class", "const", "let", "var"];
    let declaration_follows = |rest: &str| {
        let rest = rest.trim_start();
        DECL_KEYWORDS.iter().any(|kw| match rest.strip_prefix(kw) {
            // The keyword must end at a non-identifier boundary so `constant`
            // is not mistaken for `const`.
            Some(after) => match after.chars().next() {
                Some(c) => !is_js_ident_char(c),
                None => true,
            },
            None => false,
        })
    };

    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let indent = &line[..line.len() - trimmed.len()];
            if let Some(rest) = trimmed.strip_prefix("export default ") {
                if declaration_follows(rest) {
                    return format!("{indent}{rest}");
                }
            } else if let Some(rest) = trimmed.strip_prefix("export ") {
                if declaration_follows(rest) {
                    return format!("{indent}{rest}");
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run a structured custom adapter function inside the page context
pub async fn run_adapter(
    client: &mut CdpClient,
    session_id: &str,
    file_path: &str,
    function_name: &str,
    script_args: &serde_json::Value,
    format: OutputFormat,
    output: Option<&str>,
    track_navigation: bool,
) -> Result<CommandResult> {
    // `function_name` is interpolated straight into the injected IIFE, so reject
    // anything that isn't a plain identifier before touching Chrome or the disk.
    if !is_valid_js_identifier(function_name) {
        anyhow::bail!(
            "Invalid adapter function name '{}': must be a valid JavaScript identifier",
            function_name
        );
    }

    let script_content = std::fs::read_to_string(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read adapter file '{}': {}", file_path, e))?;

    // Perform domain protection
    let domains = parse_adapter_domains(&script_content);
    if !domains.is_empty() {
        let current_url = client.current_url(session_id).await?;
        let matched = domains.iter().any(|domain| url_matches_domain(&current_url, domain));

        if !matched {
            let target_domain = &domains[0];
            // Preserve the host exactly as declared in `@domain`; only supply a
            // scheme when one is missing. Forcing a `www.` subdomain breaks apex
            // hosts and adapters that target an existing subdomain
            // (e.g. `creator.xiaohongshu.com`).
            let target_url = if target_domain.starts_with("http://") || target_domain.starts_with("https://") {
                // An explicit scheme always wins, so authors can force http/https
                // by writing it in `@domain` (e.g. `@domain http://localhost:3000`).
                target_domain.clone()
            } else if is_local_host(target_domain) {
                // Local dev servers generally speak http, not https.
                format!("http://{}", target_domain)
            } else {
                format!("https://{}", target_domain)
            };
            eprintln!("[adapter] Current URL '{}' does not match adapter domains {:?}. Auto-navigating to '{}'...", current_url, domains, target_url);
            
            crate::commands::navigate::navigate(
                client,
                session_id,
                Some(&target_url),
                false,
                false,
                false,
                None,
                None,
            )
            .await?;

            let post_nav_url = client.current_url(session_id).await?;
            let post_matched = domains.iter().any(|domain| url_matches_domain(&post_nav_url, domain));
            if !post_matched {
                anyhow::bail!(
                    "Auto-navigation to '{}' resulted in URL '{}' which does not match adapter domains {:?}",
                    target_url,
                    post_nav_url,
                    domains
                );
            }
        }
    }

    // Normalize away `export` so module-style adapter declarations parse when
    // injected as statements below. Domain parsing above used the raw source,
    // which is unaffected (domains live in comments).
    let script_content = strip_export_keywords(&script_content);

    let args_str = serde_json::to_string(script_args)?;
    let ctx = build_ctx_object(&args_str);

    let iife = format!(
        r#"(async () => {{
            {ctx}

            {script_content}

            if (typeof {function_name} !== 'function') {{
                throw new Error("Function '{function_name}' not found in adapter");
            }}
            return await {function_name}(ctx);
        }})()"#
    );

    evaluate(client, session_id, &iife, format, output, track_navigation).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_adapter_domains() {
        let content = r#"
            // ==UserAdapter==
            // @name         Xiaohongshu Custom Adapter
            // @domain       xiaohongshu.com
            // @domain       creator.xiaohongshu.com
            // ==/UserAdapter==
        "#;
        let domains = parse_adapter_domains(content);
        assert_eq!(domains, vec!["xiaohongshu.com", "creator.xiaohongshu.com"]);
    }

    #[test]
    fn test_parse_adapter_domains_jsdoc_block() {
        // The `* @domain` JSDoc continuation form is also honored.
        let content = "/**\n * @domain example.com\n */";
        assert_eq!(parse_adapter_domains(content), vec!["example.com"]);
    }

    #[test]
    fn test_parse_adapter_domains_ignores_non_metadata() {
        // Only genuine comment metadata lines count: string literals, prose, and
        // tokens like `@domainname` must not be picked up.
        let content = r#"
            // @domain real.com
            const note = "send mail to user@domain.com";
            // contact foo@domain.org for help
            // @domainname not-a-real-marker.com
            const x = "@domain inside-string.com";
        "#;
        assert_eq!(parse_adapter_domains(content), vec!["real.com"]);
    }

    #[test]
    fn test_strip_export_keywords() {
        let src = "export async function ask(ctx) {}\n  export function read() {}\nexport const helper = 1;\nexport default function main() {}\nconst x = \"export inside string\";";
        let out = strip_export_keywords(src);
        assert_eq!(
            out,
            "async function ask(ctx) {}\n  function read() {}\nconst helper = 1;\nfunction main() {}\nconst x = \"export inside string\";"
        );
    }

    #[test]
    fn test_strip_export_keywords_preserves_non_declarations() {
        // Re-export blocks, `export *`, and prose that merely starts with the
        // word must be left untouched (only declarations are stripped).
        let src = "export { ask, read };\nexport * from './x';\nexport const ok = 1;\nexport constants = 2;";
        let out = strip_export_keywords(src);
        assert_eq!(
            out,
            "export { ask, read };\nexport * from './x';\nconst ok = 1;\nexport constants = 2;"
        );
    }

    #[test]
    fn test_is_valid_js_identifier() {
        assert!(is_valid_js_identifier("ask"));
        assert!(is_valid_js_identifier("_private"));
        assert!(is_valid_js_identifier("$dollar"));
        assert!(is_valid_js_identifier("readWiki2"));
        assert!(!is_valid_js_identifier(""));
        assert!(!is_valid_js_identifier("2fast"));
        assert!(!is_valid_js_identifier("foo.bar"));
        assert!(!is_valid_js_identifier("foo(); evil"));
        assert!(!is_valid_js_identifier("foo bar"));
    }

    #[test]
    fn test_is_local_host() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("localhost:3000"));
        assert!(is_local_host("127.0.0.1:8080"));
        assert!(is_local_host("[::1]"));
        assert!(is_local_host("[::1]:8080"));
        assert!(is_local_host("::1"));
        assert!(is_local_host("app.localhost"));
        assert!(is_local_host("http://localhost:5173/path"));
        assert!(!is_local_host("example.com"));
        assert!(!is_local_host("notlocalhost.com"));
    }

    #[test]
    fn test_url_matches_domain() {
        assert!(url_matches_domain("https://www.xiaohongshu.com/explore", "xiaohongshu.com"));
        assert!(url_matches_domain("http://creator.xiaohongshu.com", "creator.xiaohongshu.com"));
        assert!(url_matches_domain("https://xiaohongshu.com:8080/path", "xiaohongshu.com"));
        assert!(url_matches_domain("http://[::1]:3000", "[::1]"));
        assert!(!url_matches_domain("https://google.com", "xiaohongshu.com"));
    }

    #[test]
    fn test_url_matches_domain_normalizes_domain() {
        // `@domain` written with a scheme and/or path still matches the host.
        assert!(url_matches_domain("https://www.example.com/page", "https://example.com"));
        assert!(url_matches_domain("https://example.com/explore", "example.com/path"));
        assert!(url_matches_domain("https://example.com", "http://example.com:443/"));
        assert!(!url_matches_domain("https://example.com", ""));
    }

    #[test]
    fn test_build_ctx_object_embeds_args_and_helpers() {
        let ctx = build_ctx_object(r#"{"query":"hi"}"#);
        assert!(ctx.starts_with("const ctx = {"));
        assert!(ctx.contains(r#"args: {"query":"hi"}"#));
        for helper in ["wait:", "waitForText:", "waitForSelector:", "click:", "fill:"] {
            assert!(ctx.contains(helper), "missing helper: {helper}");
        }
        // fill must special-case checkable inputs instead of setting `value`.
        assert!(ctx.contains("el.type === 'checkbox' || el.type === 'radio'"));
        assert!(ctx.contains("el.checked ="));
        // fill must support contenteditable elements.
        assert!(ctx.contains("el.isContentEditable"));
        assert!(ctx.contains("el.innerText ="));
    }
}

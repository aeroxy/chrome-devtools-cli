use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;
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
        anyhow::bail!(
            "{desc}\n\n[HINT: To explore the page DOM, use the `snapshot` command instead of `evaluate`. To interact with elements, use `click` or `fill`.]"
        );
    }

    let value = &result["result"];
    let val_type = value["type"].as_str().unwrap_or("undefined");

    let output_hint = if format.is_text() {
        let mut text = match val_type {
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

        if expression.contains("querySelector")
            || expression.contains("document.body")
            || expression.contains("getElementById")
            || expression.contains("getElementsBy")
        {
            text.push_str("\n\n[HINT: Avoid using `evaluate` for DOM traversal. Use the `snapshot` command to get a clean accessibility tree of the page, then use `click` or `fill`.]");
        }
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

    let args_str = serde_json::to_string(script_args)?;

    let iife = format!(
        r#"(async () => {{
            const ctx = {{
                args: {args_str},
                wait: async (ms) => new Promise(r => setTimeout(r, ms)),
                waitForText: async (text, timeout = 30000) => {{
                    const start = Date.now();
                    while (Date.now() - start < timeout) {{
                        if (document.body && document.body.innerText.includes(text)) return;
                        await new Promise(r => setTimeout(r, 100));
                    }}
                    throw new Error("Timeout waiting for text: " + text);
                }},
                waitForSelector: async (selector, timeout = 30000) => {{
                    const start = Date.now();
                    while (Date.now() - start < timeout) {{
                        if (document.querySelector(selector)) return;
                        await new Promise(r => setTimeout(r, 100));
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
                    el.value = value;
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }}
            }};

            {script_content}
        }})()"#
    );

    evaluate(client, session_id, &iife, format, output, track_navigation).await
}

/// Extract `@domain` JSDoc comments from a script
fn parse_adapter_domains(content: &str) -> Vec<String> {
    let mut domains = Vec::new();
    for line in content.lines() {
        if let Some(pos) = line.find("@domain") {
            let rest = &line[pos + 7..];
            let domain = rest.trim().split_whitespace().next().unwrap_or("");
            if !domain.is_empty() {
                domains.push(domain.to_string());
            }
        }
    }
    domains
}

/// Check if a URL matches a domain pattern
fn url_matches_domain(url: &str, domain: &str) -> bool {
    let url_lower = url.to_lowercase();
    let domain_lower = domain.to_lowercase();
    
    let s = url_lower
        .strip_prefix("https://")
        .or_else(|| url_lower.strip_prefix("http://"))
        .unwrap_or(&url_lower);
        
    let host = s.split('/').next().unwrap_or(s);
    let host = host.split(':').next().unwrap_or(host);
    
    host == domain_lower || host.ends_with(&format!(".{}", domain_lower))
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
    let script_content = std::fs::read_to_string(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read adapter file '{}': {}", file_path, e))?;

    // Perform domain protection
    let domains = parse_adapter_domains(&script_content);
    if !domains.is_empty() {
        let current_url = client.current_url(session_id).await?;
        let matched = domains.iter().any(|domain| url_matches_domain(&current_url, domain));

        if !matched {
            let target_domain = &domains[0];
            let target_url = if target_domain.starts_with("http://") || target_domain.starts_with("https://") {
                target_domain.clone()
            } else {
                format!("https://www.{}", target_domain)
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
        }
    }

    let args_str = serde_json::to_string(script_args)?;

    let iife = format!(
        r#"(async () => {{
            const ctx = {{
                args: {args_str},
                wait: async (ms) => new Promise(r => setTimeout(r, ms)),
                waitForText: async (text, timeout = 30000) => {{
                    const start = Date.now();
                    while (Date.now() - start < timeout) {{
                        if (document.body && document.body.innerText.includes(text)) return;
                        await new Promise(r => setTimeout(r, 100));
                    }}
                    throw new Error("Timeout waiting for text: " + text);
                }},
                waitForSelector: async (selector, timeout = 30000) => {{
                    const start = Date.now();
                    while (Date.now() - start < timeout) {{
                        if (document.querySelector(selector)) return;
                        await new Promise(r => setTimeout(r, 100));
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
                    el.value = value;
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }}
            }};

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
    fn test_url_matches_domain() {
        assert!(url_matches_domain("https://www.xiaohongshu.com/explore", "xiaohongshu.com"));
        assert!(url_matches_domain("http://creator.xiaohongshu.com", "creator.xiaohongshu.com"));
        assert!(url_matches_domain("https://xiaohongshu.com:8080/path", "xiaohongshu.com"));
        assert!(!url_matches_domain("https://google.com", "xiaohongshu.com"));
    }
}

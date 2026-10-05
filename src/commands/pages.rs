use anyhow::Result;
use serde_json::json;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::friendly;
use crate::result::CommandResult;

/// Apply extra HTTP headers to a page session via Network.setExtraHTTPHeaders.
pub async fn apply_extra_headers(
    client: &mut CdpClient,
    session_id: &str,
    extra_headers: Option<&str>,
) -> Result<()> {
    if let Some(headers_json) = extra_headers {
        let headers: serde_json::Value = serde_json::from_str(headers_json)
            .map_err(|e| anyhow::anyhow!("Invalid --extra-headers JSON: {e}"))?;
        let headers_obj = headers
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("--extra-headers must be a JSON object"))?;
        let mut converted_headers = serde_json::Map::new();
        for (k, v) in headers_obj {
            let val_str = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => anyhow::bail!(
                    "Header value for '{}' must be a primitive scalar (string, number, or boolean)",
                    k
                ),
            };
            converted_headers.insert(k.clone(), serde_json::Value::String(val_str));
        }
        client
            .send_to_target(session_id, "Network.enable", json!({}))
            .await?;
        client
            .send_to_target(
                session_id,
                "Network.setExtraHTTPHeaders",
                json!({"headers": converted_headers}),
            )
            .await?;
    }
    Ok(())
}

/// Clear extra HTTP headers for a page session.
pub async fn clear_extra_headers(client: &mut CdpClient, session_id: &str) -> Result<()> {
    client
        .send_to_target(
            session_id,
            "Network.setExtraHTTPHeaders",
            json!({"headers": {}}),
        )
        .await?;
    // Undo apply_extra_headers' Network.enable only on a per-command session:
    // the persistent session needs Network for `network` to keep collecting.
    if client.persistent_session.as_deref() != Some(session_id) {
        let _ = client
            .send_to_target(session_id, "Network.disable", json!({}))
            .await;
    }
    Ok(())
}

/// List all open page targets with their friendly names, titles, and URLs.
pub async fn list_pages(client: &mut CdpClient, format: OutputFormat) -> Result<CommandResult> {
    let pages = client.get_page_targets().await?;

    if format.is_text() {
        if pages.is_empty() {
            return Ok(CommandResult::output("No pages open.".to_string()));
        }
        let mut out = String::new();
        for (i, page) in pages.iter().enumerate() {
            let name = friendly::to_friendly(&page.target_id);
            writeln!(out, "[{i}] ({name}) {} — {}", page.title, page.url).unwrap();
        }
        Ok(CommandResult::output(out))
    } else {
        let items: Vec<_> = pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                json!({
                    "index": i,
                    "target": friendly::to_friendly(&p.target_id),
                    "title": p.title,
                    "url": p.url,
                })
            })
            .collect();
        let value = serde_json::to_value(&items)?;
        Ok(CommandResult::output(format_structured(&value, format)?))
    }
}

/// Open a new page, optionally applying emulation and extra headers before navigation.
pub async fn new_page(
    client: &mut CdpClient,
    url: &str,
    emulation: Option<crate::commands::emulation::EmulateParams>,
    extra_headers: Option<&str>,
) -> Result<CommandResult> {
    let target_id = if emulation.is_some() || extra_headers.is_some() {
        // Create blank page so emulation/headers are applied before the real URL loads
        let target_id = client.create_target("about:blank").await?;

        // emulate() records overrides as the state of the persistent session's
        // tab, so they run on the new tab's persistent session, as `navigate`
        // does for an existing tab. A throwaway session would lose them at
        // detach and credit them to whichever tab was active before.
        let result: Result<()> = async {
            client.ensure_persistent_session(&target_id).await?;
            let session_id = client
                .persistent_session
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No persistent session for the new page"))?;
            if let Some(params) = emulation {
                crate::commands::emulation::emulate(client, &session_id, params).await?;
            }
            crate::commands::navigate::navigate(
                client,
                &session_id,
                Some(url),
                false,
                false,
                false,
                extra_headers,
                None,
            )
            .await?;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = client.close_target(&target_id).await;
            client.forget_target(&target_id);
            return Err(e);
        }
        target_id
    } else {
        client.create_target(url).await?
    };

    Ok(opened_page(url, &target_id))
}

/// Name the new tab the way `list-pages` does. Browser-level commands skip the
/// step that tags page commands with their target, so the tag is set here to
/// give `new-page` the same `[target:name]` line.
fn opened_page(url: &str, target_id: &str) -> CommandResult {
    let friendly_name = friendly::to_friendly(target_id);
    let mut result =
        CommandResult::output(format!("Opened new page: {url} (target: {friendly_name})"));
    result.target_id = Some(friendly_name);
    result
}

/// Close a page target by its target ID.
pub async fn close_page(client: &mut CdpClient, target_id: &str) -> Result<CommandResult> {
    client.close_target(target_id).await?;
    // Drop the closed tab's saved emulation state so it doesn't linger.
    client.forget_target(target_id);
    let friendly_name = friendly::to_friendly(target_id);
    Ok(CommandResult::output(format!(
        "Closed page: {friendly_name}"
    )))
}

/// Activate (bring to front) a page target by its target ID.
pub async fn select_page(client: &mut CdpClient, target_id: &str) -> Result<CommandResult> {
    client.activate_target(target_id).await?;
    let friendly_name = friendly::to_friendly(target_id);
    let mut result = CommandResult::output(format!("Activated page: {friendly_name}"));
    result.target_id = Some(friendly_name);
    Ok(result)
}

/// Wait until the page body contains the given text, or timeout.
pub async fn wait_for(
    client: &mut CdpClient,
    session_id: &str,
    text: &str,
    timeout_ms: u64,
) -> Result<CommandResult> {
    let escaped = serde_json::to_string(text)?;
    let check_expr = format!("document.body && document.body.innerText.includes({escaped})");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Timeout ({timeout_ms}ms) waiting for text: {text}");
        }

        let result = client
            .send_to_target(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": check_expr,
                    "returnByValue": true,
                }),
            )
            .await;

        match result {
            Ok(val) => {
                if val["result"]["value"].as_bool() == Some(true) {
                    return Ok(CommandResult::output(format!("Found text: {text}")));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Execution context was destroyed")
                    || msg.contains("Cannot find context")
                {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
                return Err(anyhow::anyhow!(
                    "wait_for failed for session {session_id}: {e}"
                ));
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Callers pin later commands to the new tab by this name, so it must be the
    /// friendly one `list-pages` shows, and it must reach the `[target:name]` line.
    #[test]
    fn new_page_reports_the_friendly_name() {
        let id = "C08EA3F291931B3333A12DFB7B570CF6";
        let name = friendly::to_friendly(id);
        let result = opened_page("https://example.com", id);
        assert_eq!(
            result.output,
            format!("Opened new page: https://example.com (target: {name})")
        );
        assert_eq!(result.target_id, Some(name));
    }
}

use anyhow::Result;
use serde_json::json;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::constants::NAVIGATION_TIMEOUT_MS;
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
        for (k, v) in headers_obj {
            if !v.is_string() {
                anyhow::bail!("Header value for '{}' must be a string", k);
            }
        }
        client
            .send_to_target(session_id, "Network.enable", json!({}))
            .await?;
        client
            .send_to_target(
                session_id,
                "Network.setExtraHTTPHeaders",
                json!({"headers": headers_obj}),
            )
            .await?;
    }
    Ok(())
}

/// List all open page targets with their friendly names, titles, and URLs.
pub async fn list_pages(client: &mut CdpClient, as_json: bool) -> Result<CommandResult> {
    let pages = client.get_page_targets().await?;

    if as_json {
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
        Ok(CommandResult::output(serde_json::to_string_pretty(&items)?))
    } else {
        if pages.is_empty() {
            return Ok(CommandResult::output("No pages open.".to_string()));
        }
        let mut out = String::new();
        for (i, page) in pages.iter().enumerate() {
            let name = friendly::to_friendly(&page.target_id);
            writeln!(out, "[{i}] ({name}) {} — {}", page.title, page.url).unwrap();
        }
        Ok(CommandResult::output(out))
    }
}

/// Open a new page, optionally applying emulation and extra headers before navigation.
pub async fn new_page(
    client: &mut CdpClient,
    url: &str,
    emulation: Option<crate::commands::emulation::EmulateParams>,
    extra_headers: Option<&str>,
) -> Result<CommandResult> {
    if emulation.is_some() || extra_headers.is_some() {
        // Create blank page so emulation/headers are applied before the real URL loads
        let target_id = client.create_target("about:blank").await?;

        let result: Result<()> = async {
            let session_id = client.attach_to_target(&target_id).await?;

            let inner: Result<()> = async {
                if let Some(params) = emulation {
                    crate::commands::emulation::emulate(client, &session_id, params).await?;
                }
                apply_extra_headers(client, &session_id, extra_headers).await?;
                let nav_result = client
                    .send_to_target(&session_id, "Page.navigate", json!({ "url": url }))
                    .await?;
                if let Some(error_text) = nav_result.get("errorText").and_then(|v| v.as_str()) {
                    anyhow::bail!("Page.navigate failed: {error_text}");
                }
                crate::commands::navigate::wait_for_load(client, &session_id, NAVIGATION_TIMEOUT_MS).await?;
                Ok(())
            }
            .await;

            let _ = client.detach_from_target(&session_id).await;
            inner
        }
        .await;

        if let Err(e) = result {
            let _ = client.close_target(&target_id).await;
            return Err(e);
        }

        Ok(CommandResult::output(format!(
            "Opened new page: {url} (target: {target_id})"
        )))
    } else {
        let target_id = client.create_target(url).await?;
        Ok(CommandResult::output(format!(
            "Opened new page: {url} (target: {target_id})"
        )))
    }
}

/// Close a page target by its target ID.
pub async fn close_page(client: &mut CdpClient, target_id: &str) -> Result<CommandResult> {
    client.close_target(target_id).await?;
    Ok(CommandResult::output(format!("Closed page: {target_id}")))
}

/// Activate (bring to front) a page target by its target ID.
pub async fn select_page(client: &mut CdpClient, target_id: &str) -> Result<CommandResult> {
    client.activate_target(target_id).await?;
    Ok(CommandResult::output(format!("Activated page: {target_id}")))
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
                return Err(anyhow::anyhow!("wait_for failed for session {session_id}: {e}"));
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

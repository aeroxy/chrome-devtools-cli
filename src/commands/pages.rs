use anyhow::{anyhow, Result};
use serde_json::json;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::friendly;
use crate::result::CommandResult;

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

pub async fn new_page(client: &mut CdpClient, url: &str) -> Result<CommandResult> {
    let target_id = client.create_target(url).await?;
    Ok(CommandResult::output(format!(
        "Opened new page: {url} (target: {target_id})"
    )))
}

pub async fn close_page(client: &mut CdpClient, index: usize) -> Result<CommandResult> {
    let pages = client.get_page_targets().await?;
    let page = pages
        .get(index)
        .ok_or_else(|| anyhow!("No page at index {index} (have {} pages)", pages.len()))?;
    client.close_target(&page.target_id).await?;
    Ok(CommandResult::output(format!(
        "Closed page [{index}]: {}",
        page.url
    )))
}

pub async fn select_page(client: &mut CdpClient, index: usize) -> Result<CommandResult> {
    let pages = client.get_page_targets().await?;
    let page = pages
        .get(index)
        .ok_or_else(|| anyhow!("No page at index {index} (have {} pages)", pages.len()))?;
    client.activate_target(&page.target_id).await?;
    Ok(CommandResult::output(format!(
        "Activated page [{index}]: {} — {}",
        page.title, page.url
    )))
}

pub async fn resize(
    client: &mut CdpClient,
    session_id: &str,
    width: u32,
    height: u32,
) -> Result<CommandResult> {
    client
        .send_to_target(
            session_id,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
        )
        .await?;
    Ok(CommandResult::output(format!(
        "Resized viewport to {width}x{height}"
    )))
}

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

        if let Ok(val) = result {
            if val["result"]["value"].as_bool() == Some(true) {
                return Ok(CommandResult::output(format!("Found text: {text}")));
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

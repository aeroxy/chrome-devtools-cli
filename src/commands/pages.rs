use anyhow::Result;
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

pub async fn new_page(
    client: &mut CdpClient,
    url: &str,
    emulation: Option<crate::commands::emulation::EmulateParams>,
) -> Result<CommandResult> {
    if let Some(params) = emulation {
        // Create an empty page first so we can apply emulation before it loads the real URL
        let target_id = client.create_target("about:blank").await?;
        let session_id = client.attach_to_target(&target_id).await?;

        // Use a block to ensure detachment and closure occurs even if emulation or navigation fails
        let result: Result<()> = async {
            crate::commands::emulation::emulate(client, &session_id, params).await?;
            client
                .send_to_target(&session_id, "Page.navigate", json!({ "url": url }))
                .await?;
            // Wait for load (consistent with navigate command)
            crate::commands::navigate::wait_for_load(client, &session_id, 30_000).await?;
            Ok(())
        }
        .await;

        let _ = client.detach_from_target(&session_id).await;
        if result.is_err() {
            let _ = client.close_target(&target_id).await;
            return Err(result.unwrap_err());
        }

        Ok(CommandResult::output(format!(
            "Opened new page with emulation: {url} (target: {target_id})"
        )))
    } else {
        let target_id = client.create_target(url).await?;
        Ok(CommandResult::output(format!(
            "Opened new page: {url} (target: {target_id})"
        )))
    }
}

pub async fn close_page(client: &mut CdpClient, target_id: &str) -> Result<CommandResult> {
    client.close_target(target_id).await?;
    Ok(CommandResult::output(format!("Closed page: {target_id}")))
}

pub async fn select_page(client: &mut CdpClient, target_id: &str) -> Result<CommandResult> {
    client.activate_target(target_id).await?;
    Ok(CommandResult::output(format!("Activated page: {target_id}")))
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
            .await?;

        if result["result"]["value"].as_bool() == Some(true) {
            return Ok(CommandResult::output(format!("Found text: {text}")));
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

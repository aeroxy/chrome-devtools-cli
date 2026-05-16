use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

pub async fn navigate(
    client: &mut CdpClient,
    session_id: &str,
    url: Option<&str>,
    back: bool,
    forward: bool,
    reload: bool,
    output: Option<&str>,
) -> Result<CommandResult> {
    if back {
        return go_back(client, session_id, output).await;
    }
    if forward {
        return go_forward(client, session_id, output).await;
    }
    if reload {
        return do_reload(client, session_id, output).await;
    }

    let url =
        url.ok_or_else(|| anyhow::anyhow!("URL required (or use --back, --forward, --reload)"))?;

    let result = client
        .send_to_target(session_id, "Page.navigate", json!({"url": url}))
        .await?;

    if let Some(err) = result.get("errorText").and_then(|v| v.as_str()) {
        bail!("Navigation error: {err}");
    }

    wait_for_load(client, session_id, 30_000).await?;
    let result = CommandResult::output(format!("Navigated to {url}"))
        .with_navigated_to(url.to_string());

    if let Some(path) = output {
        tokio::fs::write(path, result.output.as_bytes()).await?;
        Ok(CommandResult::output(format!("Output saved to {path}")).with_navigated_to(url.to_string()))
    } else {
        Ok(result)
    }
}

async fn go_back(client: &mut CdpClient, session_id: &str, output: Option<&str>) -> Result<CommandResult> {
    let history = client
        .send_to_target(session_id, "Page.getNavigationHistory", json!({}))
        .await?;

    let current_index = history["currentIndex"].as_i64().unwrap_or(0);
    let entries = history["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No navigation history entries"))?;

    if current_index <= 0 {
        bail!("Already at the beginning of history");
    }

    let prev_entry = &entries[(current_index - 1) as usize];
    let entry_id = prev_entry["id"].as_i64().unwrap_or(0);

    client
        .send_to_target(
            session_id,
            "Page.navigateToHistoryEntry",
            json!({"entryId": entry_id}),
        )
        .await?;

    wait_for_load(client, session_id, 30_000).await?;
    let url = prev_entry["url"].as_str().unwrap_or("unknown");
    let result = CommandResult::output(format!("Navigated back to {url}"))
        .with_navigated_to(url.to_string());
    if let Some(path) = output {
        tokio::fs::write(path, result.output.as_bytes()).await?;
        Ok(CommandResult::output(format!("Output saved to {path}")).with_navigated_to(url.to_string()))
    } else {
        Ok(result)
    }
}

async fn go_forward(client: &mut CdpClient, session_id: &str, output: Option<&str>) -> Result<CommandResult> {
    let history = client
        .send_to_target(session_id, "Page.getNavigationHistory", json!({}))
        .await?;

    let current_index = history["currentIndex"].as_i64().unwrap_or(0) as usize;
    let entries = history["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No navigation history entries"))?;

    if current_index + 1 >= entries.len() {
        bail!("Already at the end of history");
    }

    let next_entry = &entries[current_index + 1];
    let entry_id = next_entry["id"].as_i64().unwrap_or(0);

    client
        .send_to_target(
            session_id,
            "Page.navigateToHistoryEntry",
            json!({"entryId": entry_id}),
        )
        .await?;

    wait_for_load(client, session_id, 30_000).await?;
    let url = next_entry["url"].as_str().unwrap_or("unknown");
    let result = CommandResult::output(format!("Navigated forward to {url}"))
        .with_navigated_to(url.to_string());
    if let Some(path) = output {
        tokio::fs::write(path, result.output.as_bytes()).await?;
        Ok(CommandResult::output(format!("Output saved to {path}")).with_navigated_to(url.to_string()))
    } else {
        Ok(result)
    }
}

async fn do_reload(client: &mut CdpClient, session_id: &str, output: Option<&str>) -> Result<CommandResult> {
    client
        .send_to_target(session_id, "Page.reload", json!({}))
        .await?;
    wait_for_load(client, session_id, 30_000).await?;
    let url = client.current_url(session_id).await?;
    let result = CommandResult::output("Reloaded page".to_string())
        .with_navigated_to(url.clone());
    if let Some(path) = output {
        tokio::fs::write(path, result.output.as_bytes()).await?;
        Ok(CommandResult::output(format!("Output saved to {path}")).with_navigated_to(url))
    } else {
        Ok(result)
    }
}

async fn wait_for_load(client: &mut CdpClient, session_id: &str, timeout_ms: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    loop {
        if tokio::time::Instant::now() > deadline {
            eprintln!("Warning: page did not reach readyState=complete within {timeout_ms}ms");
            return Ok(());
        }

        let result = client
            .send_to_target(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "document.readyState",
                    "returnByValue": true,
                }),
            )
            .await;

        if let Ok(val) = result {
            if val["result"]["value"].as_str() == Some("complete") {
                return Ok(());
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

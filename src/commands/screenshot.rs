use anyhow::Result;
use base64::Engine;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

pub async fn take_screenshot(
    client: &mut CdpClient,
    session_id: &str,
    output: Option<&str>,
    format: &str,
    full_page: bool,
) -> Result<CommandResult> {
    let mut params = json!({
        "format": format,
        "optimizeForSpeed": true,
    });
    if full_page {
        params["captureBeyondViewport"] = json!(true);
        let metrics = client
            .send_to_target(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "JSON.stringify({width: document.documentElement.scrollWidth, height: document.documentElement.scrollHeight})",
                    "returnByValue": true,
                }),
            )
            .await?;
        if let Some(val) = metrics["result"]["value"].as_str() {
            if let Ok(dims) = serde_json::from_str::<serde_json::Value>(val) {
                let w = dims["width"].as_f64().unwrap_or(1920.0);
                let h = dims["height"].as_f64().unwrap_or(1080.0);
                params["clip"] = json!({
                    "x": 0, "y": 0,
                    "width": w, "height": h,
                    "scale": 1,
                });
            }
        }
    }

    let result = client
        .send_to_target(session_id, "Page.captureScreenshot", params)
        .await?;

    let data_b64 = result["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No screenshot data in response"))?;

    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64)?;

    match output {
        Some(path) => {
            tokio::fs::write(path, &bytes).await?;
            Ok(CommandResult::output(format!(
                "Screenshot saved to {path} ({} bytes)",
                bytes.len()
            )))
        }
        None => {
            // Return raw base64 (agent can read it)
            Ok(CommandResult::output(data_b64.to_string()))
        }
    }
}

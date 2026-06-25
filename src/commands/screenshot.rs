use anyhow::Result;
use base64::Engine;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Capture a screenshot of the current page.
pub async fn take_screenshot(
    client: &mut CdpClient,
    session_id: &str,
    output: Option<&str>,
    format: &str,
    full_page: bool,
    quality: Option<u64>,
    max_width: Option<f64>,
    max_height: Option<f64>,
) -> Result<CommandResult> {
    let mut params = json!({
        "format": format,
        "optimizeForSpeed": true,
    });

    if let Some(q) = quality {
        if format != "png" {
            params["quality"] = json!(q.min(100));
        }
    }

    // Determine the viewport or document scroll dimensions
    let mut src_w = 1920.0;
    let mut src_h = 1080.0;
    let is_full_page = full_page;

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
                src_w = dims["width"].as_f64().unwrap_or(1920.0);
                src_h = dims["height"].as_f64().unwrap_or(1080.0);
            }
        }
    } else {
        // Query current viewport bounds if not capturing the full page
        let metrics = client
            .send_to_target(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "JSON.stringify({width: window.innerWidth, height: window.innerHeight})",
                    "returnByValue": true,
                }),
            )
            .await?;
        if let Some(val) = metrics["result"]["value"].as_str() {
            if let Ok(dims) = serde_json::from_str::<serde_json::Value>(val) {
                src_w = dims["width"].as_f64().unwrap_or(1920.0);
                src_h = dims["height"].as_f64().unwrap_or(1080.0);
            }
        }
    }

    // Downscaling logic (calculate custom clip with scale factor)
    let mut clip_scale = 1.0;
    if max_width.is_some() || max_height.is_some() {
        let width_scale = match max_width {
            Some(max_w) if max_w > 0.0 && src_w > 0.0 => (max_w / src_w).min(1.0),
            _ => 1.0,
        };
        let height_scale = match max_height {
            Some(max_h) if max_h > 0.0 && src_h > 0.0 => (max_h / src_h).min(1.0),
            _ => 1.0,
        };
        clip_scale = width_scale.min(height_scale);
    }

    if is_full_page || clip_scale < 1.0 {
        params["clip"] = json!({
            "x": 0, "y": 0,
            "width": src_w, "height": src_h,
            "scale": clip_scale,
        });
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
        None => Ok(CommandResult::output(data_b64.to_string())),
    }
}

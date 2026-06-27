use anyhow::{Context, Result};
use base64::Engine;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Capture a screenshot of the current page.
pub struct ScreenshotOptions {
    pub output: Option<String>,
    pub format: String,
    pub full_page: bool,
    pub quality: Option<u64>,
    pub max_width: Option<f64>,
    pub max_height: Option<f64>,
}

pub async fn take_screenshot(
    client: &mut CdpClient,
    session_id: &str,
    opts: ScreenshotOptions,
) -> Result<CommandResult> {
    let ScreenshotOptions {
        output,
        format,
        full_page,
        quality,
        max_width,
        max_height,
    } = opts;
    // Normalize so case-insensitive input (e.g. "PNG") is handled correctly:
    // CDP expects lowercase format values, and the quality check below relies on it.
    let format = format.to_ascii_lowercase();
    let mut params = json!({
        "format": format,
    });

    // optimizeForSpeed trades compression/quality for speed, which would override
    // an explicit --quality setting; only enable it when quality isn't requested.
    if quality.is_none() {
        params["optimizeForSpeed"] = json!(true);
    }

    if let Some(q) = quality {
        if format != "png" {
            params["quality"] = json!(q.min(100));
        }
    }

    // src_w/src_h are only needed when a clip will be emitted
    // (full-page capture, or downscaling via max_width/max_height).
    let mut src_w = 1920.0;
    let mut src_h = 1080.0;
    // Scroll offsets of the layout viewport. Clip x/y are relative to the
    // document origin, so a non-full-page capture of a scrolled viewport must
    // use these to frame the visible region.
    let mut scroll_x = 0.0;
    let mut scroll_y = 0.0;
    let needs_metrics = full_page || max_width.is_some() || max_height.is_some();

    if needs_metrics {
        if full_page {
            params["captureBeyondViewport"] = json!(true);
        }

        // Use Page.getLayoutMetrics instead of Runtime.evaluate: it queries the
        // renderer's layout system directly, works on non-HTML pages (PDF viewers,
        // chrome://), and avoids a JS execution round-trip.
        let metrics = client
            .send_to_target(session_id, "Page.getLayoutMetrics", json!({}))
            .await
            .context("Failed to query page layout metrics")?;

        if full_page {
            // contentSize is the full scrollable content area.
            if let Some(size) = metrics.get("contentSize") {
                // Filter non-positive values (empty/unrendered pages, certain
                // document types) — they'd produce an invalid CDP clip.
                src_w = size["width"].as_f64().filter(|&v| v > 0.0).unwrap_or(1920.0);
                src_h = size["height"].as_f64().filter(|&v| v > 0.0).unwrap_or(1080.0);
            }
        } else {
            // layoutViewport.clientWidth/Height is the visible viewport.
            if let Some(viewport) = metrics.get("layoutViewport") {
                src_w = viewport["clientWidth"].as_f64().filter(|&v| v > 0.0).unwrap_or(1920.0);
                src_h = viewport["clientHeight"].as_f64().filter(|&v| v > 0.0).unwrap_or(1080.0);
                scroll_x = viewport["pageX"].as_f64().unwrap_or(0.0);
                scroll_y = viewport["pageY"].as_f64().unwrap_or(0.0);
            }
        }
    }

    let clip_scale = clip_scale_factor(src_w, src_h, max_width, max_height);

    // Clip coordinates are relative to the document origin. For full-page
    // captures the region starts at the document origin (scroll is irrelevant
    // since the whole content is captured). For viewport captures with
    // downscaling, the layout viewport's scroll offsets (pageX/pageY) must be
    // used so the visible region — not the document's top-left — is framed.
    if full_page || clip_scale < 1.0 {
        params["clip"] = json!({
            "x": scroll_x, "y": scroll_y,
            "width": src_w, "height": src_h,
            "scale": clip_scale,
        });
    }

    let result = client
        .send_to_target(session_id, "Page.captureScreenshot", params)
        .await
        .context("Failed to capture screenshot via CDP")?;

    let data_b64 = result["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No screenshot data in response"))?;

    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64)?;

    match output {
        Some(path) => {
            tokio::fs::write(&path, &bytes)
                .await
                .with_context(|| format!("Failed to write screenshot to {}", path))?;
            Ok(CommandResult::output(format!(
                "Screenshot saved to {path} ({} bytes)",
                bytes.len()
            )))
        }
        None => Ok(CommandResult::output(data_b64.to_string())),
    }
}

/// Compute the downscale factor for a screenshot clip.
///
/// Returns the smaller of the width and height scale ratios, clamped to <= 1.0
/// (never upscales). A `None` dimension, or a non-positive max/src value, yields
/// 1.0 for that axis (no scaling). Returns 1.0 when neither dimension is set.
fn clip_scale_factor(src_w: f64, src_h: f64, max_width: Option<f64>, max_height: Option<f64>) -> f64 {
    let width_scale = match max_width {
        Some(max_w) if max_w > 0.0 && src_w > 0.0 => (max_w / src_w).min(1.0),
        _ => 1.0,
    };
    let height_scale = match max_height {
        Some(max_h) if max_h > 0.0 && src_h > 0.0 => (max_h / src_h).min(1.0),
        _ => 1.0,
    };
    width_scale.min(height_scale)
}

#[cfg(test)]
mod tests {
    use super::clip_scale_factor;

    #[test]
    fn no_max_dimensions_returns_one() {
        assert_eq!(clip_scale_factor(1920.0, 1080.0, None, None), 1.0);
    }

    #[test]
    fn zero_max_is_treated_as_no_scaling() {
        assert_eq!(clip_scale_factor(1920.0, 1080.0, Some(0.0), Some(0.0)), 1.0);
    }

    #[test]
    fn negative_max_is_treated_as_no_scaling() {
        assert_eq!(clip_scale_factor(1920.0, 1080.0, Some(-100.0), Some(-50.0)), 1.0);
    }

    #[test]
    fn zero_source_is_treated_as_no_scaling() {
        assert_eq!(clip_scale_factor(0.0, 0.0, Some(100.0), Some(100.0)), 1.0);
    }

    #[test]
    fn one_sided_width_downscales_only_width() {
        // src 1920x1080, max_width 960 → width_scale 0.5, height_scale 1.0 → 0.5
        assert_eq!(clip_scale_factor(1920.0, 1080.0, Some(960.0), None), 0.5);
    }

    #[test]
    fn one_sided_height_downscales_only_height() {
        // src 1920x1080, max_height 540 → height_scale 0.5, width_scale 1.0 → 0.5
        assert_eq!(clip_scale_factor(1920.0, 1080.0, None, Some(540.0)), 0.5);
    }

    #[test]
    fn both_dimensions_uses_the_smaller_ratio() {
        // src 2000x1000, max 1000x250 → width_scale 0.5, height_scale 0.25 → 0.25
        assert_eq!(clip_scale_factor(2000.0, 1000.0, Some(1000.0), Some(250.0)), 0.25);
    }

    #[test]
    fn never_upscales_when_max_exceeds_source() {
        // src 800x600, max 1600x1200 → both ratios > 1.0, clamped to 1.0
        assert_eq!(clip_scale_factor(800.0, 600.0, Some(1600.0), Some(1200.0)), 1.0);
    }
}

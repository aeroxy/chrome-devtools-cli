use anyhow::{Context, Result};
use base64::Engine;
use serde_json::json;

use crate::cdp::{is_connection_or_page_failure, CdpClient, DialogOpen};
use crate::result::CommandResult;

/// Chrome's GPU texture limit: past this many device pixels in either
/// direction, captured content repeats or comes out corrupt.
const MAX_CAPTURE_DIMENSION: f64 = 16384.0;

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
    // Device pixels per CSS pixel. Images come out at device resolution, so
    // --max-width/--max-height and the capture limit are in device pixels.
    let mut dpr = 1.0;
    // DIPs per CSS pixel: the browser zoom, which the clip has to include.
    let mut zoom = 1.0;
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

        dpr = match page_device_pixel_ratio(client, session_id).await? {
            Some(ratio) => ratio,
            None => device_pixel_ratio(&metrics),
        };
        zoom = browser_zoom(&metrics);
        if full_page {
            // cssContentSize is the full scrollable content area in CSS pixels.
            // (The legacy `contentSize` returns DIPs, which are wrong on HiDPI
            // or emulated devices.)
            if let Some(size) = metrics.get("cssContentSize") {
                // Filter non-positive values (empty/unrendered pages, certain
                // document types) — they'd produce an invalid CDP clip.
                src_w = size["width"]
                    .as_f64()
                    .filter(|&v| v > 0.0)
                    .unwrap_or(1920.0);
                src_h = size["height"]
                    .as_f64()
                    .filter(|&v| v > 0.0)
                    .unwrap_or(1080.0);
            }
        } else {
            // cssLayoutViewport.clientWidth/Height is the visible viewport in
            // CSS pixels; pageX/pageY are its document-origin scroll offsets.
            if let Some(viewport) = metrics.get("cssLayoutViewport") {
                src_w = viewport["clientWidth"]
                    .as_f64()
                    .filter(|&v| v > 0.0)
                    .unwrap_or(1920.0);
                src_h = viewport["clientHeight"]
                    .as_f64()
                    .filter(|&v| v > 0.0)
                    .unwrap_or(1080.0);
                scroll_x = viewport["pageX"].as_f64().unwrap_or(0.0);
                scroll_y = viewport["pageY"].as_f64().unwrap_or(0.0);
            }
        }
    }

    let clip_scale = clip_scale_factor(src_w * dpr, src_h * dpr, max_width, max_height);
    let (image_w, image_h) = (src_w * dpr * clip_scale, src_h * dpr * clip_scale);
    let mut notes = Vec::new();
    if full_page && (image_w > MAX_CAPTURE_DIMENSION || image_h > MAX_CAPTURE_DIMENSION) {
        notes.push(format!(
            "[warning] The screenshot is {:.0}x{:.0}px, past Chrome's {MAX_CAPTURE_DIMENSION}px \
             capture limit, so content beyond it may repeat or be corrupt. Downscale with \
             --max-height (or --max-width).",
            image_w, image_h
        ));
    }

    // Clip coordinates are relative to the document origin. For full-page
    // captures the region starts at the document origin (scroll is irrelevant
    // since the whole content is captured). For viewport captures with
    // downscaling, the layout viewport's scroll offsets (pageX/pageY) must be
    // used so the visible region — not the document's top-left — is framed.
    if full_page || clip_scale < 1.0 {
        params["clip"] = capture_clip(scroll_x, scroll_y, src_w, src_h, zoom, clip_scale);
    }

    let result = client
        .send_to_target(session_id, "Page.captureScreenshot", params)
        .await
        .context("Failed to capture screenshot via CDP")?;

    let data_b64 = result["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No screenshot data in response"))?;

    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64)?;

    let result = match output {
        Some(path) => {
            tokio::fs::write(&path, &bytes)
                .await
                .with_context(|| format!("Failed to write screenshot to {}", path))?;
            CommandResult::output(format!(
                "Screenshot saved to {path} ({} bytes)",
                bytes.len()
            ))
        }
        None => CommandResult::output(data_b64.to_string()),
    };
    Ok(CommandResult { notes, ..result })
}

/// The window's `devicePixelRatio`: the browser zoom times the device scale
/// factor, an emulated `--device-scale-factor` included (the layout viewports
/// below leave that out). Read in an isolated world, since the page can
/// replace `window.devicePixelRatio` in its own, as some fingerprinting
/// defences do, and a smaller value there would let a capture past
/// --max-width through. Chrome reuses a world of the same name, so repeated
/// screenshots don't add any. `None` where the page can't run JavaScript.
async fn page_device_pixel_ratio(client: &mut CdpClient, session_id: &str) -> Result<Option<f64>> {
    let ratio = async {
        let frame_id = super::input::main_frame_id(client, session_id).await?;
        let world = client
            .send_to_target(
                session_id,
                "Page.createIsolatedWorld",
                json!({"frameId": frame_id, "worldName": "chrome-devtools-cli"}),
            )
            .await?;
        let result = client
            .send_to_target(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": "devicePixelRatio",
                    "contextId": world["executionContextId"],
                    "returnByValue": true,
                }),
            )
            .await?;
        Ok::<Option<f64>, anyhow::Error>(result["result"]["value"].as_f64().filter(|r| *r > 0.0))
    }
    .await;
    dpr_probe_result(ratio)
}

/// A dialog or a failed page/connection from the DPR probe is the screenshot's
/// failure too: the capture would otherwise retry the same page with a fresh
/// deadline, doubling the wait before it fails (and keeping the daemon busy).
/// Other errors only mean the ratio is unavailable, so the metrics-based
/// fallback can supply it.
fn dpr_probe_result(ratio: Result<Option<f64>>) -> Result<Option<f64>> {
    match ratio {
        Err(e) if e.downcast_ref::<DialogOpen>().is_some() || is_connection_or_page_failure(&e) => {
            Err(e)
        }
        Err(_) => Ok(None),
        Ok(ratio) => Ok(ratio),
    }
}

/// DIPs per CSS pixel, which is the browser zoom (`cssVisualViewport.zoom`;
/// pinch zoom is its `scale`). 1 when missing.
fn browser_zoom(metrics: &serde_json::Value) -> f64 {
    metrics["cssVisualViewport"]["zoom"]
        .as_f64()
        .filter(|z| z.is_finite() && *z > 0.0)
        .unwrap_or(1.0)
}

/// `Page.captureScreenshot`'s clip for a region in CSS pixels. Chrome reads
/// the rectangle in DIPs, which are CSS pixels times the browser zoom, and
/// renders it at `scale` times the device scale factor. Verified on Chrome
/// 158: at 200% zoom, a rectangle in CSS pixels covered only the top-left
/// quarter of the viewport, and a full-page one only half of the page.
fn capture_clip(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    zoom: f64,
    scale: f64,
) -> serde_json::Value {
    json!({
        "x": x * zoom, "y": y * zoom,
        "width": width * zoom, "height": height * zoom,
        "scale": scale,
    })
}

/// Fallback for pages without JavaScript: `layoutViewport` is in device pixels
/// and `cssLayoutViewport` in CSS pixels, so their ratio is the browser zoom
/// times the screen's scale factor. It misses an emulated
/// `--device-scale-factor`, which neither includes. 1 when either is missing.
fn device_pixel_ratio(metrics: &serde_json::Value) -> f64 {
    let device = metrics["layoutViewport"]["clientWidth"].as_f64();
    let css = metrics["cssLayoutViewport"]["clientWidth"].as_f64();
    match (device, css) {
        (Some(d), Some(c)) if d > 0.0 && c > 0.0 => d / c,
        _ => 1.0,
    }
}

/// Compute the downscale factor for a screenshot clip.
///
/// Returns the smaller of the width and height scale ratios, clamped to <= 1.0
/// (never upscales). A `None` dimension, or a non-positive max/src value, yields
/// 1.0 for that axis (no scaling). Returns 1.0 when neither dimension is set.
fn clip_scale_factor(
    src_w: f64,
    src_h: f64,
    max_width: Option<f64>,
    max_height: Option<f64>,
) -> f64 {
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
    use super::dpr_probe_result;
    use super::{browser_zoom, capture_clip, clip_scale_factor, device_pixel_ratio};
    use crate::cdp::DialogOpen;

    /// Page.getLayoutMetrics at 200% browser zoom on a 1x screen (Chrome
    /// 158), scrolled to y=500: the clip must be in DIPs to frame the viewport.
    #[test]
    fn the_clip_is_in_dips_at_any_browser_zoom() {
        let metrics = serde_json::json!({
            "layoutViewport": {"pageX": 0, "pageY": 1000, "clientWidth": 756, "clientHeight": 469},
            "cssLayoutViewport": {"pageX": 0, "pageY": 500, "clientWidth": 378, "clientHeight": 234},
            "cssVisualViewport": {"clientWidth": 378, "clientHeight": 234.5, "scale": 1, "zoom": 2},
        });
        let zoom = browser_zoom(&metrics);
        assert_eq!(zoom, 2.0);
        assert_eq!(
            capture_clip(0.0, 500.0, 378.0, 234.0, zoom, 0.5),
            serde_json::json!({"x": 0.0, "y": 1000.0, "width": 756.0, "height": 468.0, "scale": 0.5})
        );
        assert_eq!(browser_zoom(&serde_json::json!({})), 1.0);
        // The fallback ratio has the zoom in it, as devicePixelRatio does.
        assert_eq!(device_pixel_ratio(&metrics), 2.0);
    }

    #[test]
    fn device_pixel_ratio_comes_from_the_two_viewports() {
        let hidpi = serde_json::json!({
            "layoutViewport": {"clientWidth": 1600},
            "cssLayoutViewport": {"clientWidth": 800},
        });
        assert_eq!(device_pixel_ratio(&hidpi), 2.0);
        assert_eq!(device_pixel_ratio(&serde_json::json!({})), 1.0);
        // --max-width 400 on an 800 CSS px viewport at 2x: the image is 1600
        // device px wide, so the clip must scale by 400/1600, not 400/800.
        assert_eq!(
            clip_scale_factor(800.0 * 2.0, 600.0 * 2.0, Some(400.0), None),
            0.25
        );
    }

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
        assert_eq!(
            clip_scale_factor(1920.0, 1080.0, Some(-100.0), Some(-50.0)),
            1.0
        );
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
        assert_eq!(
            clip_scale_factor(2000.0, 1000.0, Some(1000.0), Some(250.0)),
            0.25
        );
    }

    /// A dialog that the probe could not settle must reach the caller: the
    /// capture call would start a new deadline and could hide the failure.
    #[test]
    fn the_dpr_probe_keeps_a_dialog_failure() {
        let dialog = anyhow::Error::new(DialogOpen {
            dialog_type: "alert".into(),
            message: "hi".into(),
        });
        assert!(dpr_probe_result(Err(dialog))
            .unwrap_err()
            .downcast_ref::<DialogOpen>()
            .is_some());
        assert_eq!(
            dpr_probe_result(Err(anyhow::anyhow!("no JS"))).unwrap(),
            None
        );
        assert_eq!(dpr_probe_result(Ok(Some(2.0))).unwrap(), Some(2.0));
    }

    /// A stalled page or a lost connection must stop the command here: retrying
    /// `Page.captureScreenshot` on the same page only doubles the wait.
    #[test]
    fn the_dpr_probe_keeps_a_stall_or_lost_connection() {
        for error in [
            anyhow::Error::new(crate::cdp::PageUnresponsive("stalled".into())),
            anyhow::Error::new(crate::cdp::ConnectionLost("closed".into())),
        ] {
            let failed = dpr_probe_result(Err(error)).unwrap_err();
            assert!(
                crate::cdp::is_connection_or_page_failure(&failed),
                "{failed}"
            );
        }
    }

    #[test]
    fn never_upscales_when_max_exceeds_source() {
        // src 800x600, max 1600x1200 → both ratios > 1.0, clamped to 1.0
        assert_eq!(
            clip_scale_factor(800.0, 600.0, Some(1600.0), Some(1200.0)),
            1.0
        );
    }
}

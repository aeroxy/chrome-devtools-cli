use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Unified emulation command — get or set viewport and geolocation overrides.
///
/// With no flags, returns all active overrides.
/// Page-based: overrides persist until cleared or page is closed.
pub async fn emulate(
    client: &mut CdpClient,
    session_id: &str,
    as_json: bool,
    viewport: Option<&str>,
    geolocation: Option<&str>,
    accuracy: Option<f64>,
    clear_viewport: bool,
    clear_geolocation: bool,
    clear_all: bool,
) -> Result<CommandResult> {
    // No flags → show current state
    if viewport.is_none()
        && geolocation.is_none()
        && !clear_viewport
        && !clear_geolocation
        && !clear_all
    {
        return get_all(client, session_id, as_json).await;
    }

    if clear_all {
        clear_viewport_override(client, session_id).await?;
        clear_geolocation_override(client, session_id).await?;
        return Ok(CommandResult::output("All emulation overrides cleared".to_string()));
    }

    if clear_viewport {
        clear_viewport_override(client, session_id).await?;
    }
    if clear_geolocation {
        clear_geolocation_override(client, session_id).await?;
    }

    if let Some(viewport_str) = viewport {
        let (w, h) = parse_viewport(viewport_str)?;
        client
            .send_to_target(
                session_id,
                "Emulation.setDeviceMetricsOverride",
                json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false}),
            )
            .await?;
    }

    if let Some(geo_str) = geolocation {
        let (lat, lon) = parse_geolocation(geo_str)?;
        let acc = accuracy.unwrap_or(100.0);
        if !(-90.0..=90.0).contains(&lat) {
            bail!("latitude must be between -90 and 90");
        }
        if !(-180.0..=180.0).contains(&lon) {
            bail!("longitude must be between -180 and 180");
        }
        if !acc.is_finite() || acc < 0.0 {
            bail!("accuracy must be a non-negative finite number");
        }
        client
            .send_to_target(
                session_id,
                "Emulation.setGeolocationOverride",
                json!({"latitude": lat, "longitude": lon, "accuracy": acc}),
            )
            .await?;
    }

    Ok(CommandResult::output("Emulation overrides applied".to_string()))
}

/// Parse "WxH" viewport string.
fn parse_viewport(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        bail!("--viewport must be WxH format (e.g. 1280x720)");
    }
    let w: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid viewport width: '{}'", parts[0]))?;
    let h: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid viewport height: '{}'", parts[1]))?;
    Ok((w, h))
}

/// Parse "lat,lon" geolocation string.
fn parse_geolocation(s: &str) -> Result<(f64, f64)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 2 {
        bail!("--geolocation must be lat,lon format (e.g. 37.7749,-122.4194)");
    }
    let lat: f64 = parts[0]
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid latitude: '{}'", parts[0]))?;
    let lon: f64 = parts[1]
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid longitude: '{}'", parts[1]))?;
    Ok((lat, lon))
}

async fn clear_viewport_override(client: &mut CdpClient, session_id: &str) -> Result<()> {
    client
        .send_to_target(session_id, "Emulation.clearDeviceMetricsOverride", json!({}))
        .await?;
    Ok(())
}

async fn clear_geolocation_override(client: &mut CdpClient, session_id: &str) -> Result<()> {
    client
        .send_to_target(session_id, "Emulation.clearGeolocationOverride", json!({}))
        .await?;
    Ok(())
}

/// Get all active emulation overrides.
async fn get_all(
    client: &mut CdpClient,
    session_id: &str,
    as_json: bool,
) -> Result<CommandResult> {
    let mut out = Vec::new();

    // Viewport
    let vp = client
        .send_to_target(session_id, "Emulation.getDeviceMetricsOverride", json!({}))
        .await?;
    let vp_override = vp["width"].as_u64().and_then(|w| {
        vp["height"].as_u64().map(|h| {
            (
                w,
                h,
                vp["deviceScaleFactor"].as_f64().unwrap_or(1.0),
                vp["mobile"].as_bool().unwrap_or(false),
            )
        })
    });

    // Geolocation (try to read via JS)
    let geo_expr = r#"
        new Promise((resolve, reject) => {
            navigator.geolocation.getCurrentPosition(
                pos => resolve({latitude: pos.coords.latitude, longitude: pos.coords.longitude, accuracy: pos.coords.accuracy}),
                err => reject(err.message),
                {maximumAge: 0, timeout: 3000}
            );
        })
    "#;
    let geo = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": geo_expr,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await;
    let geo_override = geo.ok().and_then(|r| {
        if r.get("exceptionDetails").is_some() {
            None
        } else {
            let v = &r["result"]["value"];
            Some((
                v["latitude"].as_f64().unwrap_or(0.0),
                v["longitude"].as_f64().unwrap_or(0.0),
                v["accuracy"].as_f64().unwrap_or(0.0),
            ))
        }
    });

    if as_json {
        let obj = json!({
            "viewport": vp_override.map(|(w, h, s, m)| json!({"width": w, "height": h, "scale": s, "mobile": m})),
            "geolocation": geo_override.map(|(lat, lon, acc)| json!({"latitude": lat, "longitude": lon, "accuracy": acc})),
        });
        return Ok(CommandResult::output(serde_json::to_string_pretty(&obj)?));
    }

    match vp_override {
        Some((w, h, s, m)) => out.push(format!("Viewport: {w}x{h} (scale: {s}, mobile: {m})")),
        None => out.push("Viewport: none".to_string()),
    }
    match geo_override {
        Some((lat, lon, acc)) => {
            out.push(format!("Geolocation: {lat}, {lon} (accuracy: {acc}m)"))
        }
        None => out.push("Geolocation: none".to_string()),
    }

    Ok(CommandResult::output(out.join("\n")))
}

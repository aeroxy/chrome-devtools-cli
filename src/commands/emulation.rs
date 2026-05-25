use anyhow::{anyhow, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Combined emulation parameters for the `emulate` command.
pub struct EmulateParams {
    pub viewport: Option<String>,
    pub geolocation: Option<String>,
    pub accuracy: Option<f64>,
    pub clear_viewport: bool,
    pub clear_geolocation: bool,
    pub clear_all: bool,
}

/// Execute the unified emulation command.
pub async fn emulate(
    client: &mut CdpClient,
    session_id: &str,
    params: EmulateParams,
    as_json: bool,
) -> Result<CommandResult> {
    let mut actions = Vec::new();

    // 1. Handle clearing
    if params.clear_all || params.clear_viewport {
        client
            .send_to_target(session_id, "Emulation.clearDeviceMetricsOverride", json!({}))
            .await?;
        actions.push("Viewport cleared".to_string());
    }

    if params.clear_all || params.clear_geolocation {
        client
            .send_to_target(session_id, "Emulation.clearGeolocationOverride", json!({}))
            .await?;
        actions.push("Geolocation cleared".to_string());
    }

    // 2. Handle setting viewport
    if let Some(viewport_str) = params.viewport {
        let parts: Vec<&str> = viewport_str.split('x').collect();
        if parts.len() != 2 {
            anyhow::bail!("Viewport must be in WxH format (e.g. 1280x720)");
        }
        let w: u32 = parts[0].parse().map_err(|_| anyhow!("Invalid width: {}", parts[0]))?;
        let h: u32 = parts[1].parse().map_err(|_| anyhow!("Invalid height: {}", parts[1]))?;

        client
            .send_to_target(
                session_id,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": w,
                    "height": h,
                    "deviceScaleFactor": 1,
                    "mobile": false,
                }),
            )
            .await?;
        actions.push(format!("Viewport set to {}x{}", w, h));
    }

    // 3. Handle setting geolocation
    if let Some(geo_str) = params.geolocation {
        let parts: Vec<&str> = geo_str.split(',').collect();
        if parts.len() != 2 {
            anyhow::bail!("Geolocation must be in lat,lon format (e.g. 37.77,-122.41)");
        }
        let lat: f64 = parts[0].parse().map_err(|_| anyhow!("Invalid latitude: {}", parts[0]))?;
        let lon: f64 = parts[1].parse().map_err(|_| anyhow!("Invalid longitude: {}", parts[1]))?;
        let acc = params.accuracy.unwrap_or(100.0);

        if !(-90.0..=90.0).contains(&lat) {
            anyhow::bail!("latitude must be between -90 and 90");
        }
        if !(-180.0..=180.0).contains(&lon) {
            anyhow::bail!("longitude must be between -180 and 180");
        }

        client
            .send_to_target(
                session_id,
                "Emulation.setGeolocationOverride",
                json!({ "latitude": lat, "longitude": lon, "accuracy": acc }),
            )
            .await?;
        actions.push(format!("Geolocation set to {}, {} (acc: {}m)", lat, lon, acc));
    }

    // 4. If no specific action taken and not clearing, show current state
    if actions.is_empty() && !params.clear_all && !params.clear_viewport && !params.clear_geolocation {
        return get_emulation_state(client, session_id, as_json).await;
    }

    Ok(CommandResult::output(actions.join(", ")))
}

/// Retrieve all active emulation overrides.
async fn get_emulation_state(
    client: &mut CdpClient,
    session_id: &str,
    as_json: bool,
) -> Result<CommandResult> {
    // Get viewport override
    let viewport_resp = client
        .send_to_target(session_id, "Emulation.getDeviceMetricsOverride", json!({}))
        .await?;
    
    let vw = viewport_resp["width"].as_u64();
    let vh = viewport_resp["height"].as_u64();
    let viewport = if let (Some(w), Some(h)) = (vw, vh) {
        if w > 0 && h > 0 {
            Some(json!({ "width": w, "height": h }))
        } else {
            None
        }
    } else {
        None
    };

    // Get geolocation override via JS (since there is no CDP getter)
    let geo_expr = r#"
        new Promise((resolve) => {
            navigator.geolocation.getCurrentPosition(
                pos => resolve({latitude: pos.coords.latitude, longitude: pos.coords.longitude, accuracy: pos.coords.accuracy}),
                err => resolve({error: err.message}),
                {maximumAge: 0, timeout: 2000}
            );
        })
    "#;

    let geo_result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": geo_expr,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;

    let geo_val = &geo_result["result"]["value"];
    let geolocation = if geo_val["error"].is_null() && !geo_val["latitude"].is_null() {
        Some(geo_val.clone())
    } else {
        None
    };

    if as_json {
        Ok(CommandResult::output(serde_json::to_string_pretty(&json!({
            "viewport": viewport,
            "geolocation": geolocation,
        }))?))
    } else {
        let mut out = Vec::new();
        match viewport {
            Some(v) => out.push(format!("Viewport: {}x{}", v["width"], v["height"])),
            None => out.push("Viewport: (default)".to_string()),
        }
        match geolocation {
            Some(g) => out.push(format!("Geolocation: {}, {} (acc: {}m)", g["latitude"], g["longitude"], g["accuracy"])),
            None => out.push("Geolocation: (default)".to_string()),
        }
        Ok(CommandResult::output(out.join("\n")))
    }
}

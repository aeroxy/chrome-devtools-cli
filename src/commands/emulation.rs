use anyhow::{anyhow, Result};
use serde_json::json;

use crate::cdp::{CdpClient, GeolocationOverride, ViewportOverride};
use crate::result::CommandResult;

/// Combined emulation parameters for the `emulate` command.
pub struct EmulateParams {
    pub viewport: Option<String>,
    pub device_scale_factor: Option<f64>,
    pub mobile: bool,
    pub geolocation: Option<String>,
    pub accuracy: Option<f64>,
    pub clear_viewport: bool,
    pub clear_geolocation: bool,
    pub clear_all: bool,
    /// Patterns to add to the daemon's network blocklist (`*.png`, `*.gif`, etc.).
    pub block_url: Vec<String>,
    /// Patterns to remove from the daemon's network blocklist (un-block them).
    pub unblock_url: Vec<String>,
    /// Clear the entire network blocklist.
    pub clear_blocks: bool,
}

impl EmulateParams {
    /// Validate cross-field constraints before applying emulation.
    pub fn validate(&self) -> Result<()> {
        if let Some(dsf) = self.device_scale_factor {
            if !dsf.is_finite() || dsf <= 0.0 {
                anyhow::bail!("device-scale-factor must be a positive finite value");
            }
        }
        if let Some(acc) = self.accuracy {
            if acc.is_sign_negative() || !acc.is_finite() {
                anyhow::bail!("accuracy must be a non-negative finite value");
            }
        }
        if self.accuracy.is_some() && self.geolocation.is_none() {
            anyhow::bail!("--accuracy requires --geolocation");
        }
        if self.mobile && self.viewport.is_none() {
            anyhow::bail!("--mobile requires --viewport");
        }
        if self.device_scale_factor.is_some() && self.viewport.is_none() {
            anyhow::bail!("--device-scale-factor requires --viewport");
        }
        Ok(())
    }

    /// Returns true if any flag that triggers emulation is set.
    pub fn has_emulation(&self) -> bool {
        self.viewport.is_some()
            || self.geolocation.is_some()
            || self.device_scale_factor.is_some()
            || self.mobile
            || self.clear_all
            || self.clear_viewport
            || self.clear_geolocation
            || !self.block_url.is_empty()
            || !self.unblock_url.is_empty()
            || self.clear_blocks
    }
}

/// Execute the unified emulation command.
pub async fn emulate(
    client: &mut CdpClient,
    session_id: &str,
    params: EmulateParams,
) -> Result<CommandResult> {
    let mut actions = Vec::new();

    // 0. Handle network blocklist (applied to the persistent session, not the
    //    per-command session — so the blocklist survives across commands/targets).
    let network_changed = !params.block_url.is_empty()
        || !params.unblock_url.is_empty()
        || params.clear_blocks
        || params.clear_all;

    if params.clear_all || params.clear_blocks {
        client.blocklist.clear();
        actions.push("Network blocks cleared".to_string());
    }

    for pattern in &params.block_url {
        if !client.blocklist.contains(pattern) {
            client.blocklist.push(pattern.clone());
        }
    }
    if !params.block_url.is_empty() {
        actions.push(format!("Blocked URLs: {}", params.block_url.join(", ")));
    }

    for pattern in &params.unblock_url {
        client.blocklist.retain(|p| p != pattern);
    }
    if !params.unblock_url.is_empty() {
        actions.push(format!(
            "Un-blocked URLs: {}",
            params.unblock_url.join(", ")
        ));
    }

    if network_changed {
        // Apply to persistent session if available; otherwise fall back to the
        // command's session (e.g., direct mode or degraded daemon path).
        if client.persistent_session.is_some() {
            client.apply_network_rules().await?;
        } else {
            client.apply_network_rules_internal(session_id).await?;
        }
    }

    // 1. Handle clearing
    if params.clear_all || params.clear_viewport {
        client
            .send_to_target(
                session_id,
                "Emulation.clearDeviceMetricsOverride",
                json!({}),
            )
            .await?;
        client.viewport = None;
        actions.push("Viewport cleared".to_string());
    }

    if params.clear_all || params.clear_geolocation {
        client
            .send_to_target(session_id, "Emulation.clearGeolocationOverride", json!({}))
            .await?;
        client.geolocation = None;
        actions.push("Geolocation cleared".to_string());
    }

    // 2. Handle setting viewport
    if let Some(viewport_str) = params.viewport {
        let viewport_lower = viewport_str.to_lowercase();
        let parts: Vec<&str> = viewport_lower.split('x').collect();
        if parts.len() != 2 {
            anyhow::bail!("Viewport must be in WxH format (e.g. 1280x720)");
        }
        let w: u32 = parts[0]
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid width: {}", parts[0]))?;
        let h: u32 = parts[1]
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid height: {}", parts[1]))?;
        if w == 0 {
            anyhow::bail!("Invalid width: {} (must be > 0)", w);
        }
        if h == 0 {
            anyhow::bail!("Invalid height: {} (must be > 0)", h);
        }
        let dsf = params.device_scale_factor.unwrap_or(1.0);

        client
            .send_to_target(
                session_id,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": w,
                    "height": h,
                    "deviceScaleFactor": dsf,
                    "mobile": params.mobile,
                }),
            )
            .await?;
        client.viewport = Some(ViewportOverride {
            width: w,
            height: h,
            device_scale_factor: dsf,
            mobile: params.mobile,
        });
        actions.push(format!(
            "Viewport set to {}x{} (scale: {}, mobile: {})",
            w, h, dsf, params.mobile
        ));
    }

    // 3. Handle setting geolocation
    if let Some(geo_str) = params.geolocation {
        let parts: Vec<&str> = geo_str.split(',').collect();
        if parts.len() != 2 {
            anyhow::bail!("Geolocation must be in lat,lon format (e.g. 37.77,-122.41)");
        }
        let lat: f64 = parts[0]
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid latitude: {}", parts[0]))?;
        let lon: f64 = parts[1]
            .trim()
            .parse()
            .map_err(|_| anyhow!("Invalid longitude: {}", parts[1]))?;
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
        client.geolocation = Some(GeolocationOverride {
            latitude: lat,
            longitude: lon,
            accuracy: acc,
        });
        actions.push(format!(
            "Geolocation set to {}, {} (acc: {}m)",
            lat, lon, acc
        ));
    }

    // 4. If no specific action taken, show current overrides
    if actions.is_empty() {
        let mut out = String::new();
        if let Some(vp) = &client.viewport {
            out.push_str(&format!(
                "Viewport: {}x{} (scale: {}, mobile: {})\n",
                vp.width, vp.height, vp.device_scale_factor, vp.mobile
            ));
        }
        if let Some(geo) = &client.geolocation {
            out.push_str(&format!(
                "Geolocation: {}, {} (acc: {}m)\n",
                geo.latitude, geo.longitude, geo.accuracy
            ));
        }
        if !client.blocklist.is_empty() {
            out.push_str("Blocked URLs:\n");
            for p in &client.blocklist {
                out.push_str(&format!("  {p}\n"));
            }
        }
        if out.is_empty() {
            out.push_str("No emulation overrides active.");
        }
        return Ok(CommandResult::output(out.trim_end().to_string()));
    }

    Ok(CommandResult::output(actions.join(", ")))
}

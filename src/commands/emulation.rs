use anyhow::{anyhow, Result};
use serde_json::json;

use crate::cdp::CdpClient;
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
}

impl EmulateParams {
    /// Validate cross-field constraints before applying emulation.
    pub fn validate(&self) -> Result<()> {
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
    }
}

/// Execute the unified emulation command.
pub async fn emulate(
    client: &mut CdpClient,
    session_id: &str,
    params: EmulateParams,
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
        if w == 0 {
            anyhow::bail!("Invalid width: {} (must be > 0)", w);
        }
        if h == 0 {
            anyhow::bail!("Invalid height: {} (must be > 0)", h);
        }
        let dsf = params.device_scale_factor.unwrap_or(1.0);
        if !dsf.is_finite() || dsf <= 0.0 {
            anyhow::bail!("device-scale-factor must be a positive finite value");
        }

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
        actions.push(format!("Viewport set to {}x{} (scale: {}, mobile: {})", w, h, dsf, params.mobile));
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

        if acc.is_sign_negative() || !acc.is_finite() {
            anyhow::bail!("accuracy must be a non-negative finite value");
        }

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

    // 4. If no specific action taken, return error (getters removed due to CDP limitations)
    if actions.is_empty() {
        anyhow::bail!("No emulation action specified (use --viewport, --geolocation, or --clear flags)");
    }

    Ok(CommandResult::output(actions.join(", ")))
}

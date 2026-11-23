use anyhow::{anyhow, Result};
use serde_json::json;

use crate::cdp::{CdpClient, GeolocationOverride, MediaOverride, ViewportOverride};
use crate::result::CommandResult;

/// Media features Chrome emulates, with the values each takes. Chrome accepts
/// any name and value without error and ignores what it doesn't know, so a typo
/// would look applied while changing nothing.
const MEDIA_FEATURES: &[(&str, &[&str])] = &[
    ("prefers-color-scheme", &["light", "dark"]),
    ("prefers-reduced-motion", &["reduce", "no-preference"]),
    ("prefers-reduced-transparency", &["reduce", "no-preference"]),
    (
        "prefers-contrast",
        &["more", "less", "custom", "no-preference"],
    ),
    ("forced-colors", &["active", "none"]),
    ("color-gamut", &["srgb", "p3", "rec2020"]),
];

/// Parse `--media`: the CSS media type to emulate.
pub fn parse_media_type(raw: &str) -> Result<String> {
    let media = raw.trim().to_ascii_lowercase();
    match media.as_str() {
        "print" | "screen" => Ok(media),
        _ => anyhow::bail!("Invalid --media '{raw}': expected print or screen"),
    }
}

/// Parse one `--media-feature name=value`, checked against [`MEDIA_FEATURES`].
pub fn parse_media_feature(raw: &str) -> Result<(String, String)> {
    let (name, value) = raw.split_once('=').ok_or_else(|| {
        anyhow!(
            "Invalid --media-feature '{raw}': expected name=value (e.g. prefers-color-scheme=dark)"
        )
    })?;
    let name = name.trim().to_ascii_lowercase();
    let value = value.trim().to_ascii_lowercase();
    let Some((_, allowed)) = MEDIA_FEATURES.iter().find(|(n, _)| *n == name) else {
        let names: Vec<&str> = MEDIA_FEATURES.iter().map(|(n, _)| *n).collect();
        anyhow::bail!(
            "Unsupported media feature '{name}'. Supported: {}",
            names.join(", ")
        );
    };
    if !allowed.contains(&value.as_str()) {
        anyhow::bail!(
            "Invalid value '{value}' for {name}: expected {}",
            allowed.join(", ")
        );
    }
    Ok((name, value))
}

/// Fold newly requested media settings into the current ones. Settings not
/// named keep their value, as geolocation is kept when only the viewport is
/// set; a feature set again takes its new value.
fn merge_media(
    mut current: MediaOverride,
    media: Option<String>,
    features: Vec<(String, String)>,
) -> MediaOverride {
    if media.is_some() {
        current.media = media;
    }
    for (name, value) in features {
        match current.features.iter_mut().find(|(n, _)| *n == name) {
            Some(existing) => existing.1 = value,
            None => current.features.push((name, value)),
        }
    }
    current
}

/// One-line description of a media override, in the flags' own spelling.
fn describe_media(media: &MediaOverride) -> String {
    let mut parts: Vec<String> = media.media.iter().cloned().collect();
    parts.extend(media.features.iter().map(|(n, v)| format!("{n}={v}")));
    parts.join("; ")
}

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
    /// CSS media type to emulate (`print` or `screen`).
    pub media: Option<String>,
    /// CSS media features to emulate, each as `name=value`.
    pub media_features: Vec<String>,
    /// Clear the CSS media emulation.
    pub clear_media: bool,
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
        if let Some(media) = &self.media {
            parse_media_type(media)?;
        }
        for feature in &self.media_features {
            parse_media_feature(feature)?;
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
            || self.media.is_some()
            || !self.media_features.is_empty()
            || self.clear_media
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

    if params.clear_all || params.clear_media {
        client
            .send_to_target(
                session_id,
                "Emulation.setEmulatedMedia",
                MediaOverride::default().cdp_params(),
            )
            .await?;
        client.media = None;
        actions.push("Media emulation cleared".to_string());
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

    // 4. Handle CSS media emulation. Chrome replaces every feature on each
    //    call, so the merged set is sent whole.
    if params.media.is_some() || !params.media_features.is_empty() {
        let media = params.media.as_deref().map(parse_media_type).transpose()?;
        let features = params
            .media_features
            .iter()
            .map(|f| parse_media_feature(f))
            .collect::<Result<Vec<_>>>()?;
        let next = merge_media(client.media.clone().unwrap_or_default(), media, features);
        client
            .send_to_target(session_id, "Emulation.setEmulatedMedia", next.cdp_params())
            .await?;
        actions.push(format!("Media emulation set ({})", describe_media(&next)));
        client.media = Some(next);
    }

    // 5. If no specific action taken, show current overrides
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
        if let Some(media) = &client.media {
            out.push_str(&format!("Media: {}\n", describe_media(media)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_accepts_print_and_screen_only() {
        assert_eq!(parse_media_type("print").unwrap(), "print");
        assert_eq!(parse_media_type(" Screen ").unwrap(), "screen");
        let err = parse_media_type("tv").unwrap_err().to_string();
        assert!(err.contains("expected print or screen"), "{err}");
    }

    #[test]
    fn media_feature_parses_and_normalises() {
        assert_eq!(
            parse_media_feature("prefers-color-scheme=dark").unwrap(),
            ("prefers-color-scheme".to_string(), "dark".to_string())
        );
        assert_eq!(
            parse_media_feature(" Forced-Colors = ACTIVE ").unwrap(),
            ("forced-colors".to_string(), "active".to_string())
        );
    }

    /// Chrome takes unknown names and values silently, so these must fail here.
    #[test]
    fn media_feature_rejects_what_chrome_would_ignore() {
        let missing_eq = parse_media_feature("prefers-color-scheme").unwrap_err();
        assert!(missing_eq.to_string().contains("expected name=value"));
        let unknown = parse_media_feature("prefers-colour-scheme=dark").unwrap_err();
        assert!(unknown
            .to_string()
            .contains("Supported: prefers-color-scheme"));
        let bad_value = parse_media_feature("prefers-reduced-motion=yes").unwrap_err();
        assert!(bad_value
            .to_string()
            .contains("expected reduce, no-preference"));
    }

    fn params() -> EmulateParams {
        EmulateParams {
            viewport: None,
            device_scale_factor: None,
            mobile: false,
            geolocation: None,
            accuracy: None,
            clear_viewport: false,
            clear_geolocation: false,
            clear_all: false,
            block_url: vec![],
            unblock_url: vec![],
            clear_blocks: false,
            media: None,
            media_features: vec![],
            clear_media: false,
        }
    }

    /// One bad feature must stop the whole command before any override is sent.
    #[test]
    fn validate_checks_media_before_anything_is_applied() {
        let mut p = params();
        p.viewport = Some("800x600".to_string());
        p.media_features = vec![
            "prefers-color-scheme=dark".into(),
            "forced-colors=on".into(),
        ];
        assert!(p.validate().is_err());
        p.media_features.pop();
        p.media = Some("print".into());
        assert!(p.validate().is_ok());
        p.media = Some("braille".into());
        assert!(p.validate().is_err());
    }

    #[test]
    fn media_flags_count_as_emulation() {
        let mut p = params();
        assert!(!p.has_emulation());
        p.clear_media = true;
        assert!(p.has_emulation());
        let mut p = params();
        p.media_features = vec!["forced-colors=active".into()];
        assert!(p.has_emulation());
    }

    #[test]
    fn merge_keeps_unnamed_settings_and_replaces_named_ones() {
        let current = MediaOverride {
            media: Some("print".into()),
            features: vec![
                ("prefers-color-scheme".into(), "dark".into()),
                ("forced-colors".into(), "active".into()),
            ],
        };
        let merged = merge_media(
            current,
            None,
            vec![
                ("prefers-color-scheme".into(), "light".into()),
                ("prefers-reduced-motion".into(), "reduce".into()),
            ],
        );
        assert_eq!(merged.media.as_deref(), Some("print"));
        assert_eq!(
            merged.features,
            vec![
                ("prefers-color-scheme".to_string(), "light".to_string()),
                ("forced-colors".to_string(), "active".to_string()),
                ("prefers-reduced-motion".to_string(), "reduce".to_string()),
            ]
        );
        assert_eq!(
            describe_media(&merged),
            "print; prefers-color-scheme=light; forced-colors=active; prefers-reduced-motion=reduce"
        );
    }

    /// An empty media string and an empty list are what turn Chrome's override off.
    #[test]
    fn media_override_cdp_params() {
        assert_eq!(
            MediaOverride::default().cdp_params(),
            json!({"media": "", "features": []})
        );
        let set = MediaOverride {
            media: Some("print".into()),
            features: vec![("prefers-color-scheme".into(), "dark".into())],
        };
        assert_eq!(
            set.cdp_params(),
            json!({"media": "print", "features": [{"name": "prefers-color-scheme", "value": "dark"}]})
        );
    }
}

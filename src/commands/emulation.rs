use anyhow::{anyhow, Result};
use serde_json::json;

use crate::cdp::{CdpClient, GeolocationOverride, MediaOverride, ViewportOverride};
use crate::format::{format_structured, OutputFormat};
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

/// Parse `--geolocation lat,lon`. The option takes values that start with `-`
/// for southern and western coordinates, so clap hands it the next flag when
/// its value is missing (`--geolocation --clear-all`). `validate` runs this
/// so such a value fails before any other override is applied.
pub fn parse_geolocation(raw: &str) -> Result<(f64, f64)> {
    let invalid =
        || anyhow!("Invalid --geolocation '{raw}': expected lat,lon (e.g. 37.77,-122.41)");
    let (lat, lon) = raw.split_once(',').ok_or_else(invalid)?;
    let lat: f64 = lat.trim().parse().map_err(|_| invalid())?;
    let lon: f64 = lon.trim().parse().map_err(|_| invalid())?;
    // The ranges also reject NaN and infinity, which parse as f64.
    if !(-90.0..=90.0).contains(&lat) {
        anyhow::bail!("Invalid --geolocation '{raw}': latitude must be between -90 and 90");
    }
    if !(-180.0..=180.0).contains(&lon) {
        anyhow::bail!("Invalid --geolocation '{raw}': longitude must be between -180 and 180");
    }
    Ok((lat, lon))
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

/// A clear is only known to have happened once Chrome acknowledges it. Until
/// then leave an empty override as a pending-reset marker: status must not
/// claim media is cleared and `--clear-all` must retry, while a later media
/// update must not resend the old value unless the user asks for it. An absent
/// override stays absent (the tab whose saved override is being cleared may be
/// a different one).
fn media_after_clear(cache: &mut Option<MediaOverride>, acknowledged: bool) {
    if acknowledged {
        *cache = None;
    } else if cache.is_some() {
        *cache = Some(MediaOverride::default());
    }
}

/// Status text for a media override, including the pending-reset marker left
/// by a clear Chrome has not acknowledged.
fn describe_media_status(media: &MediaOverride) -> String {
    if media == &MediaOverride::default() {
        "a clear was requested but not confirmed (it may still be active); rerun with \
         --clear-media"
            .to_string()
    } else {
        describe_media(media)
    }
}

/// The read-only emulation status as one result. Text keeps the prose status,
/// while `--json`/`--toon` get structured data. Active and saved media use the
/// same shape, with a pending-reset marker for a clear Chrome has not confirmed.
fn current_overrides(
    viewport: Option<&ViewportOverride>,
    geolocation: Option<&GeolocationOverride>,
    media: Option<&MediaOverride>,
    saved: Option<&MediaOverride>,
    blocklist: &[String],
    format: OutputFormat,
) -> Result<CommandResult> {
    if !format.is_text() {
        let media_status = |m: &MediaOverride| {
            if m == &MediaOverride::default() {
                json!({"pendingReset": true})
            } else {
                json!({"media": m.media, "features": m.features})
            }
        };
        return Ok(CommandResult::output(format_structured(
            &json!({
                "viewport": viewport.map(|v| json!({
                    "width": v.width,
                    "height": v.height,
                    "deviceScaleFactor": v.device_scale_factor,
                    "mobile": v.mobile,
                })),
                "geolocation": geolocation.map(|g| json!({
                    "latitude": g.latitude,
                    "longitude": g.longitude,
                    "accuracy": g.accuracy,
                })),
                "media": media.map(media_status),
                "savedMedia": saved.map(media_status),
                "blockedUrls": blocklist,
            }),
            format,
        )?));
    }
    let mut out = String::new();
    if let Some(vp) = viewport {
        out.push_str(&format!(
            "Viewport: {}x{} (scale: {}, mobile: {})\n",
            vp.width, vp.height, vp.device_scale_factor, vp.mobile
        ));
    }
    if let Some(geo) = geolocation {
        out.push_str(&format!(
            "Geolocation: {}, {} (acc: {}m)\n",
            geo.latitude, geo.longitude, geo.accuracy
        ));
    }
    if let Some(media) = media {
        out.push_str(&format!("Media: {}\n", describe_media_status(media)));
    } else if let Some(media) = saved {
        out.push_str(&format!(
            "Media: {} (saved for this tab, not applied: the daemon could not attach to it \
             this time)\n",
            describe_media_status(media)
        ));
    }
    if !blocklist.is_empty() {
        out.push_str("Blocked URLs:\n");
        for p in blocklist {
            out.push_str(&format!("  {p}\n"));
        }
    }
    if out.is_empty() {
        out.push_str("No emulation overrides active.");
    }
    Ok(CommandResult::output(out.trim_end().to_string()))
}

/// The outcome of a command that changed something: a prose summary in text
/// mode, a list of actions otherwise.
fn applied_actions(actions: Vec<String>, format: OutputFormat) -> Result<CommandResult> {
    if format.is_text() {
        Ok(CommandResult::output(actions.join(", ")))
    } else {
        Ok(CommandResult::output(format_structured(
            &json!({"actions": actions}),
            format,
        )?))
    }
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
        if let Some(geolocation) = &self.geolocation {
            parse_geolocation(geolocation)?;
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

/// Chrome drops an override when the session that set it detaches. A one-off
/// session (no daemon, or it could not attach to the tab) detaches as the
/// command ends, so a media override set there would be gone by then, and the
/// value the client keeps would belong to no tab: the next tab whose setup
/// fails would get it. Only `emulate` sets media, so unlike the viewport it
/// has no use within a single command.
fn check_media_session(params: &EmulateParams, persistent: bool) -> Result<()> {
    if (params.media.is_some() || !params.media_features.is_empty()) && !persistent {
        anyhow::bail!(
            "CSS media emulation needs the daemon's long-lived session for this tab, which this \
             command is not running in (no daemon, or it could not attach), so the override \
             would end with the command. Run it again once the daemon is up."
        );
    }
    Ok(())
}

/// Execute the unified emulation command.
pub async fn emulate(
    client: &mut CdpClient,
    session_id: &str,
    format: OutputFormat,
    params: EmulateParams,
) -> Result<CommandResult> {
    emulate_tab(client, session_id, format, None, params).await
}

/// [`emulate`] for the daemon, which knows the tab, `target_id`. When it could
/// not attach its own session to the tab, the command runs on a one-off
/// session, and the tab's media override waits in `emulation_saved` for the
/// daemon's next session on it, which would put it back.
pub(crate) async fn emulate_tab(
    client: &mut CdpClient,
    session_id: &str,
    format: OutputFormat,
    target_id: Option<&str>,
    params: EmulateParams,
) -> Result<CommandResult> {
    // CLI and daemon validate before building these params, but `emulate` is
    // public: a library caller must not get a viewport applied and cached
    // before an invalid media feature stops the command.
    params.validate()?;
    let persistent = client.persistent_session.as_deref() == Some(session_id);
    check_media_session(&params, persistent)?;
    let saved_tab = target_id.filter(|_| !persistent);
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

    // `--clear-all` only needs a media call when there is something to
    // clear: a dialog opening after the other resets must not turn an
    // otherwise successful clear into a failure. An explicit `--clear-media`
    // always sends it.
    let saved_has_media = saved_tab
        .and_then(|t| client.emulation_saved.get(t))
        .is_some_and(|s| s.media.is_some());
    // `client.media` belongs to the persistent session; a one-off session on
    // another tab must inspect and update only that tab's saved entry.
    let target_has_media = if persistent {
        client.media.is_some()
    } else {
        saved_has_media
    };
    if params.clear_media || (params.clear_all && target_has_media) {
        // Only forget the old override once Chrome acknowledges the reset. A
        // dialog can stop the call before it is even written, in which case
        // Chrome still has `print` and a forgotten cache would make `--clear-all`
        // report success without clearing it. An unacknowledged clear leaves a
        // pending-reset marker instead of the stale value (see `media_after_clear`).
        let result = client
            .send_to_target(
                session_id,
                "Emulation.setEmulatedMedia",
                MediaOverride::default().cdp_params(),
            )
            .await;
        let acknowledged = result.is_ok();
        if persistent {
            media_after_clear(&mut client.media, acknowledged);
        } else if let Some(saved) = saved_tab.and_then(|t| client.emulation_saved.get_mut(t)) {
            media_after_clear(&mut saved.media, acknowledged);
        }
        result?;
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
        let (lat, lon) = parse_geolocation(&geo_str)?;
        let acc = params.accuracy.unwrap_or(100.0);

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
        let saved_media = saved_tab
            .and_then(|t| client.emulation_saved.get(t))
            .and_then(|saved| saved.media.as_ref());
        return current_overrides(
            client.viewport.as_ref(),
            client.geolocation.as_ref(),
            client.media.as_ref().filter(|_| persistent),
            saved_media,
            &client.blocklist,
            format,
        );
    }

    applied_actions(actions, format)
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

    /// Coordinates are checked before any override is sent; southern and
    /// western ones are negative and stay valid.
    #[test]
    fn validate_checks_geolocation_before_anything_is_applied() {
        for (raw, expected) in [
            ("37.77,-122.41", (37.77, -122.41)),
            ("-33.86,151.2", (-33.86, 151.2)),
            (" -90 , 180 ", (-90.0, 180.0)),
        ] {
            assert_eq!(parse_geolocation(raw).unwrap(), expected, "{raw}");
        }
        for raw in [
            "--clear-all",
            "37.77",
            "37.77,-122.41,5",
            "north,1",
            "1,",
            "91,0",
            "0,-180.5",
            "NaN,0",
            "0,inf",
        ] {
            let mut p = params();
            p.viewport = Some("800x600".into());
            p.geolocation = Some(raw.into());
            let err = p.validate().unwrap_err().to_string();
            assert!(
                err.starts_with(&format!("Invalid --geolocation '{raw}'")),
                "{err}"
            );
        }
    }

    /// `--geolocation` takes values that start with `-`, so clap takes a flag
    /// right after it as the value. Validation must reject that, or `emulate`
    /// would apply and cache the viewport before failing.
    #[test]
    fn a_flag_taken_as_the_geolocation_fails_validation() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "chrome-devtools",
            "emulate",
            "--viewport",
            "800x600",
            "--geolocation",
            "--clear-all",
        ])
        .unwrap();
        let crate::Commands::Emulate {
            viewport,
            geolocation,
            clear_all,
            ..
        } = cli.command
        else {
            panic!("expected emulate");
        };
        assert_eq!(geolocation.as_deref(), Some("--clear-all"));
        assert!(!clear_all);
        let p = EmulateParams {
            viewport,
            geolocation,
            ..params()
        };
        let err = p.validate().unwrap_err().to_string();
        assert!(err.contains("Invalid --geolocation '--clear-all'"), "{err}");
    }

    /// A one-off session takes its overrides with it when it detaches.
    #[test]
    fn media_needs_the_daemons_session() {
        let mut p = params();
        p.media = Some("print".into());
        let err = check_media_session(&p, false).unwrap_err().to_string();
        assert!(err.contains("long-lived session"), "{err}");
        assert!(check_media_session(&p, true).is_ok());
        let mut p = params();
        p.media_features = vec!["prefers-color-scheme=dark".into()];
        assert!(check_media_session(&p, false).is_err());
        // Clearing leaves nothing behind, and the viewport applies to the
        // command's own navigation.
        let mut p = params();
        p.clear_media = true;
        p.viewport = Some("800x600".into());
        assert!(check_media_session(&p, false).is_ok());
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

    /// A clear is only forgotten once acknowledged. Until then a pending reset
    /// keeps `--clear-all` retrying and status from claiming media is cleared,
    /// but the old value is not merged into a later media update.
    #[test]
    fn an_unacknowledged_media_clear_keeps_a_pending_reset() {
        let print = MediaOverride {
            media: Some("print".into()),
            features: vec![],
        };
        let mut cache = Some(print.clone());
        media_after_clear(&mut cache, false);
        assert_eq!(cache, Some(MediaOverride::default()));
        assert_eq!(
            describe_media_status(cache.as_ref().unwrap()),
            "a clear was requested but not confirmed (it may still be active); rerun with \
             --clear-media"
        );
        let next = merge_media(
            cache.clone().unwrap_or_default(),
            None,
            vec![("prefers-color-scheme".into(), "dark".into())],
        );
        assert_eq!(next.media, None);

        media_after_clear(&mut cache, true);
        assert_eq!(cache, None);
        // A tab with no active override of its own must not gain one from a
        // failed clear of another tab's saved state.
        let mut absent = None;
        media_after_clear(&mut absent, false);
        assert_eq!(absent, None);
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

    /// Both outcomes of `emulate` are data under `--json`/`--toon`, including
    /// active, saved and pending-reset media status. Text keeps the prose.
    #[test]
    fn emulate_results_are_structured_outside_text_mode() {
        let applied = applied_actions(vec!["Viewport cleared".into()], OutputFormat::Json).unwrap();
        let json: serde_json::Value = serde_json::from_str(&applied.output).unwrap();
        assert_eq!(json, json!({"actions": ["Viewport cleared"]}));

        let pending = MediaOverride::default();
        let saved = MediaOverride {
            media: Some("print".into()),
            features: vec![("prefers-color-scheme".into(), "dark".into())],
        };
        let result = current_overrides(
            None,
            None,
            Some(&pending),
            Some(&saved),
            &["*.png".into()],
            OutputFormat::Json,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(json["media"], json!({"pendingReset": true}));
        assert_eq!(
            json["savedMedia"],
            json!({"media": "print", "features": [["prefers-color-scheme", "dark"]]})
        );
        assert_eq!(json["blockedUrls"], json!(["*.png"]));
        assert!(json["viewport"].is_null());

        let viewport = ViewportOverride {
            width: 800,
            height: 600,
            device_scale_factor: 2.0,
            mobile: true,
        };
        let geolocation = GeolocationOverride {
            latitude: 37.77,
            longitude: -122.41,
            accuracy: 100.0,
        };
        let toon = current_overrides(
            Some(&viewport),
            Some(&geolocation),
            None,
            None,
            &[],
            OutputFormat::Toon,
        )
        .unwrap();
        assert!(toon.output.contains("viewport"), "{}", toon.output);
        assert!(!toon.output.contains("Viewport:"), "{}", toon.output);
    }
}

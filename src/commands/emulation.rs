use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Set geolocation override for the current page.
///
/// Uses `Emulation.setGeolocationOverride` CDP method.
/// Pass `--clear` to remove the override.
pub async fn set_geolocation(
    client: &mut CdpClient,
    session_id: &str,
    latitude: Option<f64>,
    longitude: Option<f64>,
    accuracy: Option<f64>,
    clear: bool,
) -> Result<CommandResult> {
    if clear {
        client
            .send_to_target(session_id, "Emulation.clearGeolocationOverride", json!({}))
            .await?;
        return Ok(CommandResult::output("Geolocation override cleared".to_string()));
    }

    let lat = latitude.ok_or_else(|| anyhow::anyhow!("latitude required (or use --clear)"))?;
    let lon = longitude.ok_or_else(|| anyhow::anyhow!("longitude required (or use --clear)"))?;
    let acc = accuracy.unwrap_or(100.0);

    client
        .send_to_target(
            session_id,
            "Emulation.setGeolocationOverride",
            json!({
                "latitude": lat,
                "longitude": lon,
                "accuracy": acc,
            }),
        )
        .await?;

    Ok(CommandResult::output(format!(
        "Geolocation set to {lat}, {lon} (accuracy: {acc}m)"
    )))
}

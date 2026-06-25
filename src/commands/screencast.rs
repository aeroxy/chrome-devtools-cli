use anyhow::{bail, Result};
use serde_json::json;
use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Start screencast recording to a specified output file path.
pub async fn screencast_start(
    client: &mut CdpClient,
    session_id: &str,
    output: &str,
    format: Option<&str>,
) -> Result<CommandResult> {
    // Check extension case-insensitively and normalize it
    let ext = std::path::Path::new(output)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase());

    let selected_format = match format {
        Some(f) => f.to_lowercase(),
        None => ext.unwrap_or_else(|| "mp4".to_string()),
    };

    if selected_format != "mp4" && selected_format != "webm" {
        bail!(
            "Unsupported screencast format \"{}\". Supported formats: mp4, webm",
            selected_format
        );
    }

    // Enable Page.startScreencast via CDP
    client
        .send_to_target(
            session_id,
            "Page.startScreencast",
            json!({
                "format": selected_format,
                "everyNthFrame": 1,
            }),
        )
        .await?;

    Ok(CommandResult::output(format!(
        "Screencast recording successfully started. Output will be saved as {}.",
        output
    )))
}

/// Stop screencast recording and finalize the output file.
pub async fn screencast_stop(
    client: &mut CdpClient,
    session_id: &str,
) -> Result<CommandResult> {
    // Disable Page.stopScreencast via CDP
    client
        .send_to_target(session_id, "Page.stopScreencast", json!({}))
        .await?;

    Ok(CommandResult::output(
        "Screencast recording successfully stopped and saved.".to_string(),
    ))
}

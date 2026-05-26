use anyhow::Result;
use serde_json::json;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Take an accessibility tree snapshot of the current page.
pub async fn take_snapshot(
    client: &mut CdpClient,
    session_id: &str,
    as_json: bool,
    output: Option<&str>,
) -> Result<CommandResult> {
    let result = client
        .send_to_target(session_id, "Accessibility.getFullAXTree", json!({}))
        .await?;

    let content = if as_json {
        serde_json::to_string_pretty(&result)?
    } else if let Some(nodes) = result["nodes"].as_array() {
        let mut out = String::new();
        for node in nodes {
            let role = node["role"]["value"].as_str().unwrap_or("");
            let name = node["name"]["value"].as_str().unwrap_or("");
            let node_id = node["nodeId"].as_str().unwrap_or("");

            if role == "none" || role == "generic" || role == "Ignored" {
                continue;
            }

            let depth = node["depth"].as_u64().unwrap_or(0) as usize;
            let indent = "  ".repeat(depth);

            if name.is_empty() {
                writeln!(out, "{indent}[{role}] #{node_id}").unwrap();
            } else {
                writeln!(out, "{indent}[{role}] \"{name}\" #{node_id}").unwrap();
            }
        }
        out
    } else {
        serde_json::to_string_pretty(&result)?
    };

    Ok(CommandResult::output(content).save_output(output).await?)
}

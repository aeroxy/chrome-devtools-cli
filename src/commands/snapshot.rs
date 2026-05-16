use anyhow::Result;
use serde_json::json;
use std::fmt::Write;
use std::fs;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

pub async fn take_snapshot(
    client: &mut CdpClient,
    session_id: &str,
    as_json: bool,
    output: Option<&str>,
) -> Result<CommandResult> {
    let result = client
        .send_to_target(session_id, "Accessibility.getFullAXTree", json!({}))
        .await?;

if as_json {
        let json = serde_json::to_string_pretty(&result)?;
        if let Some(path) = output {
            tokio::fs::write(path, &json).await?;
            return Ok(CommandResult::output(format!("Snapshot saved to {path}")));
        }
        return Ok(CommandResult::output(json));
    }

    let nodes = result["nodes"].as_array();
    if let Some(nodes) = nodes {
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
        if let Some(path) = output {
            tokio::fs::write(path, &out).await?;
            return Ok(CommandResult::output(format!("Snapshot saved to {path}")));
        }
        Ok(CommandResult::output(out))
    } else {
        let json = serde_json::to_string_pretty(&result)?;
        if let Some(path) = output {
            tokio::fs::write(path, &json).await?;
            return Ok(CommandResult::output(format!("Snapshot saved to {path}")));
        }
        Ok(CommandResult::output(json))
    }
}

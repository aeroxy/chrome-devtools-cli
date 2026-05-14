use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

pub async fn list_3p_tools(
    client: &mut CdpClient,
    session_id: &str,
    json_output: bool,
) -> Result<CommandResult> {
    let expr = r#"(() => {
        const dtmcp = window.__dtmcp;
        if (!dtmcp || !dtmcp.toolGroup) {
            return JSON.stringify({ tools: [] });
        }
        return JSON.stringify({
            name: dtmcp.toolGroup.name,
            description: dtmcp.toolGroup.description,
            tools: dtmcp.toolGroup.tools.map(t => ({
                name: t.name,
                description: t.description,
                inputSchema: t.inputSchema
            }))
        });
    })()"#;

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true}),
        )
        .await?;

    let val_str = result["result"]["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Failed to list third-party tools"))?;

    if json_output {
        Ok(CommandResult::output(val_str.to_string()))
    } else {
        let val: serde_json::Value = serde_json::from_str(val_str)?;
        let tools = val["tools"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid response from page"))?;

        if tools.is_empty() {
            return Ok(CommandResult::output("No third-party developer tools found on this page.".to_string()));
        }

        let mut output = String::new();
        if let Some(name) = val["name"].as_str() {
            output.push_str(&format!("Tool Group: {}\n", name));
        }
        if let Some(desc) = val["description"].as_str() {
            output.push_str(&format!("Description: {}\n", desc));
        }
        output.push_str("\nAvailable Tools:\n");

        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("unknown");
            let desc = tool["description"].as_str().unwrap_or("");
            output.push_str(&format!("- {}: {}\n", name, desc));
        }

        Ok(CommandResult::output(output))
    }
}

pub async fn execute_3p_tool(
    client: &mut CdpClient,
    session_id: &str,
    name: &str,
    params: Option<&str>,
    json_output: bool,
) -> Result<CommandResult> {
    let params_json = params.unwrap_or("{}");
    // Basic validation of params
    let parsed_params: serde_json::Value = serde_json::from_str(params_json)
        .map_err(|e| anyhow::anyhow!("Invalid JSON parameters: {e}"))?;

    // Safe escaping of name and params: Re-serialize to get valid JSON literals
    let safe_name_json = serde_json::to_string(name)?;
    let safe_params_json = serde_json::to_string(&parsed_params)?;

    let expr = format!(
        r#"(async () => {{
        const dtmcp = window.__dtmcp;
        if (!dtmcp || !dtmcp.executeTool) {{
            return JSON.stringify({{ error: 'Third-party tools not supported on this page' }});
        }}
        try {{
            const params = {safe_params_json};
            const result = await dtmcp.executeTool({safe_name_json}, params);
            return JSON.stringify({{ result }});
        }} catch (e) {{
            return JSON.stringify({{ error: e.message || String(e) }});
        }}
    }})()"#
    );

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
        )
        .await?;

    let val_str = result["result"]["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Failed to execute third-party tool"))?;

    let val: serde_json::Value = serde_json::from_str(val_str)?;
    if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
        bail!("Tool execution error: {err}");
    }

    if json_output {
        Ok(CommandResult::output(val["result"].to_string()))
    } else {
        Ok(CommandResult::output(format!(
            "Successfully executed tool '{}'. Result:\n{}",
            name,
            serde_json::to_string_pretty(&val["result"])?
        )))
    }
}

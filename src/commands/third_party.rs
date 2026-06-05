use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// List third-party DevTools tools registered via __dtmcp on the page.
pub async fn list_3p_tools(
    client: &mut CdpClient,
    session_id: &str,
    json_output: bool,
) -> Result<CommandResult> {
    let expr = r#"(() => {
        const dtmcp = window.__dtmcp;
        if (!dtmcp) {
            return JSON.stringify({ groups: [] });
        }
        if (Array.isArray(dtmcp.toolGroups) && dtmcp.toolGroups.length > 0) {
            return JSON.stringify({
                groups: dtmcp.toolGroups.map(g => ({
                    name: g.name,
                    description: g.description,
                    tools: (g.tools || []).map(t => ({
                        name: t.name,
                        description: t.description,
                        inputSchema: t.inputSchema
                    }))
                }))
            });
        }
        if (dtmcp.toolGroup) {
            return JSON.stringify({
                groups: [{
                    name: dtmcp.toolGroup.name,
                    description: dtmcp.toolGroup.description,
                    tools: (dtmcp.toolGroup.tools || []).map(t => ({
                        name: t.name,
                        description: t.description,
                        inputSchema: t.inputSchema
                    }))
                }]
            });
        }
        return JSON.stringify({ groups: [] });
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
        let groups = val["groups"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid response from page"))?;

        let total_tools: usize = groups
            .iter()
            .filter_map(|g| g["tools"].as_array().map(|a| a.len()))
            .sum();

        if total_tools == 0 {
            return Ok(CommandResult::output(
                "No third-party developer tools found on this page.".to_string(),
            ));
        }

        let mut output = String::new();
        for (i, group) in groups.iter().enumerate() {
            if i > 0 {
                output.push('\n');
            }
            let name = group["name"].as_str().unwrap_or("unknown");
            let desc = group["description"].as_str().unwrap_or("");
            output.push_str(&format!("{}: {}\n", name, desc));
            if let Some(tools) = group["tools"].as_array() {
                if !tools.is_empty() {
                    output.push_str("Available tools:\n");
                    for tool in tools {
                        let tname = tool["name"].as_str().unwrap_or("unknown");
                        let tdesc = tool["description"].as_str().unwrap_or("");
                        output.push_str(&format!("  - {}: {}\n", tname, tdesc));
                    }
                }
            }
        }

        Ok(CommandResult::output(output))
    }
}

/// Execute a named third-party DevTools tool with optional JSON parameters.
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
        if (!dtmcp) {{
            return JSON.stringify({{ error: 'Third-party tools not supported on this page' }});
        }}
        try {{
            const params = {safe_params_json};
            if (dtmcp.executeTool) {{
                const result = await dtmcp.executeTool({safe_name_json}, params);
                return JSON.stringify({{ result }});
            }}
            const groups = dtmcp.toolGroups || (dtmcp.toolGroup ? [dtmcp.toolGroup] : []);
            for (const group of groups) {{
                const tool = (group.tools || []).find(t => t.name === {safe_name_json});
                if (tool && typeof tool.execute === 'function') {{
                    const result = await tool.execute(params);
                    return JSON.stringify({{ result }});
                }}
            }}
            return JSON.stringify({{ error: 'Tool ' + {safe_name_json} + ' not found' }});
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

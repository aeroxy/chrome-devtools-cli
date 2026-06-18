use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// List third-party DevTools tools via WebMCP (`document.modelContext`) or
/// the legacy `__dtmcp` global. WebMCP is preferred when available.
pub async fn list_3p_tools(
    client: &mut CdpClient,
    session_id: &str,
    format: OutputFormat,
) -> Result<CommandResult> {
    let expr = r#"(() => {
        if (document.modelContext && typeof document.modelContext.getTools === 'function') {
            return document.modelContext.getTools().then(tools => JSON.stringify({
                api: 'modelContext',
                groups: [{
                    name: 'WebMCP',
                    description: 'Tools exposed via the WebMCP API',
                    tools: tools.map(t => ({
                        name: t.name,
                        description: t.description,
                        inputSchema: t.inputSchema,
                        annotations: t.annotations || null,
                        origin: t.origin || null
                    }))
                }]
            })).catch(e => JSON.stringify({ error: e.message || String(e) }));
        }
        const dtmcp = window.__dtmcp;
        if (!dtmcp) {
            return JSON.stringify({ api: 'none', groups: [] });
        }
        if (Array.isArray(dtmcp.toolGroups) && dtmcp.toolGroups.length > 0) {
            return JSON.stringify({
                api: 'dtmcp',
                groups: dtmcp.toolGroups.map(g => ({
                    name: g.name,
                    description: g.description,
                    tools: (g.tools || []).map(t => ({
                        name: t.name,
                        description: t.description,
                        inputSchema: t.inputSchema,
                        annotations: null,
                        origin: null
                    }))
                }))
            });
        }
        if (dtmcp.toolGroup) {
            return JSON.stringify({
                api: 'dtmcp',
                groups: [{
                    name: dtmcp.toolGroup.name,
                    description: dtmcp.toolGroup.description,
                    tools: (dtmcp.toolGroup.tools || []).map(t => ({
                        name: t.name,
                        description: t.description,
                        inputSchema: t.inputSchema,
                        annotations: null,
                        origin: null
                    }))
                }]
            });
        }
        return JSON.stringify({ api: 'none', groups: [] });
    })()"#;

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
        )
        .await?;

    let val_str = result["result"]["value"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Failed to list third-party tools"))?;

    let val: serde_json::Value = serde_json::from_str(val_str)?;
    if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
        bail!("Failed to list tools: {err}");
    }

    if !format.is_text() {
        Ok(CommandResult::output(format_structured(&val, format)?))
    } else {
        let api = val["api"].as_str().unwrap_or("none");
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

        let api_label = match api {
            "modelContext" => "WebMCP",
            "dtmcp" => "DTMCP (legacy)",
            _ => "unknown",
        };

        let mut output = format!("API: {}\n", api_label);
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
                        output.push_str(&format!("  - {}: {}", tname, tdesc));
                        if let Some(origin) = tool["origin"].as_str() {
                            output.push_str(&format!(" [origin: {}]", origin));
                        }
                        output.push('\n');
                        if let Some(ann) = tool.get("annotations") {
                            if !ann.is_null() {
                                let parts: Vec<String> = ann
                                    .as_object()
                                    .map(|obj| {
                                        obj.iter()
                                            .map(|(k, v)| format!("{}={}", k, v))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                if !parts.is_empty() {
                                    output.push_str(&format!(
                                        "    annotations: {}\n",
                                        parts.join(", ")
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(CommandResult::output(output))
    }
}

/// Execute a named third-party DevTools tool via WebMCP or legacy `__dtmcp`.
pub async fn execute_3p_tool(
    client: &mut CdpClient,
    session_id: &str,
    name: &str,
    params: Option<&str>,
    format: OutputFormat,
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
        const params = {safe_params_json};
        let hasModelContext = false;
        if (document.modelContext && typeof document.modelContext.getTools === 'function') {{
            hasModelContext = true;
            try {{
                const tools = await document.modelContext.getTools();
                const tool = tools.find(t => t.name === {safe_name_json});
                if (tool) {{
                    const result = await document.modelContext.executeTool(tool, {safe_params_json});
                    return JSON.stringify({{ api: 'modelContext', result }});
                }}
            }} catch (e) {{
                return JSON.stringify({{ error: e.message || String(e) }});
            }}
        }}
        const dtmcp = window.__dtmcp;
        if (!dtmcp) {{
            const msg = hasModelContext
                ? 'Tool ' + {safe_name_json} + ' not found via WebMCP'
                : 'No third-party tools API available on this page';
            return JSON.stringify({{ error: msg }});
        }}
        try {{
            if (dtmcp.executeTool) {{
                const result = await dtmcp.executeTool({safe_name_json}, params);
                return JSON.stringify({{ api: 'dtmcp', result }});
            }}
            const groups = dtmcp.toolGroups || (dtmcp.toolGroup ? [dtmcp.toolGroup] : []);
            for (const group of groups) {{
                const tool = (group.tools || []).find(t => t.name === {safe_name_json});
                if (tool && typeof tool.execute === 'function') {{
                    const result = await tool.execute(params);
                    return JSON.stringify({{ api: 'dtmcp', result }});
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

    if !format.is_text() {
        Ok(CommandResult::output(format_structured(
            &val["result"],
            format,
        )?))
    } else {
        let api = val["api"].as_str().unwrap_or("unknown");
        let api_label = match api {
            "modelContext" => "WebMCP",
            "dtmcp" => "DTMCP (legacy)",
            _ => "unknown",
        };
        Ok(CommandResult::output(format!(
            "Executed '{}' via {}:\n{}",
            name,
            api_label,
            serde_json::to_string_pretty(&val["result"])?
        )))
    }
}

use anyhow::{bail, Result};
use serde_json::json;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// Ask the page for its tool groups the way DevTools does: dispatch a
/// `devtoolstooldiscovery` event whose `respondWith` hands back each group,
/// then expose what came back as `window.__dtmcp`, which the list and execute
/// expressions read, and return it. Pages that set `__dtmcp` themselves answer
/// nothing and keep theirs; groups an earlier discovery installed are dropped
/// once nothing answers, so a page that removed its listener stops listing its
/// old tools, unless the page has since put its own list in their place.
/// A group missing a field, or holding a malformed tool, is skipped with a
/// console error, and the other groups still count. Only the legacy path runs
/// this: WebMCP needs none of it, and must not fail because of it.
const DISCOVER_TOOLS_JS: &str = r#"async () => {
    const groups = [];
    const event = new CustomEvent('devtoolstooldiscovery');
    event.respondWith = (group) => {
        if (!group || typeof group.name !== 'string' || !Array.isArray(group.tools)
            || (group.description && typeof group.description !== 'string')) {
            console.error('Invalid toolGroup:', group);
            return;
        }
        for (const tool of group.tools) {
            if (!tool || typeof tool.name !== 'string' || typeof tool.description !== 'string'
                || typeof tool.inputSchema !== 'object' || typeof tool.execute !== 'function') {
                console.error('Invalid tool:', tool);
                return;
            }
        }
        groups.push(group);
    };
    window.dispatchEvent(event);
    // A listener may answer from a microtask or a zero-delay timer.
    await new Promise(r => setTimeout(r, 0));
    if (groups.length === 0) {
        const dtmcp = window.__dtmcp;
        if (dtmcp && dtmcp.__discovered && dtmcp.toolGroups === dtmcp.__discovered) {
            dtmcp.toolGroups = [];
        }
        return groups;
    }
    const dtmcp = window.__dtmcp = window.__dtmcp || {};
    dtmcp.toolGroups = groups;
    // The list itself, so a later run can tell it from one the page set.
    dtmcp.__discovered = groups;
    if (typeof dtmcp.executeTool !== 'function') {
        dtmcp.executeTool = async (toolName, args) => {
            // Outlives discovery: once that stops answering, the list is
            // empty and listing shows a legacy toolGroup, so look there too.
            const groups = Array.isArray(dtmcp.toolGroups) && dtmcp.toolGroups.length > 0
                ? dtmcp.toolGroups : (dtmcp.toolGroup ? [dtmcp.toolGroup] : []);
            for (const group of groups) {
                const tool = (group.tools || []).find(t => t.name === toolName);
                if (tool && typeof tool.execute === 'function') return await tool.execute(args);
            }
            throw new Error('Tool ' + toolName + ' not found');
        };
    }
    return groups;
}"#;

/// Tools take named parameters, so anything but a JSON object (an array, a
/// bare string) is a mistake the page would otherwise have to make sense of.
fn parse_tool_params(params: Option<&str>) -> Result<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(params.unwrap_or("{}"))
        .map_err(|e| anyhow::anyhow!("Invalid JSON parameters: {e}"))?;
    if !parsed.is_object() {
        bail!("Tool parameters must be a JSON object, e.g. '{{\"query\": \"x\"}}'");
    }
    Ok(parsed)
}

/// The page-side half of `list-3p-tools`. It is handed the discovery function
/// and calls it only once WebMCP turns out to be missing.
fn list_tools_js() -> String {
    let list_expr = r#"async (discover) => {
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
        await discover();
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
    }"#;
    format!("({list_expr})({DISCOVER_TOOLS_JS})")
}

/// List third-party DevTools tools via WebMCP (`document.modelContext`) or
/// the legacy `__dtmcp` global. WebMCP is preferred when available.
pub async fn list_3p_tools(
    client: &mut CdpClient,
    session_id: &str,
    format: OutputFormat,
) -> Result<CommandResult> {
    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": list_tools_js(), "returnByValue": true, "awaitPromise": true}),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        anyhow::bail!("{}", crate::remote_object::describe_exception(exception));
    }

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
            "none" => "None",
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
                                        obj.iter().map(|(k, v)| format!("{}={}", k, v)).collect()
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

/// The page-side half of `execute-3p-tool`: the WebMCP tool of that name if
/// there is one, then a tool discovery hands back, then the legacy `__dtmcp`.
fn execute_tool_js(name: &str, params: &serde_json::Value) -> Result<String> {
    // Safe escaping of name and params: Re-serialize to get valid JSON literals
    let safe_name_json = serde_json::to_string(name)?;
    let safe_params_json = serde_json::to_string(params)?;

    Ok(format!(
        r#"(async () => {{
        const params = {safe_params_json};
        let hasModelContext = false;
        if (document.modelContext && typeof document.modelContext.getTools === 'function') {{
            hasModelContext = true;
            try {{
                const tools = await document.modelContext.getTools();
                const tool = tools.find(t => t.name === {safe_name_json});
                if (tool) {{
                    const result = await document.modelContext.executeTool(tool, params);
                    return JSON.stringify({{ api: 'modelContext', result }});
                }}
            }} catch (e) {{
                return JSON.stringify({{ error: e.message || String(e) }});
            }}
        }}
        const discovered = await ({DISCOVER_TOOLS_JS})();
        const dtmcp = window.__dtmcp;
        if (!dtmcp) {{
            const msg = hasModelContext
                ? 'Tool ' + {safe_name_json} + ' not found via WebMCP'
                : 'No third-party tools API available on this page';
            return JSON.stringify({{ error: msg }});
        }}
        try {{
            // A discovered tool runs its own handler: a legacy executeTool the
            // page set itself need not know the tools its listener answered with.
            for (const group of discovered) {{
                const tool = (group.tools || []).find(t => t.name === {safe_name_json});
                if (tool) {{
                    const result = await tool.execute(params);
                    return JSON.stringify({{ api: 'dtmcp', result }});
                }}
            }}
            if (dtmcp.executeTool) {{
                const result = await dtmcp.executeTool({safe_name_json}, params);
                return JSON.stringify({{ api: 'dtmcp', result }});
            }}
            // Look where listing does: an empty list falls back to toolGroup.
            const groups = Array.isArray(dtmcp.toolGroups) && dtmcp.toolGroups.length > 0
                ? dtmcp.toolGroups : (dtmcp.toolGroup ? [dtmcp.toolGroup] : []);
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
    ))
}

/// Execute a named third-party DevTools tool via WebMCP or legacy `__dtmcp`.
pub async fn execute_3p_tool(
    client: &mut CdpClient,
    session_id: &str,
    name: &str,
    params: Option<&str>,
    format: OutputFormat,
) -> Result<CommandResult> {
    let expr = execute_tool_js(name, &parse_tool_params(params)?)?;

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
        )
        .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        anyhow::bail!("{}", crate::remote_object::describe_exception(exception));
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_params_must_be_an_object() {
        assert_eq!(parse_tool_params(None).unwrap(), json!({}));
        assert_eq!(
            parse_tool_params(Some(r#"{"q": 1}"#)).unwrap(),
            json!({"q": 1})
        );
        for bad in ["[]", "[1]", "\"x\"", "3", "null"] {
            let err = parse_tool_params(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("must be a JSON object"), "{bad}: {err}");
        }
        assert!(parse_tool_params(Some("{"))
            .unwrap_err()
            .to_string()
            .contains("Invalid JSON"));
    }

    /// Position of `needle` in `haystack`, which must contain it.
    fn at(haystack: &str, needle: &str) -> usize {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing"))
    }

    /// WebMCP answers before anything legacy runs: discovery, which touches
    /// the page's `__dtmcp`, comes only on the legacy path, so it can't make a
    /// working WebMCP call fail.
    #[test]
    fn webmcp_comes_before_discovery() {
        let list = list_tools_js();
        assert!(at(&list, "modelContext.getTools()") < at(&list, "await discover()"));
        let exec = execute_tool_js("t", &json!({})).unwrap();
        assert!(at(&exec, "modelContext.executeTool(") < at(&exec, "devtoolstooldiscovery"));
    }

    /// A discovered tool runs its own `execute`, ahead of a legacy
    /// `executeTool` the page set itself, which need not know it.
    #[test]
    fn discovered_tools_run_their_own_handler() {
        let exec = execute_tool_js("t", &json!({})).unwrap();
        assert!(at(&exec, "of discovered)") < at(&exec, "dtmcp.executeTool("));
    }

    /// A run that gets no answer clears only the list discovery installed,
    /// told apart by identity, never a list the page put in its place.
    /// Once discovery stops answering, its list is emptied and listing shows
    /// a legacy `toolGroup` instead, so running a tool looks there too: in
    /// the executor discovery installed, and in the fallback without one.
    #[test]
    fn running_a_tool_looks_where_listing_does() {
        let rule = "Array.isArray(dtmcp.toolGroups) && dtmcp.toolGroups.length > 0";
        assert!(list_tools_js().contains(rule));
        let executor = &DISCOVER_TOOLS_JS[at(DISCOVER_TOOLS_JS, "dtmcp.executeTool = ")..];
        assert!(executor.contains(rule), "{executor}");
        assert!(executor.contains("[dtmcp.toolGroup]"), "{executor}");
        let exec = execute_tool_js("t", &json!({})).unwrap();
        let fallback = &exec[at(&exec, "if (dtmcp.executeTool)")..];
        assert!(fallback.contains(rule), "{fallback}");
        assert!(fallback.contains("[dtmcp.toolGroup]"), "{fallback}");
    }

    #[test]
    fn discovery_clears_only_the_groups_it_installed() {
        assert!(DISCOVER_TOOLS_JS.contains("dtmcp.__discovered = groups;"));
        assert!(DISCOVER_TOOLS_JS.contains("dtmcp.toolGroups === dtmcp.__discovered"));
        assert!(!DISCOVER_TOOLS_JS.contains("__discovered = true"));
    }
}

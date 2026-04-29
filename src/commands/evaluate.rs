use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;

pub async fn evaluate(
    client: &mut CdpClient,
    session_id: &str,
    expression: &str,
    as_json: bool,
    dialog_action: Option<&str>,
) -> Result<String> {
    if dialog_action.is_some() {
        client
            .send_to_target(session_id, "Page.enable", json!({}))
            .await?;
    }

    // Prepare the evaluation command
    let id = client
        .send_raw_no_wait(
            Some(session_id),
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;

    // Wait for the response, but also handle Page.javascriptDialogOpening events
    loop {
        let resp_text = client.read_text().await?;
        let resp: serde_json::Value = serde_json::from_str(&resp_text)?;

        if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
            if let Some(error) = resp.get("error") {
                anyhow::bail!(
                    "CDP error in Runtime.evaluate: {}",
                    serde_json::to_string_pretty(error)?
                );
            }
            let result = &resp["result"];
            if let Some(exception) = result.get("exceptionDetails") {
                let text = exception["text"].as_str().unwrap_or("Unknown error");
                let desc = exception["exception"]["description"]
                    .as_str()
                    .unwrap_or(text);
                anyhow::bail!(
                    "{desc}\n\n[HINT: To explore the page DOM, use the `snapshot` command instead of `evaluate`. To interact with elements, use `click` or `fill`.]"
                );
            }

            let value = &result["result"];
            let val_type = value["type"].as_str().unwrap_or("undefined");

            let mut output = if as_json {
                if let Some(v) = value.get("value") {
                    serde_json::to_string_pretty(v)?
                } else {
                    serde_json::to_string_pretty(value)?
                }
            } else {
                match val_type {
                    "undefined" => "undefined".to_string(),
                    "string" => value["value"].as_str().unwrap_or("").to_string(),
                    _ => {
                        if let Some(v) = value.get("value") {
                            serde_json::to_string_pretty(v)?
                        } else {
                            value["description"].as_str().unwrap_or("").to_string()
                        }
                    }
                }
            };

            if expression.contains("querySelector")
                || expression.contains("document.body")
                || expression.contains("getElementById")
                || expression.contains("getElementsBy")
            {
                output.push_str("\n\n[HINT: Avoid using `evaluate` for DOM traversal. Use the `snapshot` command to get a clean accessibility tree of the page, then use `click` or `fill`.]");
            }

            return Ok(output);
        }

        // Handle dialog events if they occur
        if resp.get("method").and_then(|v| v.as_str()) == Some("Page.javascriptDialogOpening") {
            if let Some(action) = dialog_action {
                let mut params = json!({});
                match action {
                    "accept" => {
                        params["accept"] = json!(true);
                    }
                    "dismiss" => {
                        params["accept"] = json!(false);
                    }
                    text => {
                        params["accept"] = json!(true);
                        params["promptText"] = json!(text);
                    }
                }
                client
                    .send_to_target(session_id, "Page.handleJavaScriptDialog", params)
                    .await?;
            } else {
                let dialog_type = resp
                    .get("params")
                    .and_then(|p| p.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("unknown");
                let msg = resp
                    .get("params")
                    .and_then(|p| p.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                anyhow::bail!("A javascript dialog is open ({dialog_type}: {msg}). Use `evaluate` with --dialog-action to dismiss it.");
            }
        }
    }
}

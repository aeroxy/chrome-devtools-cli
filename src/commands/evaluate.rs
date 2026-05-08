use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;

pub async fn evaluate(
    client: &mut CdpClient,
    session_id: &str,
    expression: &str,
    as_json: bool,
) -> Result<String> {
    // Note: To handle JavaScript dialogs (alert, confirm, prompt) during evaluation,
    // client.dialog_action must be set to "accept", "dismiss", or a prompt response string
    // BEFORE calling this function. The underlying send_to_target call will then
    // automatically handle any Page.javascriptDialogOpening events that occur.

    let result = client
        .send_to_target(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;

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

    Ok(output)
}

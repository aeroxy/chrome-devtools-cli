use anyhow::Result;
use serde_json::json;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

pub async fn evaluate(
    client: &mut CdpClient,
    session_id: &str,
    expression: &str,
    as_json: bool,
    output: Option<&str>,
    track_navigation: bool,
) -> Result<CommandResult> {
    // Note: To handle JavaScript dialogs (alert, confirm, prompt) during evaluation,
    // client.dialog_action must be set to "accept", "dismiss", or a prompt response string
    // BEFORE calling this function. The underlying send_to_target call will then
    // automatically handle any Page.javascriptDialogOpening events that occur.

    let initial_url = if track_navigation {
        Some(client.current_url(session_id).await?)
    } else {
        None
    };

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

    let mut output_hint = if as_json {
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
        output_hint.push_str("\n\n[HINT: Avoid using `evaluate` for DOM traversal. Use the `snapshot` command to get a clean accessibility tree of the page, then use `click` or `fill`.]");
    }

    if let Some(initial_url) = initial_url {
        let new_url = client.current_url(session_id).await?;
        let result = CommandResult::output(output_hint)
            .with_navigated_to_if_changed(new_url.clone(), initial_url.clone());
        if let Some(path) = output {
            let data = result.output.as_bytes();
            tokio::fs::write(path, data).await?;
            Ok(CommandResult::output(format!("Output saved to {path}"))
                .with_navigated_to_if_changed(new_url, initial_url))
        } else {
            Ok(result)
        }
    } else {
        let result = CommandResult::output(output_hint);
        if let Some(path) = output {
            tokio::fs::write(path, result.output.as_bytes()).await?;
            Ok(CommandResult::output(format!("Output saved to {path}")))
        } else {
            Ok(result)
        }
    }
}

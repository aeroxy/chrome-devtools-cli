use anyhow::Result;
use serde_json::json;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

const EXTENSION_PREFIX: &str = "chrome-extension://";

pub async fn collect_sw_logs(
    client: &mut CdpClient,
    duration_ms: u64,
    extension_id_filter: Option<&str>,
    format: OutputFormat,
) -> Result<CommandResult> {
    let targets = client.get_all_targets().await?;

    let sw_targets: Vec<_> = targets
        .iter()
        .filter(|t| {
            t.target_type == "service_worker" && t.url.starts_with(EXTENSION_PREFIX)
        })
        .filter(|t| {
            if let Some(filter_id) = extension_id_filter {
                extract_extension_id(&t.url)
                    .map(|id| id == filter_id)
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .collect();

    if sw_targets.is_empty() {
        return Ok(CommandResult::output(
            "No extension service workers found.".to_string(),
        ));
    }

    let mut sessions = Vec::new();
    for target in &sw_targets {
        match client.attach_to_target(&target.target_id).await {
            Ok(session_id) => {
                let _ = client
                    .send_to_target(&session_id, "Runtime.enable", json!({}))
                    .await;
                sessions.push(((*target).clone(), session_id));
            }
            Err(e) => {
                eprintln!(
                    "Warning: failed to attach to service worker {}: {e}",
                    target.url
                );
            }
        }
    }

    let events = client.read_events_for(duration_ms).await?;

    for (_, session_id) in &sessions {
        let _ = client
            .send_to_target(session_id, "Runtime.disable", json!({}))
            .await;
        let _ = client.detach_from_target(session_id).await;
    }

    let mut messages = Vec::new();
    for event in &events {
        let method = event["method"].as_str().unwrap_or("");
        let params = &event["params"];
        let session_id = event["sessionId"].as_str().unwrap_or("");

        let source = sessions
            .iter()
            .find(|(_, sid)| sid == session_id)
            .map(|(t, _)| t.url.as_str())
            .unwrap_or("unknown");

        let ext_id = extract_extension_id(source).unwrap_or_default();

        match method {
            "Runtime.consoleAPICalled" => {
                let msg_type = params["type"].as_str().unwrap_or("log");
                let args: Vec<String> = params["args"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|arg| {
                                arg["value"]
                                    .as_str()
                                    .or_else(|| arg["description"].as_str())
                                    .unwrap_or("<object>")
                                    .to_string()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let text = args.join(" ");

                messages.push(json!({
                    "extensionId": ext_id,
                    "type": msg_type,
                    "text": text,
                    "source": source,
                }));
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let text = details["text"].as_str().unwrap_or("Unknown error");
                let description = details["exception"]["description"]
                    .as_str()
                    .unwrap_or(text);

                messages.push(json!({
                    "extensionId": ext_id,
                    "type": "exception",
                    "text": description,
                    "source": source,
                }));
            }
            _ => {}
        }
    }

    if format.is_text() {
        if messages.is_empty() {
            return Ok(CommandResult::output(
                "No service worker logs collected.".to_string(),
            ));
        }
        let mut out = String::new();
        for msg in &messages {
            let ext_id = msg["extensionId"].as_str().unwrap_or("?");
            let msg_type = msg["type"].as_str().unwrap_or("?");
            let text = msg["text"].as_str().unwrap_or("");
            writeln!(out, "[{ext_id}] [{msg_type}] {text}").unwrap();
        }
        Ok(CommandResult::output(out))
    } else {
        let value = serde_json::to_value(&messages)?;
        Ok(CommandResult::output(format_structured(&value, format)?))
    }
}

fn extract_extension_id(url: &str) -> Option<String> {
    let rest = url.strip_prefix(EXTENSION_PREFIX)?;
    let slash = rest.find('/').unwrap_or(rest.len());
    Some(rest[..slash].to_string())
}

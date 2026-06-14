use anyhow::Result;
use serde_json::json;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

pub async fn collect_console(
    client: &mut CdpClient,
    session_id: &str,
    duration_ms: u64,
    type_filter: Vec<String>,
    format: OutputFormat,
) -> Result<CommandResult> {
    let events = if duration_ms > 0 {
        // Live mode: the persistent session already has Runtime enabled,
        // so we skip enabling it on the command's own session to avoid
        // duplicate events from two sessions both receiving the same messages.
        if client.persistent_session.is_none() {
            client
                .send_to_target(session_id, "Runtime.enable", json!({}))
                .await?;
        }
        let events = client.read_events_for(duration_ms).await?;
        if client.persistent_session.is_none() {
            let _ = client
                .send_to_target(session_id, "Runtime.disable", json!({}))
                .await;
        }
        events
    } else {
        // Drain mode: return accumulated events from persistent session
        client.drain_console_events()
    };

    let messages = process_console_events(&events, &type_filter);

    format_console_output(&messages, format)
}

fn process_console_events(events: &[serde_json::Value], type_filter: &[String]) -> Vec<serde_json::Value> {
    let filter_set: Option<std::collections::HashSet<&str>> = if type_filter.is_empty() {
        None
    } else {
        Some(type_filter.iter().map(|s| s.as_str()).collect())
    };

    let mut messages = Vec::new();

    for event in events {
        let method = event["method"].as_str().unwrap_or("");
        let params = &event["params"];

        match method {
            "Runtime.consoleAPICalled" => {
                let msg_type = params["type"].as_str().unwrap_or("log");
                if let Some(ref set) = filter_set {
                    if !set.contains(msg_type) {
                        continue;
                    }
                }
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
                let timestamp = params["timestamp"].as_f64().unwrap_or(0.0);

                messages.push(json!({
                    "type": msg_type,
                    "text": text,
                    "timestamp": timestamp,
                }));
            }
            "Runtime.exceptionThrown" => {
                if let Some(ref set) = filter_set {
                    if !set.contains("exception") && !set.contains("error") {
                        continue;
                    }
                }
                let details = &params["exceptionDetails"];
                let text = details["text"].as_str().unwrap_or("Unknown error");
                let description = details["exception"]["description"]
                    .as_str()
                    .unwrap_or(text);
                let timestamp = params["timestamp"].as_f64().unwrap_or(0.0);

                messages.push(json!({
                    "type": "exception",
                    "text": description,
                    "timestamp": timestamp,
                }));
            }
            _ => {}
        }
    }

    messages
}

fn format_console_output(messages: &[serde_json::Value], format: OutputFormat) -> Result<CommandResult> {
    if format.is_text() {
        if messages.is_empty() {
            return Ok(CommandResult::output("No console messages collected.".to_string()));
        }
        let mut out = String::new();
        for msg in messages {
            let msg_type = msg["type"].as_str().unwrap_or("?");
            let text = msg["text"].as_str().unwrap_or("");
            writeln!(out, "[{msg_type}] {text}").unwrap();
        }
        Ok(CommandResult::output(out))
    } else {
        let value = serde_json::to_value(messages)?;
        Ok(CommandResult::output(format_structured(&value, format)?))
    }
}

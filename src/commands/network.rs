use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

pub async fn collect_network(
    client: &mut CdpClient,
    session_id: &str,
    duration_ms: u64,
    type_filter: Vec<String>,
    format: OutputFormat,
) -> Result<CommandResult> {
    let events = if duration_ms > 0 {
        // Live mode: the persistent session already has Network enabled,
        // so we skip enabling it on the command's own session to avoid
        // duplicate events from two sessions both receiving the same messages.
        if client.persistent_session.is_none() {
            client
                .send_to_target(session_id, "Network.enable", json!({}))
                .await?;
        }
        let events = client.read_events_for(duration_ms).await?;
        if client.persistent_session.is_none() {
            let _ = client
                .send_to_target(session_id, "Network.disable", json!({}))
                .await;
        }
        events
    } else {
        // Drain mode: return accumulated events from persistent session
        client.drain_network_events()
    };

    let requests = process_network_events(&events, &type_filter);
    format_network_output(&requests, format)
}

fn process_network_events(
    events: &[serde_json::Value],
    type_filter: &[String],
) -> Vec<serde_json::Value> {
    let filter_set: Option<std::collections::HashSet<&str>> = if type_filter.is_empty() {
        None
    } else {
        Some(type_filter.iter().map(|s| s.as_str()).collect())
    };

    let mut requests: HashMap<String, serde_json::Value> = HashMap::new();

    for event in events {
        let method = event["method"].as_str().unwrap_or("");
        let params = &event["params"];
        let request_id = params["requestId"].as_str().unwrap_or("").to_string();

        match method {
            "Network.requestWillBeSent" => {
                let url = params["request"]["url"].as_str().unwrap_or("").to_string();
                let resource_type = params["type"].as_str().unwrap_or("Other").to_string();
                let http_method = params["request"]["method"]
                    .as_str()
                    .unwrap_or("GET")
                    .to_string();

                requests.insert(
                    request_id,
                    json!({
                        "url": url,
                        "method": http_method,
                        "resourceType": resource_type,
                        "status": null,
                    }),
                );
            }
            "Network.responseReceived" => {
                if let Some(req) = requests.get_mut(&request_id) {
                    let status = params["response"]["status"].as_u64().unwrap_or(0);
                    let status_text = params["response"]["statusText"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    req["status"] = json!(status);
                    req["statusText"] = json!(status_text);
                }
            }
            "Network.loadingFailed" => {
                if let Some(req) = requests.get_mut(&request_id) {
                    let error = params["errorText"].as_str().unwrap_or("failed").to_string();
                    req["status"] = json!("failed");
                    req["error"] = json!(error);
                }
            }
            _ => {}
        }
    }

    let mut filtered: Vec<serde_json::Value> = requests.into_values().collect();
    if let Some(ref set) = filter_set {
        filtered.retain(|r| {
            r["resourceType"]
                .as_str()
                .map(|t| set.contains(t))
                .unwrap_or(false)
        });
    }

    // Sort by URL for stable output
    filtered.sort_by(|a, b| {
        a["url"]
            .as_str()
            .unwrap_or("")
            .cmp(b["url"].as_str().unwrap_or(""))
    });

    filtered
}

fn format_network_output(
    requests: &[serde_json::Value],
    format: OutputFormat,
) -> Result<CommandResult> {
    if format.is_text() {
        if requests.is_empty() {
            return Ok(CommandResult::output(
                "No network requests collected.".to_string(),
            ));
        }
        let mut out = String::new();
        writeln!(out, "{:<8} {:<6} {}", "STATUS", "METHOD", "URL").unwrap();
        writeln!(out, "{}", "-".repeat(72)).unwrap();
        for req in requests {
            let status = match &req["status"] {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                _ => "?".to_string(),
            };
            let method = req["method"].as_str().unwrap_or("?");
            let url = req["url"].as_str().unwrap_or("?");
            writeln!(out, "{:<8} {:<6} {}", status, method, url).unwrap();
        }
        Ok(CommandResult::output(out))
    } else {
        let value = serde_json::to_value(requests)?;
        Ok(CommandResult::output(format_structured(&value, format)?))
    }
}

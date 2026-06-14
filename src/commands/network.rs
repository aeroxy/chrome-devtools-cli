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
    let events = if duration_ms == 0 {
        // Drain mode: return accumulated events from the persistent session.
        client.drain_network_events()
    } else if client.persistent_session.is_some() {
        // Live mode, daemon: the persistent session already has Network enabled
        // and push_event accumulates its events into network_events. Read for the
        // window so they land in the buffer, then drain the buffer as the single
        // source of truth. Draining also consumes them, so a later drain can't
        // repeat them (read_events_for's own return value would double-count).
        client.read_events_for(duration_ms).await?;
        client.drain_network_events()
    } else {
        // Live mode, direct/fallback: no persistent buffer. Enable Network on our
        // own session, collect for the window, disable, and return what we read.
        client
            .send_to_target(session_id, "Network.enable", json!({}))
            .await?;
        let events_result = client.read_events_for(duration_ms).await;
        let _ = client
            .send_to_target(session_id, "Network.disable", json!({}))
            .await;
        events_result?
    };

    let requests = process_network_events(&events, &type_filter);
    format_network_output(&requests, format)
}

fn process_network_events(
    events: &[serde_json::Value],
    type_filter: &[String],
) -> Vec<serde_json::Value> {
    // Match case-insensitively: CDP emits capitalized resource types
    // (e.g. "Fetch", "XHR"), but the CLI help documents lowercase
    // (e.g. "fetch", "xhr"). Normalize both sides to lowercase so the
    // documented input actually matches.
    let filter_set: Option<std::collections::HashSet<String>> = if type_filter.is_empty() {
        None
    } else {
        Some(type_filter.iter().map(|s| s.to_lowercase()).collect())
    };

    let mut requests: HashMap<String, serde_json::Value> = HashMap::new();
    // First-seen order of request IDs, so output follows the network timeline
    // rather than an arbitrary HashMap / alphabetical ordering.
    let mut order: Vec<String> = Vec::new();

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

                // Record first-seen position; redirects reuse the same requestId
                // and update the record in place without changing its position.
                if !requests.contains_key(&request_id) {
                    order.push(request_id.clone());
                }
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

    // Emit in first-seen (chronological) order rather than HashMap order.
    let mut filtered: Vec<serde_json::Value> = order
        .into_iter()
        .filter_map(|id| requests.remove(&id))
        .collect();
    if let Some(ref set) = filter_set {
        filtered.retain(|r| {
            r["resourceType"]
                .as_str()
                .map(|t| set.contains(&t.to_lowercase()))
                .unwrap_or(false)
        });
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_process_network_events_merges_request_and_response() {
        let events = vec![
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-1",
                    "request": {"url": "https://example.com/api", "method": "POST"},
                    "type": "Fetch"
                }
            }),
            json!({
                "method": "Network.responseReceived",
                "params": {
                    "requestId": "req-1",
                    "response": {"status": 201, "statusText": "Created"}
                }
            }),
        ];
        let out = process_network_events(&events, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["url"], "https://example.com/api");
        assert_eq!(out[0]["method"], "POST");
        assert_eq!(out[0]["resourceType"], "Fetch");
        assert_eq!(out[0]["status"], 201);
        assert_eq!(out[0]["statusText"], "Created");
    }

    #[test]
    fn test_process_network_events_loading_failed() {
        let events = vec![
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-2",
                    "request": {"url": "https://blocked.com/ads.js", "method": "GET"},
                    "type": "Script"
                }
            }),
            json!({
                "method": "Network.loadingFailed",
                "params": {
                    "requestId": "req-2",
                    "errorText": "net::ERR_BLOCKED_BY_CLIENT"
                }
            }),
        ];
        let out = process_network_events(&events, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["status"], "failed");
        assert_eq!(out[0]["error"], "net::ERR_BLOCKED_BY_CLIENT");
    }

    #[test]
    fn test_process_network_events_type_filter() {
        let events = vec![
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-a",
                    "request": {"url": "https://example.com/page", "method": "GET"},
                    "type": "Document"
                }
            }),
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-b",
                    "request": {"url": "https://example.com/script.js", "method": "GET"},
                    "type": "Script"
                }
            }),
        ];
        let filter = vec!["Document".to_string()];
        let out = process_network_events(&events, &filter);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["url"], "https://example.com/page");
    }

    #[test]
    fn test_process_network_events_type_filter_case_insensitive() {
        // The CLI help documents lowercase resource types, but CDP emits them
        // capitalized. Documented lowercase input must still match, and the
        // output must preserve the original CDP casing.
        let events = vec![
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-a",
                    "request": {"url": "https://example.com/api", "method": "GET"},
                    "type": "Fetch"
                }
            }),
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-b",
                    "request": {"url": "https://example.com/x", "method": "GET"},
                    "type": "XHR"
                }
            }),
        ];
        // lowercase "fetch" (as documented) and odd-cased "xhr"/"Xhr".
        let filter = vec!["fetch".to_string(), "Xhr".to_string()];
        let out = process_network_events(&events, &filter);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["resourceType"], "Fetch");
        assert_eq!(out[1]["resourceType"], "XHR");
    }

    #[test]
    fn test_process_network_events_preserves_chronological_order() {
        // Output should follow first-seen (request order), NOT alphabetical URL.
        let events = vec![
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "r1",
                    "request": {"url": "https://z.example.com/", "method": "GET"},
                    "type": "Document"
                }
            }),
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "r2",
                    "request": {"url": "https://a.example.com/", "method": "GET"},
                    "type": "Document"
                }
            }),
        ];
        let out = process_network_events(&events, &[]);
        assert_eq!(out[0]["url"], "https://z.example.com/");
        assert_eq!(out[1]["url"], "https://a.example.com/");
    }

    #[test]
    fn test_process_network_events_ignores_unknown_methods() {
        let events = vec![
            json!({"method": "Network.dataReceived", "params": {"requestId": "r1", "dataLength": 100}}),
        ];
        let out = process_network_events(&events, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_process_network_events_handles_response_without_request() {
        // responseReceived arrives with no prior requestWillBeSent — should be dropped
        let events = vec![json!({
            "method": "Network.responseReceived",
            "params": {
                "requestId": "orphan",
                "response": {"status": 200, "statusText": "OK"}
            }
        })];
        let out = process_network_events(&events, &[]);
        assert!(out.is_empty());
    }
}

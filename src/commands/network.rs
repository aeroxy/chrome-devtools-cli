use anyhow::Result;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::cdp::CdpClient;
use crate::format::{format_structured, OutputFormat};
use crate::result::CommandResult;

/// Which headers `network` shows. They carry credentials (Cookie,
/// Authorization, Set-Cookie), so they are printed only when asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Headers {
    pub request: bool,
    pub response: bool,
}

pub async fn collect_network(
    client: &mut CdpClient,
    session_id: &str,
    duration_ms: u64,
    type_filter: Vec<String>,
    headers: Headers,
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
        client.read_events_for(duration_ms, false).await?;
        client.drain_network_events()
    } else {
        // Live mode, direct/fallback: no persistent buffer. Enable Network on our
        // own session, collect for the window, disable, and return what we read.
        client
            .send_to_target(session_id, "Network.enable", json!({}))
            .await?;
        let events_result = client.read_events_for(duration_ms, true).await;
        let _ = client
            .send_to_target(session_id, "Network.disable", json!({}))
            .await;
        events_result?
    };

    let requests = process_network_events(&events, &type_filter, headers);
    format_network_output(&requests, format)
}

fn process_network_events(
    events: &[serde_json::Value],
    type_filter: &[String],
    headers: Headers,
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

    // Chrome reports the headers that went over the wire (Cookie and
    // Set-Cookie among them, which the page's own view leaves out) in
    // separate events, in hop order, though not always after the hop's other
    // events. A hop answered from the cache or a service worker has none, and
    // Chrome says which hops do: `redirectHasExtraInfo` on the next hop's
    // request, `hasExtraInfo` on the response. So the wire events are lined up
    // with the hops that have them, a hop Chrome says nothing about counts as
    // having them, and the page's view stands in where there are none. It
    // also stands in for a chain whose first request isn't among these events
    // (an earlier drain took it, or the buffer dropped it): wire events of the
    // hops not seen here may still be, and nothing tells them apart.
    let mut wire_request: HashMap<&str, Vec<&serde_json::Value>> = HashMap::new();
    let mut wire_response: HashMap<&str, Vec<&serde_json::Value>> = HashMap::new();
    // Request and response wire headers are announced separately: the final
    // hop's `hasExtraInfo` says only whether its response had wire headers,
    // not its request's. Keeping the two per-hop flag lists apart stops a
    // cache-served response from hiding the request's wire headers.
    let mut hop_has_request_wire: HashMap<&str, Vec<bool>> = HashMap::new();
    let mut hop_has_response_wire: HashMap<&str, Vec<bool>> = HashMap::new();
    let mut begun_earlier: HashSet<&str> = HashSet::new();
    for event in events {
        let params = &event["params"];
        let Some(id) = params["requestId"].as_str() else {
            continue;
        };
        // A redirect hop's wire headers are announced on the next hop's
        // request (`redirectHasExtraInfo`); a response's on its own
        // `responseReceived` (`hasExtraInfo`, omitted when there are none).
        // The first hop's request is unmarked, so assume it has them.
        let flag = |name: &str, default: bool| params[name].as_bool().unwrap_or(default);
        match event["method"].as_str() {
            Some("Network.requestWillBeSentExtraInfo") => {
                wire_request.entry(id).or_default().push(&params["headers"])
            }
            Some("Network.responseReceivedExtraInfo") => wire_response
                .entry(id)
                .or_default()
                .push(&params["headers"]),
            Some("Network.requestWillBeSent") => {
                let requests = hop_has_request_wire.entry(id).or_default();
                let responses = hop_has_response_wire.entry(id).or_default();
                if params.get("redirectResponse").is_some() {
                    match (requests.last_mut(), responses.last_mut()) {
                        (Some(request), Some(response)) => {
                            let has_extra = flag("redirectHasExtraInfo", true);
                            *request = has_extra;
                            *response = has_extra;
                        }
                        // The redirect began before this batch: its headers
                        // may have been dropped by an earlier drain.
                        _ => {
                            begun_earlier.insert(id);
                        }
                    }
                }
                requests.push(true);
                responses.push(true);
            }
            Some("Network.responseReceived") => {
                if let Some(last) = hop_has_response_wire.get_mut(id).and_then(|h| h.last_mut()) {
                    *last = flag("hasExtraInfo", false);
                }
            }
            _ => {}
        }
    }
    // A drain between a hop's ExtraInfo event and its request keeps no
    // unmatched event, so that wire event is lost. The next batch would map
    // the surviving (later hop's) headers onto the earlier hop, and the later
    // hop would fall back to its page view. Trust the wire only where every
    // hop Chrome says has ExtraInfo still has an event; otherwise use the
    // page's view for the whole chain instead of guessing a hop.
    let complete_wire = |wire: &HashMap<&str, Vec<&serde_json::Value>>,
                         hops: &HashMap<&str, Vec<bool>>|
     -> HashSet<String> {
        wire.iter()
            .filter(|(id, events)| {
                let expected = hops
                    .get(*id)
                    .map_or(0, |h| h.iter().filter(|&&w| w).count());
                events.len() >= expected
            })
            .map(|(id, _)| (*id).to_string())
            .collect()
    };
    let request_wire_complete = complete_wire(&wire_request, &hop_has_request_wire);
    let response_wire_complete = complete_wire(&wire_response, &hop_has_response_wire);
    let pick = |wire: &HashMap<&str, Vec<&serde_json::Value>>,
                has_wire_map: &HashMap<&str, Vec<bool>>,
                id: &str,
                hop: usize,
                page_view: &serde_json::Value,
                complete: &HashSet<String>| {
        if begun_earlier.contains(id) || !complete.contains(id) {
            return page_view.clone();
        }
        let has_wire = has_wire_map.get(id).map_or(&[][..], Vec::as_slice);
        let index = (has_wire.get(hop) != Some(&false))
            .then(|| has_wire.iter().take(hop).filter(|&&w| w).count());
        index
            .and_then(|i| wire.get(id)?.get(i))
            .filter(|h| h.is_object())
            .copied()
            .unwrap_or(page_view)
            .clone()
    };
    // How many requests (hops) each id has had so far.
    let mut hops: HashMap<String, usize> = HashMap::new();

    for event in events {
        let method = event["method"].as_str().unwrap_or("");
        let params = &event["params"];
        let request_id = match params["requestId"].as_str() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };

        match method {
            "Network.requestWillBeSent" => {
                let url = params["request"]["url"].as_str().unwrap_or("").to_string();
                let resource_type = params["type"].as_str().unwrap_or("Other").to_string();
                let http_method = params["request"]["method"]
                    .as_str()
                    .unwrap_or("GET")
                    .to_string();

                // A redirect re-fires requestWillBeSent with the same requestId,
                // and Chrome sends NO separate responseReceived for the hop — its
                // 3xx status lives only on `redirectResponse`. Stash the prior hop
                // (with that status) as its own entry so redirect chains stay
                // visible instead of being overwritten by the final request.
                let hop = hops.get(&request_id).copied().unwrap_or(0);
                if requests.contains_key(&request_id) {
                    if let Some(redirect) = params.get("redirectResponse") {
                        if let Some(mut prev) = requests.get(&request_id).cloned() {
                            prev["status"] = json!(redirect["status"].as_u64().unwrap_or(0));
                            prev["statusText"] =
                                json!(redirect["statusText"].as_str().unwrap_or(""));
                            if headers.response {
                                prev["responseHeaders"] = pick(
                                    &wire_response,
                                    &hop_has_response_wire,
                                    &request_id,
                                    hop.saturating_sub(1),
                                    &redirect["headers"],
                                    &response_wire_complete,
                                );
                            }

                            let mut counter = 1;
                            let mut redirect_id = format!("{request_id}#redirect{counter}");
                            while requests.contains_key(&redirect_id) {
                                counter += 1;
                                redirect_id = format!("{request_id}#redirect{counter}");
                            }
                            // Insert the hop just before the final request so the
                            // timeline reads request -> redirect(s) -> final.
                            if let Some(pos) = order.iter().position(|id| id == &request_id) {
                                order.insert(pos, redirect_id.clone());
                            }
                            requests.insert(redirect_id, prev);
                        }
                    }
                } else {
                    order.push(request_id.clone());
                }
                let mut entry = json!({
                    "url": url,
                    "method": http_method,
                    "resourceType": resource_type,
                    "status": null,
                });
                if headers.request {
                    entry["requestHeaders"] = pick(
                        &wire_request,
                        &hop_has_request_wire,
                        &request_id,
                        hop,
                        &params["request"]["headers"],
                        &request_wire_complete,
                    );
                }
                requests.insert(request_id.clone(), entry);
                hops.insert(request_id, hop + 1);
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
                    if headers.response {
                        let hop = hops.get(&request_id).map_or(0, |n| n.saturating_sub(1));
                        req["responseHeaders"] = pick(
                            &wire_response,
                            &hop_has_response_wire,
                            &request_id,
                            hop,
                            &params["response"]["headers"],
                            &response_wire_complete,
                        );
                    }
                }
            }
            "Network.loadingFailed" => {
                if let Some(req) = requests.get_mut(&request_id) {
                    let error = params["errorText"].as_str().unwrap_or("failed").to_string();
                    req["status"] = json!("failed");
                    req["error"] = json!(error);
                    if headers.response {
                        // A failed fetch can still carry wire response headers in a
                        // responseReceivedExtraInfo event with no responseReceived
                        // (e.g. a CORS rejection). Keep any headers already recorded,
                        // since a failure after a response may have set them from the
                        // page's view.
                        let hop = hops.get(&request_id).map_or(0, |n| n.saturating_sub(1));
                        let page_view = req
                            .get("responseHeaders")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        req["responseHeaders"] = pick(
                            &wire_response,
                            &hop_has_response_wire,
                            &request_id,
                            hop,
                            &page_view,
                            &response_wire_complete,
                        );
                    }
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
            // curl -v's markers: > what was sent, < what came back.
            for (marker, key) in [(">", "requestHeaders"), ("<", "responseHeaders")] {
                // Chrome sorts the wire headers by name but not the page's
                // view, so sort both, or the same headers print two ways.
                let mut sorted: Vec<_> = req[key].as_object().into_iter().flatten().collect();
                sorted.sort_by(|a, b| a.0.cmp(b.0));
                for (name, value) in sorted {
                    // Chrome joins a repeated header's values with newlines.
                    for line in value.as_str().unwrap_or_default().split('\n') {
                        writeln!(out, "  {marker} {name}: {line}").unwrap();
                    }
                }
            }
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
        let out = process_network_events(&events, &[], Headers::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["url"], "https://example.com/api");
        assert_eq!(out[0]["method"], "POST");
        assert_eq!(out[0]["resourceType"], "Fetch");
        assert_eq!(out[0]["status"], 201);
        assert_eq!(out[0]["statusText"], "Created");
    }

    /// A login POST answered with a 302 that sets the session cookie, then
    /// the redirected GET. Chrome's wire headers come in their own events,
    /// one per hop, the first even before the request they belong to.
    #[test]
    fn headers_follow_each_hop_and_prefer_the_wire() {
        let event =
            |method: &str, params: serde_json::Value| json!({"method": method, "params": params});
        let events = vec![
            event(
                "Network.requestWillBeSentExtraInfo",
                json!({"requestId": "1", "headers": {"cookie": "pre=1"}}),
            ),
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Document",
                "request": {"url": "https://a.test/login", "method": "POST", "headers": {"content-type": "form"}}}),
            ),
            event(
                "Network.responseReceivedExtraInfo",
                json!({"requestId": "1",
                "headers": {"location": "/home", "set-cookie": "sid=s1; HttpOnly\ntheme=dark"}}),
            ),
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Document",
                "request": {"url": "https://a.test/home", "method": "GET", "headers": {"accept": "html"}},
                "redirectResponse": {"status": 302, "statusText": "Found", "headers": {"location": "/home"}}}),
            ),
            event(
                "Network.requestWillBeSentExtraInfo",
                json!({"requestId": "1", "headers": {"cookie": "pre=1; sid=s1"}}),
            ),
            // No wire event for the last response (from the cache, say).
            event(
                "Network.responseReceived",
                json!({"requestId": "1",
                "response": {"status": 200, "statusText": "OK", "headers": {"content-type": "text/html"}}}),
            ),
        ];
        let both = Headers {
            request: true,
            response: true,
        };
        let out = process_network_events(&events, &[], both);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["status"], 302);
        assert_eq!(out[0]["requestHeaders"], json!({"cookie": "pre=1"}));
        assert_eq!(
            out[0]["responseHeaders"]["set-cookie"],
            "sid=s1; HttpOnly\ntheme=dark"
        );
        assert_eq!(out[1]["requestHeaders"], json!({"cookie": "pre=1; sid=s1"}));
        assert_eq!(
            out[1]["responseHeaders"],
            json!({"content-type": "text/html"})
        );

        let plain = process_network_events(&events, &[], Headers::default());
        assert!(plain
            .iter()
            .all(|r| r.get("requestHeaders").is_none() && r.get("responseHeaders").is_none()));

        let text = format_network_output(&out, OutputFormat::Text)
            .unwrap()
            .output;
        assert!(text.contains("302      POST   https://a.test/login\n  > cookie: pre=1\n  < location: /home\n  < set-cookie: sid=s1; HttpOnly\n  < set-cookie: theme=dark\n"), "{text}");
    }

    /// A redirect answered from the cache sends no wire headers, so the next
    /// hop's must not be taken for its.
    #[test]
    fn wire_headers_skip_hops_without_them() {
        let event =
            |method: &str, params: serde_json::Value| json!({"method": method, "params": params});
        let events = vec![
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Document",
                "request": {"url": "http://a.test/", "method": "GET", "headers": {"page": "0"}}}),
            ),
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Document",
                "request": {"url": "https://a.test/", "method": "GET", "headers": {"page": "1"}},
                "redirectResponse": {"status": 301, "statusText": "Moved", "headers": {"page": "0"}},
                "redirectHasExtraInfo": false}),
            ),
            event(
                "Network.requestWillBeSentExtraInfo",
                json!({"requestId": "1", "headers": {"wire": "1"}}),
            ),
            event(
                "Network.responseReceivedExtraInfo",
                json!({"requestId": "1", "headers": {"wire": "1"}}),
            ),
            event(
                "Network.responseReceived",
                json!({"requestId": "1", "hasExtraInfo": true,
                "response": {"status": 200, "statusText": "OK", "headers": {"page": "1"}}}),
            ),
        ];
        let both = Headers {
            request: true,
            response: true,
        };
        let out = process_network_events(&events, &[], both);
        assert_eq!(out[0]["status"], 301);
        assert_eq!(out[0]["requestHeaders"], json!({"page": "0"}));
        assert_eq!(out[0]["responseHeaders"], json!({"page": "0"}));
        assert_eq!(out[1]["requestHeaders"], json!({"wire": "1"}));
        assert_eq!(out[1]["responseHeaders"], json!({"wire": "1"}));
    }

    /// A drain that starts partway through a redirect chain (an earlier one
    /// took its first request) can still hold the wire events of the hop it
    /// doesn't see, here the 302's, so the hop it does see must not take them.
    #[test]
    fn a_chain_begun_before_the_drain_gets_the_page_view() {
        let event =
            |method: &str, params: serde_json::Value| json!({"method": method, "params": params});
        let events = vec![
            event(
                "Network.responseReceivedExtraInfo",
                json!({"requestId": "1", "headers": {"location": "/b", "set-cookie": "a=1"}}),
            ),
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Fetch",
                "request": {"url": "https://a.test/b", "method": "GET", "headers": {"page": "b"}},
                "redirectResponse": {"status": 302, "statusText": "Found", "headers": {"location": "/b"}},
                "redirectHasExtraInfo": true}),
            ),
            event(
                "Network.requestWillBeSentExtraInfo",
                json!({"requestId": "1", "headers": {"cookie": "a=1"}}),
            ),
            event(
                "Network.responseReceivedExtraInfo",
                json!({"requestId": "1", "headers": {"wire": "b"}}),
            ),
            event(
                "Network.responseReceived",
                json!({"requestId": "1", "hasExtraInfo": true,
                "response": {"status": 200, "statusText": "OK", "headers": {"page": "b"}}}),
            ),
        ];
        let both = Headers {
            request: true,
            response: true,
        };
        let out = process_network_events(&events, &[], both);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["url"], "https://a.test/b");
        assert_eq!(out[0]["status"], 200);
        assert_eq!(out[0]["requestHeaders"], json!({"page": "b"}));
        assert_eq!(out[0]["responseHeaders"], json!({"page": "b"}));
    }

    /// A drain between one hop's ExtraInfo event and its request drops that
    /// event (no unmatched ExtraInfo is retained). The surviving wire event
    /// belongs to the next redirect hop, so neither hop may show it on the
    /// earlier one — the page's view stands in for the whole chain.
    #[test]
    fn a_dropped_wire_event_is_not_assigned_to_an_earlier_hop() {
        let event =
            |method: &str, params: serde_json::Value| json!({"method": method, "params": params});
        let events = vec![
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Document",
                "request": {"url": "https://a.test/a", "method": "GET", "headers": {"page": "a"}}}),
            ),
            // Hop A's wire request headers were taken by an earlier drain.
            event(
                "Network.requestWillBeSentExtraInfo",
                json!({"requestId": "1", "headers": {"cookie": "b=1"}}),
            ),
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Document",
                "request": {"url": "https://a.test/b", "method": "GET", "headers": {"page": "b"}},
                "redirectResponse": {"status": 302, "statusText": "Found", "headers": {"page": "a"}},
                "redirectHasExtraInfo": true}),
            ),
            event(
                "Network.responseReceivedExtraInfo",
                json!({"requestId": "1", "headers": {"wire": "b"}}),
            ),
            event(
                "Network.responseReceived",
                json!({"requestId": "1", "hasExtraInfo": true,
                "response": {"status": 200, "statusText": "OK", "headers": {"page": "b"}}}),
            ),
        ];
        let both = Headers {
            request: true,
            response: true,
        };
        let out = process_network_events(&events, &[], both);
        assert_eq!(out.len(), 2);
        // The earlier hop must not display the later hop's wire headers.
        assert_eq!(out[0]["requestHeaders"], json!({"page": "a"}));
        assert_eq!(out[1]["requestHeaders"], json!({"page": "b"}));
        assert_eq!(out[1]["responseHeaders"], json!({"page": "b"}));
    }

    /// Chrome sorts the wire headers by name but hands over the page's view
    /// in no particular order, so text lists both by name, each value's
    /// lines in the order Chrome gave them.
    #[test]
    fn text_lists_headers_by_name() {
        let req = json!({"url": "https://a.test/", "method": "GET", "status": 200,
            "responseHeaders": {"x-b": "2", "content-type": "text/html", "set-cookie": "b=2\na=1"}});
        let text = format_network_output(&[req], OutputFormat::Text)
            .unwrap()
            .output;
        assert!(
            text.ends_with(
                "  < content-type: text/html\n  < set-cookie: b=2\n  < set-cookie: a=1\n  < x-b: 2\n"
            ),
            "{text}"
        );
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
        let out = process_network_events(&events, &[], Headers::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["status"], "failed");
        assert_eq!(out[0]["error"], "net::ERR_BLOCKED_BY_CLIENT");
    }

    /// A CORS rejection can send responseReceivedExtraInfo without a
    /// responseReceived. The wire headers must still be shown for the failed
    /// request when response headers were asked for.
    #[test]
    fn cors_failure_keeps_wire_response_headers() {
        let event =
            |method: &str, params: serde_json::Value| json!({"method": method, "params": params});
        let events = vec![
            event(
                "Network.requestWillBeSent",
                json!({"requestId": "1", "type": "Fetch",
                "request": {"url": "https://api.test/data", "method": "GET"}}),
            ),
            event(
                "Network.responseReceivedExtraInfo",
                json!({"requestId": "1",
                "headers": {"access-control-allow-origin": "https://app.test", "content-type": "application/json"}}),
            ),
            event(
                "Network.loadingFailed",
                json!({"requestId": "1", "errorText": "net::ERR_FAILED", "blockedReason": "cors"}),
            ),
        ];
        let with_headers = Headers {
            request: false,
            response: true,
        };
        let out = process_network_events(&events, &[], with_headers);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["status"], "failed");
        assert_eq!(out[0]["error"], "net::ERR_FAILED");
        assert_eq!(
            out[0]["responseHeaders"]["access-control-allow-origin"],
            "https://app.test"
        );

        let without = process_network_events(&events, &[], Headers::default());
        assert_eq!(without[0]["status"], "failed");
        assert!(without[0].get("responseHeaders").is_none());
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
        let out = process_network_events(&events, &filter, Headers::default());
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
        let out = process_network_events(&events, &filter, Headers::default());
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
        let out = process_network_events(&events, &[], Headers::default());
        assert_eq!(out[0]["url"], "https://z.example.com/");
        assert_eq!(out[1]["url"], "https://a.example.com/");
    }

    #[test]
    fn test_process_network_events_ignores_unknown_methods() {
        let events = vec![
            json!({"method": "Network.dataReceived", "params": {"requestId": "r1", "dataLength": 100}}),
        ];
        let out = process_network_events(&events, &[], Headers::default());
        assert!(out.is_empty());
    }

    #[test]
    fn test_process_network_events_preserves_redirect_chain() {
        // A redirect re-fires requestWillBeSent with the same requestId and a
        // redirectResponse carrying the 3xx status of the previous hop. The hop
        // must survive as its own entry (with its 301), not be overwritten.
        let events = vec![
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-r",
                    "request": {"url": "https://example.com/old", "method": "GET"},
                    "type": "Document"
                }
            }),
            json!({
                "method": "Network.requestWillBeSent",
                "params": {
                    "requestId": "req-r",
                    "request": {"url": "https://example.com/new", "method": "GET"},
                    "type": "Document",
                    "redirectResponse": {"status": 301, "statusText": "Moved Permanently"}
                }
            }),
            json!({
                "method": "Network.responseReceived",
                "params": {
                    "requestId": "req-r",
                    "response": {"status": 200, "statusText": "OK"}
                }
            }),
        ];
        let out = process_network_events(&events, &[], Headers::default());
        assert_eq!(out.len(), 2);
        // Redirect hop first (chronological), with its real 301 status.
        assert_eq!(out[0]["url"], "https://example.com/old");
        assert_eq!(out[0]["status"], 301);
        assert_eq!(out[0]["statusText"], "Moved Permanently");
        // Final request second, with its resolved 200.
        assert_eq!(out[1]["url"], "https://example.com/new");
        assert_eq!(out[1]["status"], 200);
        // Synthetic redirect keys are internal — they must not leak into output.
        assert!(out[0].get("requestId").is_none());
        assert!(out[1].get("requestId").is_none());
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
        let out = process_network_events(&events, &[], Headers::default());
        assert!(out.is_empty());
    }
}

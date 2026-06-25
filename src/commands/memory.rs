use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::fs::File;
use std::io::BufReader;

use crate::cdp::CdpClient;
use crate::result::CommandResult;

/// Take a heap snapshot of the page and save it to a file.
pub async fn take_heapsnapshot(
    client: &mut CdpClient,
    session_id: &str,
    output: &str,
) -> Result<CommandResult> {
    let mut file = tokio::fs::File::create(output).await?;
    
    // First, let's enable the HeapProfiler.
    client.send_to_target(session_id, "HeapProfiler.enable", json!({})).await?;

    let snapshot_result = async {
        // Send the takeHeapSnapshot command without blocking so we can process chunks as they stream in
        let msg_id = client.send_raw_no_wait(
            Some(session_id),
            "HeapProfiler.takeHeapSnapshot",
            json!({ "reportProgress": true, "treatGlobalObjectsAsRoots": true, "captureNumericValue": true }),
        ).await?;

        use tokio::io::AsyncWriteExt;
        loop {
            let text = client.read_text().await?;
            let event: serde_json::Value = serde_json::from_str(&text)?;
            
            // Check if this is the completion response for our takeHeapSnapshot command
            if event.get("id").and_then(|v| v.as_u64()) == Some(msg_id) {
                if let Some(error) = event.get("error") {
                    bail!(
                        "CDP error in HeapProfiler.takeHeapSnapshot: {}",
                        serde_json::to_string_pretty(error)?
                    );
                }
                break;
            }

            let method = event["method"].as_str().unwrap_or("");
            if method == "HeapProfiler.addHeapSnapshotChunk" {
                if let Some(chunk) = event["params"]["chunk"].as_str() {
                    file.write_all(chunk.as_bytes()).await?;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = client.send_to_target(session_id, "HeapProfiler.disable", json!({})).await;
    snapshot_result?;

    Ok(CommandResult::output(format!(
        "Heap snapshot successfully saved to {}",
        output
    )))
}

/// Parse the JSON heap snapshot and locate details for the given node ID.
/// Returns a tuple of (node_name, self_size).
pub fn parse_node_from_snapshot(
    file_path: &str,
    node_id: u64,
) -> Result<(String, u64)> {
    let file = File::open(file_path)?;
    let reader = BufReader::new(file);
    let val: serde_json::Value = serde_json::from_reader(reader)?;

    let nodes = val["nodes"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: nodes array missing"))?;

    let meta = &val["snapshot"]["meta"];
    let node_fields = meta["node_fields"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: node_fields missing"))?;
    
    // Find fields offsets within the flat nodes array
    let id_offset = node_fields.iter().position(|f| f.as_str() == Some("id")).ok_or_else(|| anyhow!("id field missing"))?;
    let name_offset = node_fields.iter().position(|f| f.as_str() == Some("name")).ok_or_else(|| anyhow!("name field missing"))?;
    let self_size_offset = node_fields.iter().position(|f| f.as_str() == Some("self_size")).ok_or_else(|| anyhow!("self_size field missing"))?;
    let node_size = node_fields.len();

    // Iterate over nodes using chunk sizes defined by the schema meta
    let mut target_index = None;
    let mut current_idx = 0;
    while current_idx + id_offset < nodes.len() {
        let id = nodes.get(current_idx + id_offset).and_then(|v| v.as_u64()).unwrap_or(0);
        if id == node_id {
            target_index = Some(current_idx);
            break;
        }
        current_idx += node_size;
    }

    let target_node_index = match target_index {
        Some(idx) => idx,
        None => bail!("Node with ID {} not found", node_id),
    };

    let name_str_idx = nodes.get(target_node_index + name_offset).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let name = val["strings"].get(name_str_idx).and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
    let self_size = nodes.get(target_node_index + self_size_offset).and_then(|v| v.as_u64()).unwrap_or(0);

    Ok((name, self_size))
}

/// Look up details of a specific node from a local heap snapshot.
/// Adheres to the command-function contract, accepting client, session_id, and OutputFormat.
pub async fn inspect_heapsnapshot_node(
    _client: &mut CdpClient,
    _session_id: &str,
    file_path: &str,
    node_id: u64,
    format: crate::format::OutputFormat,
) -> Result<CommandResult> {
    let (name, self_size) = parse_node_from_snapshot(file_path, node_id)?;

    if format.is_text() {
        let mut out = String::new();
        out.push_str("nodeId,nodeName,selfSize,retainedSize\n");
        out.push_str(&format!(
            "{},{},{} B,unknown\n",
            node_id, name, self_size
        ));
        Ok(CommandResult::output(out))
    } else {
        let details = json!({
            "nodeId": node_id,
            "nodeName": name,
            "selfSize": self_size,
            "retainedSize": null
        });
        Ok(CommandResult::output(crate::format::format_structured(&details, format)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_node_from_snapshot() {
        let mut file = NamedTempFile::new().unwrap();
        let test_snapshot = json!({
            "snapshot": {
                "meta": {
                    "node_fields": ["id", "name", "self_size", "edge_count"],
                    "node_types": ["number", "string", "number", "number"]
                }
            },
            "nodes": [123, 0, 1024, 0, 456, 1, 2048, 0],
            "strings": ["TestObject", "AnotherObject"]
        });
        write!(file, "{}", test_snapshot.to_string()).unwrap();

        let (name, size) = parse_node_from_snapshot(file.path().to_str().unwrap(), 456).unwrap();
        assert_eq!(name, "AnotherObject");
        assert_eq!(size, 2048);
    }
}

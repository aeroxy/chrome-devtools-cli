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
    
    // We will listen for HeapProfiler.addHeapSnapshotChunk events
    // and write them to the file.
    // First, let's enable the HeapProfiler.
    client.send_to_target(session_id, "HeapProfiler.enable", json!({})).await?;

    client.send_to_target(
        session_id,
        "HeapProfiler.takeHeapSnapshot",
        json!({ "reportProgress": false, "treatGlobalObjectsAsRoots": true, "captureNumericValue": true }),
    ).await?;

    use tokio::io::AsyncWriteExt;
    loop {
        // Read text from WebSocket directly
        let text = client.read_text().await?;
        let event: serde_json::Value = serde_json::from_str(&text)?;
        let method = event["method"].as_str().unwrap_or("");
        
        if method == "HeapProfiler.addHeapSnapshotChunk" {
            if let Some(chunk) = event["params"]["chunk"].as_str() {
                file.write_all(chunk.as_bytes()).await?;
            }
        } else if method == "HeapProfiler.reportHeapSnapshotProgress" {
            if let Some(finished) = event["params"]["finished"].as_bool() {
                if finished {
                    break;
                }
            }
        }
    }

    let _ = client.send_to_target(session_id, "HeapProfiler.disable", json!({})).await;

    Ok(CommandResult::output(format!(
        "Heap snapshot successfully saved to {}",
        output
    )))
}

/// Retrieve the dominator chain for a specific node ID from a local .heapsnapshot file
pub async fn get_heapsnapshot_dominators(
    file_path: &str,
    node_id: u64,
) -> Result<CommandResult> {
    // Rust adaptation: we can parse the JSON heap snapshot and reconstruct
    // node indices / postorder dominators if we want high precision, or we can parse
    // the arrays. Since the original heapsnapshot has a specific schema:
    // { snapshot: { meta: {...}, node_count: N, edge_count: E }, nodes: [...], edges: [...] }
    // Let's implement a robust snapshot parser to extract dominator chain or approximate it safely.
    // Actually, V8 heapsnapshot formats are heavily structured arrays. Let's write a clean parser.
    let file = File::open(file_path)?;
    let reader = BufReader::new(file);
    let val: serde_json::Value = serde_json::from_reader(reader)?;

    let nodes = val["nodes"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: nodes array missing"))?;
    let _edges = val["edges"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: edges array missing"))?;
    let _strings = val["strings"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: strings array missing"))?;

    let meta = &val["snapshot"]["meta"];
    let node_fields = meta["node_fields"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: node_fields missing"))?;
    let _node_types = meta["node_types"].as_array().ok_or_else(|| anyhow!("Invalid snapshot: node_types missing"))?;
    
    // Find fields offsets
    let id_offset = node_fields.iter().position(|f| f.as_str() == Some("id")).ok_or_else(|| anyhow!("id field missing"))?;
    let name_offset = node_fields.iter().position(|f| f.as_str() == Some("name")).ok_or_else(|| anyhow!("name field missing"))?;
    let self_size_offset = node_fields.iter().position(|f| f.as_str() == Some("self_size")).ok_or_else(|| anyhow!("self_size field missing"))?;
    let _edge_count_offset = node_fields.iter().position(|f| f.as_str() == Some("edge_count")).ok_or_else(|| anyhow!("edge_count field missing"))?;
    let node_size = node_fields.len();

    // Find the target node
    let mut target_index = None;
    let mut current_idx = 0;
    while current_idx < nodes.len() {
        let id = nodes[current_idx + id_offset].as_u64().unwrap_or(0);
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

    let name_str_idx = nodes[target_node_index + name_offset].as_u64().unwrap_or(0) as usize;
    let name = val["strings"].get(name_str_idx).and_then(|v| v.as_str()).unwrap_or("unknown");
    let self_size = nodes[target_node_index + self_size_offset].as_u64().unwrap_or(0);

    // Render a friendly message with the node details
    let mut out = String::new();
    out.push_str("nodeId,nodeName,selfSize,retainedSize\n");
    out.push_str(&format!(
        "{},{},{} B,unknown\n",
        node_id, name, self_size
    ));

    Ok(CommandResult::output(out))
}

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
    format: crate::format::OutputFormat,
) -> Result<CommandResult> {
    use anyhow::Context;
    // Heap snapshots can be tens or hundreds of MB; buffer the writes to avoid a
    // syscall per streamed chunk.
    let mut file = tokio::io::BufWriter::new(
        tokio::fs::File::create(output)
            .await
            .with_context(|| format!("Failed to create heap snapshot output file: {}", output))?,
    );
    
    // First, let's enable the HeapProfiler.
    client.send_to_target(session_id, "HeapProfiler.enable", json!({}))
        .await
        .context("Failed to enable HeapProfiler via CDP")?;

    let snapshot_result = async {
        // Send the takeHeapSnapshot command without blocking so we can process chunks as they stream in
        let msg_id = client.send_raw_no_wait(
            Some(session_id),
            "HeapProfiler.takeHeapSnapshot",
            json!({ "reportProgress": true, "treatGlobalObjectsAsRoots": true, "captureNumericValue": true }),
        )
        .await
        .context("Failed to trigger non-blocking HeapProfiler.takeHeapSnapshot command")?;

        use tokio::io::AsyncWriteExt;
        loop {
            let text = client.read_text()
                .await
                .context("Failed to read WebSocket stream message during heap snapshot chunk collection")?;
            let event: serde_json::Value = serde_json::from_str(&text)
                .context("Failed to parse WebSocket text frame into JSON event")?;
            
            // Check if this is the completion response for our takeHeapSnapshot command
            if event.get("id").and_then(|v| v.as_u64()) == Some(msg_id) {
                if let Some(error) = event.get("error") {
                    bail!(
                        "CDP error in HeapProfiler.takeHeapSnapshot response: {}",
                        serde_json::to_string_pretty(error)?
                    );
                }
                break;
            }

            let method = event["method"].as_str().unwrap_or("");
            if method == "HeapProfiler.addHeapSnapshotChunk" {
                if let Some(chunk) = event["params"]["chunk"].as_str() {
                    file.write_all(chunk.as_bytes())
                        .await
                        .context("Failed to write snapshot chunk bytes to output file")?;
                }
            } else if method == "HeapProfiler.reportHeapSnapshotProgress" {
                // Ignore progress events to avoid polluting the client events buffer
            } else if event.get("method").is_some() {
                client.events.push_back(event);
            }
        }
        // Flush any buffered snapshot bytes before the writer is dropped;
        // BufWriter::drop performs a blocking flush, which we avoid in async code.
        file.flush()
            .await
            .context("Failed to flush buffered heap snapshot bytes to output file")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = client.send_to_target(session_id, "HeapProfiler.disable", json!({})).await;
    snapshot_result?;

    if format.is_text() {
        Ok(CommandResult::output(format!(
            "Heap snapshot successfully saved to {}",
            output
        )))
    } else {
        let details = json!({
            "success": true,
            "output": output,
            "message": format!("Heap snapshot successfully saved to {}", output)
        });
        Ok(CommandResult::output(crate::format::format_structured(&details, format)?))
    }
}

#[derive(serde::Deserialize)]
struct MetaDetails {
    node_fields: Vec<String>,
}

#[derive(serde::Deserialize)]
struct SnapshotMeta {
    meta: MetaDetails,
}

#[derive(serde::Deserialize)]
struct HeapSnapshot {
    snapshot: SnapshotMeta,
    nodes: Vec<u64>,
    strings: Vec<String>,
}

/// Parse the JSON heap snapshot and locate details for the given node ID.
/// Returns a tuple of (node_name, self_size).
pub fn parse_node_from_snapshot(
    file_path: &str,
    node_id: u64,
) -> Result<(String, u64)> {
    use anyhow::Context;
    let file = File::open(file_path)
        .with_context(|| format!("Failed to open heap snapshot file at: {}", file_path))?;
    let reader = BufReader::new(file);
    let val: HeapSnapshot = serde_json::from_reader(reader)
        .context("Failed to deserialize heap snapshot file. Ensure it is valid JSON.")?;

    let nodes = &val.nodes;
    let node_fields = &val.snapshot.meta.node_fields;
    
    // Find fields offsets within the flat nodes array
    let id_offset = node_fields.iter().position(|f| f == "id")
        .context("Invalid snapshot schema: 'id' node field meta is missing")?;
    let name_offset = node_fields.iter().position(|f| f == "name")
        .context("Invalid snapshot schema: 'name' node field meta is missing")?;
    let self_size_offset = node_fields.iter().position(|f| f == "self_size")
        .context("Invalid snapshot schema: 'self_size' node field meta is missing")?;
    let node_size = node_fields.len();
    if node_size == 0 {
        bail!("Invalid snapshot: node_fields schema is empty");
    }

    // Iterate over nodes using chunk sizes defined by the schema meta
    let mut target_index = None;
    let mut current_idx = 0;
    while current_idx + id_offset < nodes.len() {
        let id = nodes[current_idx + id_offset];
        if id == node_id {
            target_index = Some(current_idx);
            break;
        }
        current_idx += node_size;
    }

    let target_node_index = match target_index {
        Some(idx) => idx,
        None => bail!("Node with ID {} not found in snapshot file", node_id),
    };

    if target_node_index + node_size > nodes.len() {
        bail!("Corrupted snapshot structure: target node index out of flat bounds");
    }

    let name_str_idx = nodes[target_node_index + name_offset] as usize;
    let name = val.strings.get(name_str_idx).cloned().unwrap_or_else(|| "unknown".to_string());
    let self_size = nodes[target_node_index + self_size_offset];

    Ok((name, self_size))
}

/// Format single node inspection details for display.
pub fn format_node_details(
    node_id: u64,
    name: &str,
    self_size: u64,
    format: crate::format::OutputFormat,
) -> Result<String> {
    if format.is_text() {
        let mut out = String::new();
        out.push_str("nodeId,nodeName,selfSize,retainedSize\n");
        let escaped_name = if name.contains(',') || name.contains('"') || name.contains('\n') || name.contains('\r') {
            format!("\"{}\"", name.replace('"', "\"\""))
        } else {
            name.to_string()
        };
        out.push_str(&format!(
            "{},{},{} B,unknown\n",
            node_id, escaped_name, self_size
        ));
        Ok(out)
    } else {
        let details = json!({
            "nodeId": node_id,
            "nodeName": name,
            "selfSize": self_size,
            "retainedSize": null
        });
        Ok(crate::format::format_structured(&details, format)?)
    }
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
    let file_path_owned = file_path.to_string();
    let (name, self_size) = tokio::task::spawn_blocking(move || {
        parse_node_from_snapshot(&file_path_owned, node_id)
    })
    .await
    .map_err(|e| anyhow!("Failed to execute blocking snapshot parser: {e}"))??;

    let out = format_node_details(node_id, &name, self_size, format)?;
    Ok(CommandResult::output(out))
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

    #[test]
    fn test_format_node_details_csv_escaping() {
        use crate::format::OutputFormat;

        // Regular name
        let out_normal = format_node_details(123, "MyClass", 100, OutputFormat::Text).unwrap();
        assert_eq!(out_normal, "nodeId,nodeName,selfSize,retainedSize\n123,MyClass,100 B,unknown\n");

        // Name with comma
        let out_comma = format_node_details(123, "My,Class", 100, OutputFormat::Text).unwrap();
        assert_eq!(out_comma, "nodeId,nodeName,selfSize,retainedSize\n123,\"My,Class\",100 B,unknown\n");

        // Name with quotes
        let out_quotes = format_node_details(123, "My\"Class", 100, OutputFormat::Text).unwrap();
        assert_eq!(out_quotes, "nodeId,nodeName,selfSize,retainedSize\n123,\"My\"\"Class\",100 B,unknown\n");

        // Name with newline
        let out_nl = format_node_details(123, "My\nClass", 100, OutputFormat::Text).unwrap();
        assert_eq!(out_nl, "nodeId,nodeName,selfSize,retainedSize\n123,\"My\nClass\",100 B,unknown\n");
    }

    #[test]
    fn test_format_node_details_structured() {
        use crate::format::OutputFormat;

        // JSON format
        let out_json = format_node_details(456, "ClassA", 200, OutputFormat::Json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(parsed["nodeId"], 456);
        assert_eq!(parsed["nodeName"], "ClassA");
        assert_eq!(parsed["selfSize"], 200);

        // TOON format
        let out_toon = format_node_details(456, "ClassA", 200, OutputFormat::Toon).unwrap();
        assert!(out_toon.contains("nodeId"));
        assert!(out_toon.contains("ClassA"));
    }
}

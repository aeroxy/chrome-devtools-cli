use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::fs::File;
use std::io::BufReader;

use crate::cdp::CdpClient;
use crate::error::{CliError, ErrorCode};
use crate::result::CommandResult;

/// Take a heap snapshot of the page and save it to a file.
pub async fn take_heapsnapshot(
    client: &mut CdpClient,
    session_id: &str,
    output: &str,
    format: crate::format::OutputFormat,
) -> Result<CommandResult> {
    use anyhow::Context;
    // Write to a temp file in the same directory so a failed/partial stream
    // never leaves a corrupt file at the final output path. The temp file is
    // renamed to `output` only after the snapshot completes successfully.
    let output_path = std::path::Path::new(output);
    // Unique temp file (PID-suffixed) in the same directory so concurrent runs
    // can't collide, and rename is atomic (same filesystem).
    let temp_path = output_path.with_file_name(format!(
        ".{}.{}.tmp",
        output_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        std::process::id(),
    ));
    // Drop guard ensures the temp file is removed under all termination paths
    // — including future cancellation (timeout, client disconnect, Ctrl+C) and
    // panics — where the async cleanup below would never run. On the success
    // path the file has been renamed away, so `remove_file` is a harmless no-op.
    struct TempFileGuard {
        path: std::path::PathBuf,
    }
    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    let _guard = TempFileGuard {
        path: temp_path.clone(),
    };
    // Heap snapshots can be tens or hundreds of MB; buffer the writes to avoid a
    // syscall per streamed chunk.
    let mut file = tokio::io::BufWriter::new(
        tokio::fs::File::create(&temp_path).await.with_context(|| {
            format!(
                "Failed to create heap snapshot temp file: {}",
                temp_path.display()
            )
        })?,
    );

    // HeapProfiler must be enabled before takeHeapSnapshot — Chrome rejects
    // the command otherwise.
    client
        .send_to_target(session_id, "HeapProfiler.enable", json!({}))
        .await
        .context("Failed to enable HeapProfiler via CDP")?;

    let snapshot_result = async {
        // Send the takeHeapSnapshot command without blocking so we can process chunks as they stream in
        let msg_id = client.send_raw_no_wait(
            Some(session_id),
            "HeapProfiler.takeHeapSnapshot",
            json!({ "reportProgress": false, "treatGlobalObjectsAsRoots": true, "captureNumericValue": true }),
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
            } else if event.get("method").is_some() {
                // Route through push_event so Network/Runtime events land in
                // network_events/console_events (capped) instead of the generic
                // unbounded buffer, and other events get capped too.
                client.push_event(event);
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

    let _ = client
        .send_to_target(session_id, "HeapProfiler.disable", json!({}))
        .await;

    if let Err(e) = snapshot_result {
        return Err(e);
    }

    // Drop the writer (and its underlying file handle) before the rename: on
    // Windows an open handle blocks the move, and even on Unix releasing it
    // before the atomic rename is the safe, portable ordering.
    drop(file);

    // Atomically move the completed temp file to the final output path.
    tokio::fs::rename(&temp_path, output_path)
        .await
        .with_context(|| format!("Failed to rename temp file to final output: {}", output))?;

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
        Ok(CommandResult::output(crate::format::format_structured(
            &details, format,
        )?))
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
pub fn parse_node_from_snapshot(file_path: &str, node_id: u64) -> Result<(String, u64)> {
    let val = parse_snapshot_file(file_path)?;
    find_node_in_snapshot(&val, node_id)
}

/// Pure schema-validation + node-lookup logic, separated from I/O so it can be
/// unit-tested without writing a temp file.
fn find_node_in_snapshot(val: &HeapSnapshot, node_id: u64) -> Result<(String, u64)> {
    use anyhow::Context;
    let nodes = &val.nodes;
    let node_fields = &val.snapshot.meta.node_fields;

    // Find fields offsets within the flat nodes array
    let id_offset = node_fields
        .iter()
        .position(|f| f == "id")
        .context("Invalid snapshot schema: 'id' node field meta is missing")?;
    let name_offset = node_fields
        .iter()
        .position(|f| f == "name")
        .context("Invalid snapshot schema: 'name' node field meta is missing")?;
    let self_size_offset = node_fields
        .iter()
        .position(|f| f == "self_size")
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

    let name_str_idx = usize::try_from(nodes[target_node_index + name_offset])
        .ok()
        .context("Corrupt snapshot: string index overflow on 32-bit architecture")?;
    let name = val.strings.get(name_str_idx).cloned().ok_or_else(|| {
        anyhow!(
            "Corrupt snapshot: string index {} out of bounds (strings len {})",
            name_str_idx,
            val.strings.len()
        )
    })?;
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
        out.push_str("nodeId,nodeName,selfSize\n");
        let escaped_name = csv_escape(name);
        out.push_str(&format!("{},{},{}\n", node_id, escaped_name, self_size));
        Ok(out)
    } else {
        let details = json!({
            "nodeId": node_id,
            "nodeName": name,
            "selfSize": self_size,
        });
        Ok(crate::format::format_structured(&details, format)?)
    }
}

/// Offline variant that doesn't require a Chrome connection. Used by the CLI's
/// early-intercept path so `inspect-heapsnapshot-node` works without a running
/// browser or daemon.
pub async fn inspect_heapsnapshot_node_offline(
    file_path: &str,
    node_id: u64,
    format: crate::format::OutputFormat,
) -> Result<CommandResult> {
    let file_path_owned = file_path.to_string();
    let (name, self_size) =
        tokio::task::spawn_blocking(move || parse_node_from_snapshot(&file_path_owned, node_id))
            .await
            .map_err(|e| anyhow!("Failed to execute blocking snapshot parser: {e}"))??;

    let out = format_node_details(node_id, &name, self_size, format)?;
    Ok(CommandResult::output(out))
}

/// Per-class aggregate. Tracks id → self_size so the diff can recover exact
/// per-id sizes for added/deleted nodes without re-parsing the file.
struct ClassAggregate {
    nodes: std::collections::HashMap<u64, u64>,
}

impl ClassAggregate {
    fn new() -> Self {
        Self {
            nodes: std::collections::HashMap::new(),
        }
    }
}

/// Walk every node in the snapshot and group by class name (the `name`
/// field's string). Pure (no I/O) so it can be unit-tested alongside
/// `find_node_in_snapshot`.
fn build_class_aggregates(
    val: &HeapSnapshot,
) -> Result<std::collections::HashMap<String, ClassAggregate>> {
    use anyhow::Context;
    let nodes = &val.nodes;
    let node_fields = &val.snapshot.meta.node_fields;
    let id_offset = node_fields
        .iter()
        .position(|f| f == "id")
        .context("Invalid snapshot schema: 'id' node field meta is missing")?;
    let name_offset = node_fields
        .iter()
        .position(|f| f == "name")
        .context("Invalid snapshot schema: 'name' node field meta is missing")?;
    let self_size_offset = node_fields
        .iter()
        .position(|f| f == "self_size")
        .context("Invalid snapshot schema: 'self_size' node field meta is missing")?;
    let node_size = node_fields.len();
    if node_size == 0 {
        bail!("Invalid snapshot: node_fields schema is empty");
    }
    if nodes.len() % node_size != 0 {
        bail!(
            "Corrupt snapshot: nodes array length ({}) is not a multiple of node_size ({}); \
             the file is truncated or malformed",
            nodes.len(),
            node_size
        );
    }

    let mut aggregates: std::collections::HashMap<String, ClassAggregate> =
        std::collections::HashMap::new();
    let mut current_idx = 0;
    while current_idx + node_size <= nodes.len() {
        let id = nodes[current_idx + id_offset];
        let name_str_idx = usize::try_from(nodes[current_idx + name_offset])
            .ok()
            .context("Corrupt snapshot: string index overflow on 32-bit architecture")?;
        let name_ref = val.strings.get(name_str_idx).ok_or_else(|| {
            anyhow!(
                "Corrupt snapshot: string index {} out of bounds (strings len {})",
                name_str_idx,
                val.strings.len()
            )
        })?;
        let self_size = nodes[current_idx + self_size_offset];

        aggregates
            .entry(name_ref.clone())
            .or_insert_with(ClassAggregate::new)
            .nodes
            .insert(id, self_size);

        current_idx += node_size;
    }
    Ok(aggregates)
}

/// One row of the summary diff. Mirrors the MCP `HeapSnapshotClassDiff` shape
/// so output stays familiar to anyone moving between the two tools.
#[derive(Debug, Clone)]
pub struct HeapSnapshotClassDiff {
    pub class_name: String,
    pub added_count: usize,
    pub removed_count: usize,
    pub count_delta: i64,
    pub added_size: u64,
    pub removed_size: u64,
    pub size_delta: i64,
    // Per-id detail. Exposed only via the `--class-index` path.
    // Each tuple is (node_id, self_size) — kept together so sorting and
    // formatting never have to zip/unzip parallel vectors.
    pub added_nodes: Vec<(u64, u64)>,
    pub deleted_nodes: Vec<(u64, u64)>,
}

/// Compute the per-class diff between two snapshots. Returns rows filtered to
/// classes with any change (addedCount > 0 OR removedCount > 0) and sorted by
/// sizeDelta descending — matching DevTools' `#getSortedRawClassDiffs` so the
/// summary list and the `--class-index` detail view share stable indices.
fn diff_snapshots(
    mut base: std::collections::HashMap<String, ClassAggregate>,
    current: std::collections::HashMap<String, ClassAggregate>,
) -> Vec<HeapSnapshotClassDiff> {
    let mut diffs: Vec<HeapSnapshotClassDiff> = Vec::new();

    // 1. Process all classes in `current` (covers retained and added classes).
    // Removing matched classes from `base` as we go means whatever remains in
    // `base` after this loop is exactly the set of classes deleted entirely
    // from `current` — no second `contains_key` pass needed.
    for (name, cur_agg) in current {
        let base_agg = base.remove(&name);

        // Upper-bounds: every current node could be new, every base node
        // could be gone. Avoids reallocation churn on large classes.
        let mut added_nodes: Vec<(u64, u64)> = Vec::with_capacity(cur_agg.nodes.len());
        let base_len = base_agg.as_ref().map(|b| b.nodes.len()).unwrap_or(0);
        let mut deleted_nodes: Vec<(u64, u64)> = Vec::with_capacity(base_len);
        let mut added_size: u64 = 0;
        let mut removed_size: u64 = 0;

        if let Some(b) = &base_agg {
            for (id, size) in &cur_agg.nodes {
                if !b.nodes.contains_key(id) {
                    added_nodes.push((*id, *size));
                    added_size += size;
                }
            }
            for (id, size) in &b.nodes {
                if !cur_agg.nodes.contains_key(id) {
                    deleted_nodes.push((*id, *size));
                    removed_size += size;
                }
            }
        } else {
            for (id, size) in &cur_agg.nodes {
                added_nodes.push((*id, *size));
                added_size += size;
            }
        }

        let added_count = added_nodes.len();
        let removed_count = deleted_nodes.len();
        if added_count > 0 || removed_count > 0 {
            // Sort deterministically by node id so summary/detail indices
            // stay stable across runs.
            if added_count > 1 {
                added_nodes.sort_unstable_by_key(|(id, _)| *id);
            }
            if removed_count > 1 {
                deleted_nodes.sort_unstable_by_key(|(id, _)| *id);
            }

            let count_delta = added_count as i64 - removed_count as i64;
            let size_delta = added_size as i64 - removed_size as i64;

            diffs.push(HeapSnapshotClassDiff {
                class_name: name,
                added_count,
                removed_count,
                count_delta,
                added_size,
                removed_size,
                size_delta,
                added_nodes,
                deleted_nodes,
            });
        }
    }

    // 2. Whatever remains in `base` was never matched in `current` — these
    // are classes deleted entirely.
    for (name, base_agg) in base {
        let mut deleted_nodes: Vec<(u64, u64)> = Vec::with_capacity(base_agg.nodes.len());
        let mut removed_size: u64 = 0;

        for (id, size) in &base_agg.nodes {
            deleted_nodes.push((*id, *size));
            removed_size += size;
        }

        let removed_count = deleted_nodes.len();

        // Sort deterministically by node id.
        if removed_count > 1 {
            deleted_nodes.sort_unstable_by_key(|(id, _)| *id);
        }

        let count_delta = -(removed_count as i64);
        let size_delta = -(removed_size as i64);

        diffs.push(HeapSnapshotClassDiff {
            class_name: name,
            added_count: 0,
            removed_count,
            count_delta,
            added_size: 0,
            removed_size,
            size_delta,
            added_nodes: Vec::new(),
            deleted_nodes,
        });
    }

    diffs.sort_by(|a, b| {
        b.size_delta
            .cmp(&a.size_delta)
            .then_with(|| a.class_name.cmp(&b.class_name))
    });
    diffs
}

/// CSV-escape a class name the same way `format_node_details` escapes node
/// names — names like `(closure)` are safe, but `(string, joined)` would break
/// naive CSV parsing.
fn csv_escape(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        std::borrow::Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Format the summary diff table (one row per changed class).
pub fn format_class_diff_summary(
    diffs: &[HeapSnapshotClassDiff],
    format: crate::format::OutputFormat,
) -> Result<String> {
    if format.is_text() {
        use std::fmt::Write;
        let mut out = String::new();
        out.push_str(
            "idx,className,addedCount,removedCount,countDelta,addedSize,removedSize,sizeDelta\n",
        );
        for (i, d) in diffs.iter().enumerate() {
            let _ = write!(
                out,
                "{},{},{},{},{},{},{},{}\n",
                i,
                csv_escape(&d.class_name),
                d.added_count,
                d.removed_count,
                d.count_delta,
                d.added_size,
                d.removed_size,
                d.size_delta,
            );
        }
        Ok(out)
    } else {
        let rows: Vec<serde_json::Value> = diffs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                json!({
                    "idx": i,
                    "className": d.class_name,
                    "addedCount": d.added_count,
                    "removedCount": d.removed_count,
                    "countDelta": d.count_delta,
                    "addedSize": d.added_size,
                    "removedSize": d.removed_size,
                    "sizeDelta": d.size_delta,
                })
            })
            .collect();
        Ok(crate::format::format_structured(
            &json!({ "diffs": rows }),
            format,
        )?)
    }
}

/// Format the per-class detail (added/deleted node IDs + sizes). Mirrors the
/// summary's `idx` so a user can copy the index straight from summary → detail.
pub fn format_class_diff_detail(
    idx: usize,
    diff: &HeapSnapshotClassDiff,
    format: crate::format::OutputFormat,
) -> Result<String> {
    if format.is_text() {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = write!(
            out,
            "idx:{},className:{},addedCount:{},removedCount:{},countDelta:{},addedSize:{},removedSize:{},sizeDelta:{}\n",
            idx,
            csv_escape(&diff.class_name),
            diff.added_count,
            diff.removed_count,
            diff.count_delta,
            diff.added_size,
            diff.removed_size,
            diff.size_delta,
        );
        out.push_str("\nop,nodeId,selfSize\n");
        for (id, size) in &diff.added_nodes {
            let _ = write!(out, "+,{},{}\n", id, size);
        }
        for (id, size) in &diff.deleted_nodes {
            let _ = write!(out, "-,{},{}\n", id, size);
        }
        Ok(out)
    } else {
        let added: Vec<serde_json::Value> = diff
            .added_nodes
            .iter()
            .map(|(id, size)| json!({ "op": "+", "nodeId": id, "selfSize": size }))
            .collect();
        let deleted: Vec<serde_json::Value> = diff
            .deleted_nodes
            .iter()
            .map(|(id, size)| json!({ "op": "-", "nodeId": id, "selfSize": size }))
            .collect();
        let mut nodes: Vec<serde_json::Value> = added;
        nodes.extend(deleted);
        let detail = json!({
            "idx": idx,
            "className": diff.class_name,
            "addedCount": diff.added_count,
            "removedCount": diff.removed_count,
            "countDelta": diff.count_delta,
            "addedSize": diff.added_size,
            "removedSize": diff.removed_size,
            "sizeDelta": diff.size_delta,
            "nodes": nodes,
        });
        Ok(crate::format::format_structured(&detail, format)?)
    }
}

/// Offline implementation of `compare-heapsnapshots`. Parses both files,
/// diffs, and renders summary or per-class detail depending on `class_index`.
pub async fn compare_heapsnapshots_offline(
    base_path: &str,
    current_path: &str,
    class_index: Option<usize>,
    format: crate::format::OutputFormat,
) -> Result<CommandResult> {
    let base_owned = base_path.to_string();
    let current_owned = current_path.to_string();
    let diffs = tokio::task::spawn_blocking(move || -> Result<Vec<HeapSnapshotClassDiff>> {
        // Each raw HeapSnapshot (nodes + strings) can be very large; scope the
        // parse so it's dropped as soon as its aggregate is built instead of
        // holding both raw snapshots in memory for the duration of the diff.
        let base_agg = {
            let base_val = parse_snapshot_file(&base_owned)?;
            build_class_aggregates(&base_val)?
        };
        let current_agg = {
            let current_val = parse_snapshot_file(&current_owned)?;
            build_class_aggregates(&current_val)?
        };
        Ok(diff_snapshots(base_agg, current_agg))
    })
    .await
    .map_err(|e| anyhow!("Failed to execute blocking snapshot diff: {e}"))??;

    let out = match class_index {
        None => format_class_diff_summary(&diffs, format)?,
        Some(idx) => {
            let diff = diffs.get(idx).ok_or_else(|| {
                CliError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "Invalid classIndex: {}. Total classes with changes: {}",
                        idx,
                        diffs.len()
                    ),
                )
            })?;
            format_class_diff_detail(idx, diff, format)?
        }
    };
    Ok(CommandResult::output(out))
}

/// Read + deserialize a .heapsnapshot file. Shared by the diff path so both
/// base and current snapshots parse identically.
fn parse_snapshot_file(file_path: &str) -> Result<HeapSnapshot> {
    use anyhow::Context;
    let file = File::open(file_path)
        .with_context(|| format!("Failed to open heap snapshot file at: {}", file_path))?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader)
        .context("Failed to deserialize heap snapshot file. Ensure it is valid JSON.")
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
    fn test_find_node_in_snapshot_directly() {
        // Exercise the pure helper without going through file I/O.
        let snapshot = HeapSnapshot {
            snapshot: SnapshotMeta {
                meta: MetaDetails {
                    node_fields: vec!["id".into(), "name".into(), "self_size".into()],
                },
            },
            nodes: vec![10, 0, 100, 20, 1, 200],
            strings: vec!["Alpha".into(), "Beta".into()],
        };

        let (name, size) = find_node_in_snapshot(&snapshot, 20).unwrap();
        assert_eq!(name, "Beta");
        assert_eq!(size, 200);
    }

    #[test]
    fn test_find_node_not_found() {
        let snapshot = HeapSnapshot {
            snapshot: SnapshotMeta {
                meta: MetaDetails {
                    node_fields: vec!["id".into(), "name".into(), "self_size".into()],
                },
            },
            nodes: vec![10, 0, 100],
            strings: vec!["Alpha".into()],
        };

        assert!(find_node_in_snapshot(&snapshot, 999).is_err());
    }

    #[test]
    fn test_find_node_corrupt_string_index() {
        // string index 5 is out of bounds (only 1 string exists)
        let snapshot = HeapSnapshot {
            snapshot: SnapshotMeta {
                meta: MetaDetails {
                    node_fields: vec!["id".into(), "name".into(), "self_size".into()],
                },
            },
            nodes: vec![10, 5, 100],
            strings: vec!["Alpha".into()],
        };

        let err = find_node_in_snapshot(&snapshot, 10).unwrap_err();
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn test_format_node_details_csv_escaping() {
        use crate::format::OutputFormat;

        // Regular name
        let out_normal = format_node_details(123, "MyClass", 100, OutputFormat::Text).unwrap();
        assert_eq!(out_normal, "nodeId,nodeName,selfSize\n123,MyClass,100\n");

        // Name with comma
        let out_comma = format_node_details(123, "My,Class", 100, OutputFormat::Text).unwrap();
        assert_eq!(
            out_comma,
            "nodeId,nodeName,selfSize\n123,\"My,Class\",100\n"
        );

        // Name with quotes
        let out_quotes = format_node_details(123, "My\"Class", 100, OutputFormat::Text).unwrap();
        assert_eq!(
            out_quotes,
            "nodeId,nodeName,selfSize\n123,\"My\"\"Class\",100\n"
        );

        // Name with newline
        let out_nl = format_node_details(123, "My\nClass", 100, OutputFormat::Text).unwrap();
        assert_eq!(out_nl, "nodeId,nodeName,selfSize\n123,\"My\nClass\",100\n");
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

    /// Build a synthetic HeapSnapshot from a list of (id, name, self_size)
    /// triples. Keeps diff tests readable — node_fields order is fixed at the
    /// schema the production parser actually sees.
    fn make_snapshot(nodes: &[(u64, &str, u64)]) -> HeapSnapshot {
        let mut flat: Vec<u64> = Vec::with_capacity(nodes.len() * 3);
        let mut strings: Vec<String> = Vec::new();
        let mut string_idx: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        for (id, name, size) in nodes {
            let &mut idx = string_idx.entry(name).or_insert_with(|| {
                let i = strings.len() as u64;
                strings.push(name.to_string());
                i
            });
            flat.extend_from_slice(&[*id, idx, *size]);
        }
        HeapSnapshot {
            snapshot: SnapshotMeta {
                meta: MetaDetails {
                    node_fields: vec!["id".into(), "name".into(), "self_size".into()],
                },
            },
            nodes: flat,
            strings,
        }
    }

    #[test]
    fn test_build_class_aggregates_groups_by_name() {
        let snap = make_snapshot(&[(1, "Map", 100), (2, "Map", 200), (3, "String", 50)]);
        let aggs = build_class_aggregates(&snap).unwrap();
        let map = aggs.get("Map").unwrap();
        assert_eq!(map.nodes.len(), 2);
        assert_eq!(map.nodes.get(&1), Some(&100));
        assert_eq!(map.nodes.get(&2), Some(&200));
        let s = aggs.get("String").unwrap();
        assert_eq!(s.nodes.get(&3), Some(&50));
    }

    #[test]
    fn test_build_class_aggregates_rejects_truncated_nodes() {
        // node_fields describes 3-field records, but nodes has 4 entries — a
        // truncated/malformed flat array that isn't a multiple of node_size.
        // Must error instead of silently dropping the trailing partial record.
        let snap = HeapSnapshot {
            snapshot: SnapshotMeta {
                meta: MetaDetails {
                    node_fields: vec!["id".into(), "name".into(), "self_size".into()],
                },
            },
            nodes: vec![1, 0, 100, 2],
            strings: vec!["Map".into()],
        };
        let err = match build_class_aggregates(&snap) {
            Ok(_) => panic!("expected error for truncated nodes array"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("not a multiple of node_size"),
            "got: {err}"
        );
    }

    #[test]
    fn test_diff_added_removed_and_retained() {
        // Base: Map{1@100, 2@100}, String{3@50}
        // Current: Map{1@100 (retained), 4@150 (new)}, Window{5@500 (new class)}
        let base = build_class_aggregates(&make_snapshot(&[
            (1, "Map", 100),
            (2, "Map", 100),
            (3, "String", 50),
        ]))
        .unwrap();
        let current = build_class_aggregates(&make_snapshot(&[
            (1, "Map", 100),
            (4, "Map", 150),
            (5, "Window", 500),
        ]))
        .unwrap();

        let diffs = diff_snapshots(base, current);

        // Sorted by sizeDelta desc: Window(500) > Map(50) > String(-50)
        assert_eq!(diffs.len(), 3);
        assert_eq!(diffs[0].class_name, "Window");
        assert_eq!(diffs[0].added_count, 1);
        assert_eq!(diffs[0].removed_count, 0);
        assert_eq!(diffs[0].added_size, 500);
        assert_eq!(diffs[0].removed_size, 0);
        assert_eq!(diffs[0].size_delta, 500);
        assert_eq!(diffs[0].added_nodes, vec![(5, 500)]);

        assert_eq!(diffs[1].class_name, "Map");
        assert_eq!(diffs[1].added_count, 1);
        assert_eq!(diffs[1].removed_count, 1);
        assert_eq!(diffs[1].count_delta, 0);
        assert_eq!(diffs[1].added_size, 150);
        assert_eq!(diffs[1].removed_size, 100);
        assert_eq!(diffs[1].size_delta, 50);
        assert_eq!(diffs[1].added_nodes, vec![(4, 150)]);
        assert_eq!(diffs[1].deleted_nodes, vec![(2, 100)]);

        assert_eq!(diffs[2].class_name, "String");
        assert_eq!(diffs[2].added_count, 0);
        assert_eq!(diffs[2].removed_count, 1);
        assert_eq!(diffs[2].count_delta, -1);
        assert_eq!(diffs[2].size_delta, -50);
        assert_eq!(diffs[2].deleted_nodes, vec![(3, 50)]);
    }

    #[test]
    fn test_diff_filters_unchanged_classes() {
        // Map appears in both with identical nodes → no diff row at all.
        let base = build_class_aggregates(&make_snapshot(&[(1, "Map", 100)])).unwrap();
        let current = build_class_aggregates(&make_snapshot(&[(1, "Map", 100)])).unwrap();
        let diffs = diff_snapshots(base, current);
        assert!(diffs.is_empty());
    }

    #[test]
    fn test_diff_class_gone_entirely_from_current() {
        let base =
            build_class_aggregates(&make_snapshot(&[(1, "Old", 80), (2, "Old", 40)])).unwrap();
        let current = build_class_aggregates(&make_snapshot(&[])).unwrap();
        let diffs = diff_snapshots(base, current);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].class_name, "Old");
        assert_eq!(diffs[0].removed_count, 2);
        assert_eq!(diffs[0].removed_size, 120);
        assert_eq!(diffs[0].size_delta, -120);
        assert_eq!(diffs[0].deleted_nodes.len(), 2);
    }

    #[test]
    fn test_format_summary_and_detail_share_indices() {
        let base =
            build_class_aggregates(&make_snapshot(&[(1, "Map", 100), (2, "String", 50)])).unwrap();
        let current =
            build_class_aggregates(&make_snapshot(&[(3, "Window", 500), (4, "Map", 150)])).unwrap();
        let diffs = diff_snapshots(base, current);

        let summary = format_class_diff_summary(&diffs, crate::format::OutputFormat::Text).unwrap();
        // First data row (idx 0) should be the largest sizeDelta — Window.
        assert!(summary.starts_with(
            "idx,className,addedCount,removedCount,countDelta,addedSize,removedSize,sizeDelta\n"
        ));
        assert!(summary.contains("0,Window,1,0,1,500,0,500\n"));

        // Detail for idx 0 must reference the same class.
        let detail =
            format_class_diff_detail(0, &diffs[0], crate::format::OutputFormat::Text).unwrap();
        assert!(detail.contains("className:Window"));
        assert!(detail.contains("+,3,500"));
        // Map's id 4 should NOT appear here — it's a different class.
        assert!(!detail.contains("4,150"));
    }

    #[test]
    fn test_compare_offline_end_to_end_via_files() {
        use crate::format::OutputFormat;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_json = json!({
            "snapshot": { "meta": { "node_fields": ["id","name","self_size"] } },
            "nodes": [1, 0, 100, 2, 1, 50],
            "strings": ["Map", "String"],
        });
        let cur_json = json!({
            "snapshot": { "meta": { "node_fields": ["id","name","self_size"] } },
            "nodes": [3, 0, 500, 4, 1, 150],
            "strings": ["Window", "Map"],
        });

        let mut base_file = NamedTempFile::new().unwrap();
        write!(base_file, "{}", base_json).unwrap();
        let mut cur_file = NamedTempFile::new().unwrap();
        write!(cur_file, "{}", cur_json).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let summary = rt
            .block_on(compare_heapsnapshots_offline(
                base_file.path().to_str().unwrap(),
                cur_file.path().to_str().unwrap(),
                None,
                OutputFormat::Text,
            ))
            .unwrap();
        let summary_out = summary.output;
        assert!(summary_out.contains("0,Window"));
        assert!(summary_out.contains("1,Map"));

        // Detail for idx 0 should print Window's node 3.
        let detail = rt
            .block_on(compare_heapsnapshots_offline(
                base_file.path().to_str().unwrap(),
                cur_file.path().to_str().unwrap(),
                Some(0),
                OutputFormat::Text,
            ))
            .unwrap();
        assert!(detail.output.contains("className:Window"));
        assert!(detail.output.contains("+,3,500"));

        // Out-of-range index should error with a clear message.
        let err = rt.block_on(compare_heapsnapshots_offline(
            base_file.path().to_str().unwrap(),
            cur_file.path().to_str().unwrap(),
            Some(99),
            OutputFormat::Text,
        ));
        let err = match err {
            Ok(_) => panic!("expected error for out-of-range class_index"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("Invalid classIndex"), "got: {msg}");
        assert!(msg.contains("99"));
        // Must surface as a typed InvalidInput error so callers (e.g. main's
        // exit-code mapping) get a stable, non-Unspecified error code.
        let cli_err = err
            .downcast_ref::<crate::error::CliError>()
            .expect("expected a CliError");
        assert_eq!(cli_err.code(), crate::error::ErrorCode::InvalidInput);
    }
}

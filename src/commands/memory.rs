use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;

use crate::cdp::CdpClient;
use crate::error::{CliError, ErrorCode};
use crate::format::OutputFormat;
use crate::result::CommandResult;

/// Take a heap snapshot of the page and save it to a file.
pub async fn take_heapsnapshot(
    client: &mut CdpClient,
    session_id: &str,
    output: &str,
    format: crate::format::OutputFormat,
) -> Result<CommandResult> {
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
    // the command otherwise. It stays enabled afterwards: turning it off makes
    // V8 forget the ids it gave objects, so the next snapshot would number
    // every object afresh and a diff would pair unrelated objects. Every
    // session on the page turns it off as it detaches, so the ids last at
    // most while the daemon's session stays on this tab.
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

    snapshot_result?;

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

// ---------------------------------------------------------------------------
// Offline analysis of .heapsnapshot files
//
// Class grouping, DOM state and shallow sizes are ported from devtools-frontend's
// HeapSnapshot.ts (`calculateObjectNames`, `propagateDOMState`,
// `calculateShallowSizes`, `inferInterfaceDefinitions` /
// `applyInterfaceDefinitions`, `buildAggregates`, `calculateDiffForClass`), so
// the rows, counts and sizes match what the DevTools Memory panel reports for
// the same files.
// ---------------------------------------------------------------------------

/// DevTools' `DOMLinkState`: the values V8 writes to the `detachedness` field.
const DOM_UNKNOWN: u8 = 0;
const DOM_ATTACHED: u8 = 1;
const DOM_DETACHED: u8 = 2;

/// "No edge" in per-node edge arrays and "not reached" in distances.
const NONE: u32 = u32::MAX;

/// Marks a self size of `u32::MAX` or more in a 32-bit size field; the exact
/// size is in a side table. V8 writes sizes as `size_t`, so an ArrayBuffer
/// backing store can be 4 GiB or more, and moving owned sizes onto their
/// owner can add up past 32 bits too.
const WIDE: u32 = u32::MAX;

/// `size` as stored in a 32-bit size field, or `None` when it needs `WIDE`.
fn narrow(size: u64) -> Option<u32> {
    u32::try_from(size).ok().filter(|&s| s != WIDE)
}

/// Paging and name truncation for the offline heap commands. Their output
/// otherwise grows with the heap: hundreds of classes, every changed object,
/// and `string` nodes whose name is the whole string, sometimes megabytes.
#[derive(clap::Args, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListOptions {
    /// Maximum number of rows to print (classes, objects or retainers)
    #[arg(long, default_value_t = 50, value_parser = parse_positive)]
    pub limit: usize,
    /// Number of rows to skip, to page past the first --limit rows
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Cut class, node and edge names to this many characters, ending a cut name with "…"
    #[arg(long, default_value_t = 100, value_parser = parse_positive)]
    pub max_name_length: usize,
}

fn parse_positive(s: &str) -> std::result::Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1".into()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct MetaDetails {
    node_fields: Vec<String>,
    #[serde(default)]
    node_types: Vec<serde_json::Value>,
    #[serde(default)]
    edge_fields: Vec<String>,
    #[serde(default)]
    edge_types: Vec<serde_json::Value>,
    #[serde(default)]
    location_fields: Vec<String>,
}

#[derive(serde::Deserialize)]
struct SnapshotMeta {
    meta: MetaDetails,
    /// V8 writes both record counts in the header. Older or trimmed files
    /// omit them, so the counts are checked only when present.
    #[serde(default)]
    node_count: Option<usize>,
    #[serde(default)]
    edge_count: Option<usize>,
    /// Node-array index of the synthetic root. V8 leaves it out and DevTools
    /// then uses 0.
    #[serde(default)]
    root_index: usize,
}

/// A `.heapsnapshot` file as stored. The flat arrays are `u32`, as in
/// DevTools' `Uint32Array`s, which halves peak memory on multi-hundred-MB
/// files compared with `u64`.
#[derive(serde::Deserialize)]
struct RawSnapshot {
    snapshot: SnapshotMeta,
    nodes: NodeArray,
    #[serde(default)]
    edges: IntArray,
    #[serde(default)]
    locations: IntArray,
    strings: Vec<String>,
}

/// The `edges` or `locations` array. V8 holds these fields in C++ `int`s and
/// prints them signed, so a value it means as 2^31 or more comes out
/// negative: a sparse array's element index past `i32::MAX`. It also writes
/// -1 for a line or column it could not work out. Each negative value is
/// stored as the `u32` it wraps to, the number V8 meant and what DevTools'
/// `Uint32Array` holds, and listed in `negative`, so that each use decides
/// whether a negative value is valid there.
#[derive(Default)]
struct IntArray {
    fields: Vec<u32>,
    /// Indices into `fields` of the values the file wrote as negative.
    negative: Vec<usize>,
}

impl<'de> serde::Deserialize<'de> for IntArray {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = IntArray;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of 32-bit integers")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<IntArray, A::Error> {
                let mut array = IntArray::default();
                while seq.next_element_seed(Push(&mut array))?.is_some() {}
                Ok(array)
            }
        }
        d.deserialize_seq(Fields)
    }
}

/// Appends one value to an `IntArray`, taking the number as the parser hands
/// it over. Reading each value as an `i64` element and converting it was
/// measurably slower on arrays of millions of values.
struct Push<'a>(&'a mut IntArray);

impl<'de> serde::de::DeserializeSeed<'de> for Push<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_i64(self)
    }
}

impl<'de> serde::de::Visitor<'de> for Push<'_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an integer from -2147483648 to 4294967295")
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<(), E> {
        let field = u32::try_from(v)
            .map_err(|_| E::invalid_value(serde::de::Unexpected::Unsigned(v), &self))?;
        self.0.fields.push(field);
        Ok(())
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<(), E> {
        if v >= 0 {
            return self.visit_u64(v as u64);
        }
        let field = i32::try_from(v)
            .map_err(|_| E::invalid_value(serde::de::Unexpected::Signed(v), &self))?;
        self.0.negative.push(self.0.fields.len());
        self.0.fields.push(field as u32);
        Ok(())
    }
}

/// The `nodes` array, read as `u32`s. A value of `WIDE` or more, which V8
/// writes only for `self_size`, is stored as `WIDE` and listed in `wide` with
/// its array index, instead of widening every field of every node for the
/// rare node that needs it. DevTools reads it into a `Uint32Array`, which
/// wraps it; this keeps it exact.
#[derive(Default)]
struct NodeArray {
    fields: Vec<u32>,
    wide: Vec<(usize, u64)>,
}

impl<'de> serde::Deserialize<'de> for NodeArray {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = NodeArray;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of unsigned integers")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<NodeArray, A::Error> {
                let mut nodes = NodeArray::default();
                while let Some(value) = seq.next_element::<u64>()? {
                    let field = narrow(value).unwrap_or_else(|| {
                        nodes.wide.push((nodes.fields.len(), value));
                        WIDE
                    });
                    nodes.fields.push(field);
                }
                Ok(nodes)
            }
        }
        d.deserialize_seq(Fields)
    }
}

/// Indices into the `node_types` enum, looked up by name because V8 does not
/// promise an order. `None` when the snapshot's enum lacks the type.
#[derive(Default)]
struct NodeTypes {
    hidden: Option<u32>,
    array: Option<u32>,
    object: Option<u32>,
    native: Option<u32>,
    synthetic: Option<u32>,
}

#[derive(Default)]
struct EdgeTypes {
    element: Option<u32>,
    hidden: Option<u32>,
    weak: Option<u32>,
    property: Option<u32>,
    internal: Option<u32>,
}

type Location = (u32, u32, u32);

/// A parsed snapshot plus the per-node indexes every offline command needs.
/// All indices into `strings` and `nodes` are validated in `Snap::new`, so
/// the accessors below index directly.
struct Snap {
    nodes: Vec<u32>,
    edges: Vec<u32>,
    strings: Vec<String>,
    node_size: usize,
    id_off: usize,
    name_off: usize,
    size_off: usize,
    type_off: Option<usize>,
    node_types: Vec<String>,
    nt: NodeTypes,
    edge_size: usize,
    edge_type_off: usize,
    edge_name_off: usize,
    edge_to_off: usize,
    edge_types: Vec<String>,
    et: EdgeTypes,
    node_count: usize,
    /// Ordinal of the synthetic root.
    root: usize,
    /// `first_edge[n]..first_edge[n + 1]` is node n's slice of `edges`. V8
    /// stores edges grouped by owner in node order, so these are prefix sums
    /// of `edge_count`.
    first_edge: Vec<u32>,
    /// Exact self size of each node, by ordinal, whose size field is `WIDE`.
    wide_sizes: HashMap<usize, u64>,
    /// Source location per node-array index, for `object` class keys.
    locations: HashMap<usize, Location>,
    /// DOM link state per node, after DevTools' propagation.
    dom: Vec<u8>,
    /// Whether the file has a `detachedness` field. Only then does DevTools
    /// propagate DOM state and prefix "Detached " to native node names.
    has_detachedness: bool,
}

fn enum_names(types: &[serde_json::Value], offset: Option<usize>) -> Vec<String> {
    offset
        .and_then(|o| types.get(o))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|t| t.as_str().unwrap_or("unknown").to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn position(fields: &[String], name: &str) -> Option<usize> {
    fields.iter().position(|f| f == name)
}

/// Index of `name` in a snapshot enum such as `node_types`.
fn type_index(types: &[String], name: &str) -> Option<u32> {
    position(types, name).map(|i| i as u32)
}

/// The next edge-array offset after a node's `edge_count`, checked before it
/// can leave the array or wrap around. Kept apart from the array access so
/// the arithmetic can be tested without a snapshot large enough to overflow.
fn edge_prefix(total: usize, count: u32, edge_size: usize, limit: usize) -> Result<usize> {
    (count as usize)
        .checked_mul(edge_size)
        .and_then(|fields| total.checked_add(fields))
        .filter(|&next| next <= limit)
        .with_context(|| {
            format!(
                "Corrupt snapshot: the nodes' edge_count adds up to more than the \
                 {limit} fields of the edges array; the file is truncated or malformed"
            )
        })
}

impl Snap {
    fn new(raw: RawSnapshot) -> Result<Self> {
        let RawSnapshot {
            snapshot,
            nodes,
            edges,
            locations,
            strings,
        } = raw;
        let NodeArray {
            fields: nodes,
            wide,
        } = nodes;
        let IntArray {
            fields: edges,
            negative: negative_edges,
        } = edges;
        let declared_nodes = snapshot.node_count;
        let declared_edges = snapshot.edge_count;
        let meta = snapshot.meta;
        let required = |fields: &[String], name: &str, kind: &str| {
            position(fields, name).with_context(|| {
                format!("Invalid snapshot schema: '{name}' {kind} field meta is missing")
            })
        };
        let nf = &meta.node_fields;
        let id_off = required(nf, "id", "node")?;
        let name_off = required(nf, "name", "node")?;
        let size_off = required(nf, "self_size", "node")?;
        let node_size = nf.len();
        if !nodes.len().is_multiple_of(node_size) {
            bail!(
                "Corrupt snapshot: nodes array length ({}) is not a multiple of node_size ({}); \
                 the file is truncated or malformed",
                nodes.len(),
                node_size
            );
        }
        let node_count = nodes.len() / node_size;
        if let Some(declared) = declared_nodes {
            if declared != node_count {
                bail!(
                    "Corrupt snapshot: header declares {declared} nodes but the nodes array holds \
                     {node_count}"
                );
            }
        }
        // Ordinals and edge indices are stored as u32 with u32::MAX and
        // u32::MAX - 1 reserved as markers, the same limits DevTools has.
        if node_count >= (u32::MAX - 1) as usize || edges.len() >= u32::MAX as usize {
            bail!(
                "Snapshot too large: {node_count} nodes, {} edge fields",
                edges.len()
            );
        }
        let mut wide_sizes = HashMap::new();
        for (i, value) in wide {
            if i % node_size == size_off {
                wide_sizes.insert(i / node_size, value);
            } else if value > u64::from(WIDE) {
                bail!(
                    "Corrupt snapshot: node field '{}' holds {value}, which does not fit in \
                     32 bits",
                    nf[i % node_size]
                );
            }
        }
        let node_types = enum_names(&meta.node_types, position(nf, "type"));
        // A type field without its enum names no type, so such a file groups
        // by name, as one without the field does.
        let type_off = position(nf, "type").filter(|_| !node_types.is_empty());
        let edge_count_off = position(nf, "edge_count");
        let det_off = position(nf, "detachedness");
        let nt = NodeTypes {
            hidden: type_index(&node_types, "hidden"),
            array: type_index(&node_types, "array"),
            object: type_index(&node_types, "object"),
            native: type_index(&node_types, "native"),
            synthetic: type_index(&node_types, "synthetic"),
        };

        for ord in 0..node_count {
            let idx = nodes[ord * node_size + name_off] as usize;
            if idx >= strings.len() {
                bail!(
                    "Corrupt snapshot: string index {} out of bounds (strings len {})",
                    idx,
                    strings.len()
                );
            }
        }

        let ef = &meta.edge_fields;
        let edge_size = ef.len().max(1);
        let edge_types = enum_names(&meta.edge_types, position(ef, "type"));
        let et = EdgeTypes {
            element: type_index(&edge_types, "element"),
            hidden: type_index(&edge_types, "hidden"),
            weak: type_index(&edge_types, "weak"),
            property: type_index(&edge_types, "property"),
            internal: type_index(&edge_types, "internal"),
        };

        let mut first_edge: Vec<u32> = Vec::with_capacity(node_count + 1);
        let mut total = 0usize;
        let mut seen_ids: HashSet<u32> = HashSet::with_capacity(node_count);
        for ord in 0..node_count {
            // Two records with one ID would be counted in a comparison but
            // inspected as the same object, with only the last one's size.
            let id = nodes[ord * node_size + id_off];
            if !seen_ids.insert(id) {
                bail!("Corrupt snapshot: duplicate node ID {id} at node {ord}");
            }
            // Never past `edges.len()`, which fits in a u32.
            first_edge.push(total as u32);
            if let Some(off) = edge_count_off {
                total = edge_prefix(total, nodes[ord * node_size + off], edge_size, edges.len())?;
            }
        }
        if total != edges.len() {
            bail!(
                "Corrupt snapshot: the nodes' edge_count adds up to {} edge fields but the \
                 edges array has {}; the file is truncated or malformed",
                total,
                edges.len()
            );
        }
        if let Some(declared) = declared_edges {
            let records = total / edge_size;
            if declared != records {
                bail!(
                    "Corrupt snapshot: header declares {declared} edges but the edges array holds \
                     {records}"
                );
            }
        }
        first_edge.push(total as u32);

        let (edge_type_off, edge_name_off, edge_to_off) = if edges.is_empty() {
            (0, 0, 0)
        } else {
            (
                required(ef, "type", "edge")?,
                required(ef, "name_or_index", "edge")?,
                required(ef, "to_node", "edge")?,
            )
        };
        // Only an index means a number past `i32::MAX`. A negative type, node
        // index or string index is no value V8 writes.
        for &i in &negative_edges {
            let (e, field) = (i - i % edge_size, i % edge_size);
            let ty = Some(edges[e + edge_type_off]);
            if field != edge_name_off || (ty != et.element && ty != et.hidden) {
                bail!(
                    "Corrupt snapshot: edge {} has {} {}, but only an element or hidden \
                     edge's index may be negative",
                    e / edge_size,
                    ef[field],
                    edges[i] as i32
                );
            }
        }
        for e in (0..edges.len()).step_by(edge_size) {
            let to = edges[e + edge_to_off] as usize;
            if !to.is_multiple_of(node_size) || to >= nodes.len() {
                bail!(
                    "Corrupt snapshot: edge {} points to invalid node index {}",
                    e / edge_size,
                    to
                );
            }
            let ty = edges[e + edge_type_off];
            // Traversal tells weak edges by type, so an edge of a type the
            // snapshot does not declare would pass for one that retains.
            if ty as usize >= edge_types.len() {
                bail!(
                    "Corrupt snapshot: edge {} has type {ty}, but the snapshot declares {} \
                     edge types",
                    e / edge_size,
                    edge_types.len()
                );
            }
            let ty = Some(ty);
            let named_by_index = ty == et.element || ty == et.hidden;
            let name = edges[e + edge_name_off] as usize;
            if !named_by_index && name >= strings.len() {
                bail!(
                    "Corrupt snapshot: string index {} out of bounds (strings len {})",
                    name,
                    strings.len()
                );
            }
        }

        let root_index = snapshot.root_index;
        if node_count > 0 && (!root_index.is_multiple_of(node_size) || root_index >= nodes.len()) {
            bail!("Corrupt snapshot: root_index {root_index} is not a node index");
        }

        let lf = &meta.location_fields;
        let mut location_map = HashMap::new();
        let loc = (
            position(lf, "object_index"),
            position(lf, "script_id"),
            position(lf, "line"),
            position(lf, "column"),
        );
        if loc.0.is_some() && loc.1.is_some() && loc.2.is_some() && loc.3.is_some() {
            let (o, s, l, c) = (
                loc.0.unwrap(),
                loc.1.unwrap(),
                loc.2.unwrap(),
                loc.3.unwrap(),
            );
            // -1, which V8 writes for a line or column it could not work
            // out; versions that printed it unsigned wrote 4294967295.
            const UNKNOWN: u32 = -1i32 as u32;
            let IntArray {
                fields: locations,
                negative,
            } = locations;
            let stride = lf.len();
            // A partial record would otherwise be dropped, moving its object
            // to the class without a location.
            if !locations.len().is_multiple_of(stride) {
                bail!(
                    "Corrupt snapshot: locations array length ({}) is not a multiple of its \
                     {} location fields; the file is truncated or malformed",
                    locations.len(),
                    stride
                );
            }
            for &i in &negative {
                let field = i % stride;
                if (field != l && field != c) || locations[i] != UNKNOWN {
                    bail!(
                        "Corrupt snapshot: location field '{}' holds {}",
                        lf[field],
                        locations[i] as i32
                    );
                }
            }
            for (i, chunk) in locations.chunks_exact(stride).enumerate() {
                // V8 writes a node index here. Lookups use node indices, so
                // any other value would be ignored without a word.
                let at = chunk[o] as usize;
                if !at.is_multiple_of(node_size) || at >= nodes.len() {
                    bail!("Corrupt snapshot: location {i} points to invalid node index {at}");
                }
                // Where it is unknown, the object keeps no location, like
                // one whose constructor has no script.
                if chunk[l] == UNKNOWN || chunk[c] == UNKNOWN {
                    continue;
                }
                location_map.insert(at, (chunk[s], chunk[l], chunk[c]));
            }
        } else if !locations.fields.is_empty() {
            // A file that declares records but omits a field would have every
            // record dropped here, merging unrelated constructors below.
            bail!(
                "Corrupt snapshot: {} location records but the snapshot omits a required \
                 location field",
                locations.fields.len()
            );
        }

        let mut snap = Snap {
            nodes,
            edges,
            strings,
            node_size,
            id_off,
            name_off,
            size_off,
            type_off,
            node_types,
            nt,
            edge_size,
            edge_type_off,
            edge_name_off,
            edge_to_off,
            edge_types,
            et,
            node_count,
            root: root_index / node_size,
            first_edge,
            wide_sizes,
            locations: location_map,
            dom: Vec::new(),
            has_detachedness: det_off.is_some(),
        };
        snap.dom = snap.dom_states(det_off);
        snap.transfer_owned_sizes()?;
        Ok(snap)
    }

    fn field(&self, ord: usize, off: usize) -> u32 {
        self.nodes[ord * self.node_size + off]
    }

    fn id(&self, ord: usize) -> u32 {
        self.field(ord, self.id_off)
    }

    fn self_size(&self, ord: usize) -> u64 {
        match self.field(ord, self.size_off) {
            WIDE => self.wide_sizes[&ord],
            size => size.into(),
        }
    }

    fn set_self_size(&mut self, ord: usize, size: u64) {
        let at = ord * self.node_size + self.size_off;
        if self.nodes[at] == WIDE {
            self.wide_sizes.remove(&ord);
        }
        self.nodes[at] = narrow(size).unwrap_or_else(|| {
            self.wide_sizes.insert(ord, size);
            WIDE
        });
    }

    fn raw_name(&self, ord: usize) -> &str {
        &self.strings[self.field(ord, self.name_off) as usize]
    }

    fn ty(&self, ord: usize) -> Option<u32> {
        self.type_off.map(|off| self.field(ord, off))
    }

    fn is(&self, ord: usize, ty: Option<u32>) -> bool {
        ty.is_some() && self.ty(ord) == ty
    }

    fn type_name(&self, ord: usize) -> &str {
        self.ty(ord)
            .and_then(|t| self.node_types.get(t as usize))
            .map_or("unknown", String::as_str)
    }

    /// Edge-array indices of the edges leaving node `ord`.
    fn edge_range(&self, ord: usize) -> std::iter::StepBy<std::ops::Range<usize>> {
        (self.first_edge[ord] as usize..self.first_edge[ord + 1] as usize).step_by(self.edge_size)
    }

    /// Ordinal of the node edge `e` points to.
    fn edge_to(&self, e: usize) -> usize {
        self.edges[e + self.edge_to_off] as usize / self.node_size
    }

    fn edge_is(&self, e: usize, ty: Option<u32>) -> bool {
        ty == Some(self.edges[e + self.edge_type_off])
    }

    fn edge_type_name(&self, e: usize) -> &str {
        self.edge_types
            .get(self.edges[e + self.edge_type_off] as usize)
            .map_or("unknown", String::as_str)
    }

    /// `element` and `hidden` edges carry an array index in `name_or_index`;
    /// every other type carries a string-table index.
    fn edge_name(&self, e: usize) -> Cow<'_, str> {
        if self.edge_is(e, self.et.element) || self.edge_is(e, self.et.hidden) {
            Cow::Owned(self.edges[e + self.edge_name_off].to_string())
        } else {
            Cow::Borrowed(self.string_edge_name(e))
        }
    }

    /// The name of an edge of a type named by string, such as `property`.
    fn string_edge_name(&self, e: usize) -> &str {
        &self.strings[self.edges[e + self.edge_name_off] as usize]
    }

    /// Ordinal of the node that owns edge `e`. Nodes without edges share a
    /// `first_edge` value with their successor, so take the last node whose
    /// slice starts at or before `e`.
    fn edge_source(&self, e: usize) -> usize {
        self.first_edge.partition_point(|&f| f as usize <= e) - 1
    }

    /// Whether DevTools renames this node to "Detached <name>".
    fn renamed_detached(&self, ord: usize) -> bool {
        self.has_detachedness && self.dom[ord] == DOM_DETACHED && self.is(ord, self.nt.native)
    }

    /// The node's name after the DOM-state rename: what DevTools derives the
    /// class from.
    fn name(&self, ord: usize) -> Cow<'_, str> {
        if self.renamed_detached(ord) {
            Cow::Owned(format!("Detached {}", self.raw_name(ord)))
        } else {
            Cow::Borrowed(self.raw_name(ord))
        }
    }

    /// The node's name as DevTools displays it. Plain objects are all named
    /// "Object" in the file, so they are shown by their properties instead,
    /// which is what makes the holder in a retainer list recognizable.
    fn display_name(&self, ord: usize) -> Cow<'_, str> {
        if is_plain_object(self, ord) {
            Cow::Owned(self.plain_object_name(ord))
        } else {
            self.name(ord)
        }
    }

    /// DevTools' `#plainObjectName`: property names taken alternately from
    /// the front and the back, like `{a, b, …, y, z}`, until the name nears
    /// 100 UTF-16 code units.
    fn plain_object_name(&self, ord: usize) -> String {
        // Walked from both ends, so an object with many properties costs only
        // the few names that fit.
        let mut edges = self.edge_range(ord).filter(|&e| {
            self.edge_is(e, self.et.property) && self.string_edge_name(e) != "__proto__"
        });
        let (mut front, mut back): (Vec<Cow<str>>, Vec<Cow<str>>) = (Vec::new(), Vec::new());
        let (mut from_back, mut more) = (false, false);
        // UTF-16 code units, as DevTools counts: the braces, then each side's
        // names and the separators between them.
        let mut len = 2;
        loop {
            let Some(e) = (if from_back {
                edges.next_back()
            } else {
                edges.next()
            }) else {
                break;
            };
            let name = format_property_name(self.string_edge_name(e));
            let name_len = name.encode_utf16().count();
            // Always at least one property, however long. `front` holds a
            // name only once it holds text, so this is DevTools' test that
            // the front is more than its brace.
            if !front.is_empty() && len + name_len > 100 {
                more = true;
                break;
            }
            let side = if from_back { &mut back } else { &mut front };
            from_back = !from_back;
            // As in DevTools, a separator goes in only once the side holds
            // text, so an empty name first on its side adds nothing.
            if !side.is_empty() {
                len += 2;
            } else if name.is_empty() {
                continue;
            }
            len += name_len;
            side.push(name);
        }
        back.reverse();
        // DevTools puts "…" and the back names after a separator even when
        // the front holds no text, so `["", "x"]` reads `{, x}`.
        let mut parts = vec![front.join(", ")];
        if more {
            parts.push("…".to_string());
        }
        if !back.is_empty() {
            parts.push(back.join(", "));
        }
        format!("{{{}}}", parts.join(", "))
    }

    /// DevTools' `isUserRoot` for JS snapshots: page-visible roots such as
    /// `Window` rather than V8's synthetic GC-root groups.
    fn is_user_root(&self, ord: usize) -> bool {
        !self.is(ord, self.nt.synthetic) || self.raw_name(ord) == "(Document DOM trees)"
    }

    /// DevTools' `initDetachednessAndClassIndex` + `propagateDOMState`. Blink
    /// marks only some DOM nodes attached or detached; the state spreads to
    /// the native nodes they reach, attached first, so a node reachable from
    /// both counts as attached.
    fn dom_states(&self, det_off: Option<usize>) -> Vec<u8> {
        let n = self.node_count;
        let Some(det_off) = det_off else {
            // Older snapshots have no detachedness field; Blink put a
            // "Detached " prefix in the native node's name instead.
            return (0..n)
                .map(|ord| {
                    if self.is(ord, self.nt.native) && self.raw_name(ord).starts_with("Detached ") {
                        DOM_DETACHED
                    } else {
                        DOM_UNKNOWN
                    }
                })
                .collect();
        };
        let mut state: Vec<u8> = (0..n)
            .map(|ord| (self.field(ord, det_off) & 3) as u8)
            .collect();
        // Each node keeps the first state that reaches it. State does not
        // travel through JS objects: every way into Blink's graph is a native
        // node that knows its own state.
        let mut visited = vec![false; n];
        let mut first_native_visit = |ord: usize| {
            !std::mem::replace(&mut visited[ord], true) && self.is(ord, self.nt.native)
        };
        let (mut attached, mut detached): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
        for (ord, &s) in state.iter().enumerate() {
            if s != DOM_UNKNOWN && first_native_visit(ord) {
                match s {
                    DOM_ATTACHED => attached.push(ord as u32),
                    DOM_DETACHED => detached.push(ord as u32),
                    _ => {}
                }
            }
        }
        let spreads = |e: usize| !self.edge_is(e, self.et.hidden) && !self.edge_is(e, self.et.weak);
        for (mut stack, new_state) in [(attached, DOM_ATTACHED), (detached, DOM_DETACHED)] {
            while let Some(ord) = stack.pop() {
                for e in self.edge_range(ord as usize) {
                    let to = self.edge_to(e);
                    if spreads(e) && first_native_visit(to) {
                        state[to] = new_state;
                        stack.push(to as u32);
                    }
                }
            }
        }
        state
    }

    /// DevTools' `calculateShallowSizes`: arrays and V8-internal (`hidden`)
    /// objects with a single owner get their size moved onto that owner, so
    /// a class's size includes its backing stores and the internals stop
    /// showing up as rows of their own. Skipped for snapshots taken with
    /// internals exposed (no user roots), as DevTools does.
    fn transfer_owned_sizes(&mut self) -> Result<()> {
        const UNVISITED: u32 = u32::MAX;
        const MULTIPLE: u32 = u32::MAX - 1;
        let n = self.node_count;
        if n == 0
            || !self
                .edge_range(self.root)
                .any(|e| self.is_user_root(self.edge_to(e)))
        {
            return Ok(());
        }
        let mut owners = vec![UNVISITED; n];
        let mut worklist: Vec<u32> = Vec::new();
        for (ord, owner) in owners.iter_mut().enumerate() {
            let owned = self.is(ord, self.nt.hidden)
                || self.is(ord, self.nt.array)
                || (self.is(ord, self.nt.native)
                    && self.name(ord) == "system / ExternalStringData");
            if !owned {
                *owner = ord as u32;
                worklist.push(ord as u32);
            }
        }
        while let Some(id) = worklist.pop() {
            let owner = owners[id as usize];
            for e in self.edge_range(id as usize) {
                if self.edge_is(e, self.et.weak) {
                    continue;
                }
                let target = self.edge_to(e);
                match owners[target] {
                    UNVISITED => {
                        owners[target] = owner;
                        worklist.push(target as u32);
                    }
                    o if o == target as u32 || o == owner || o == MULTIPLE => {}
                    _ => {
                        owners[target] = MULTIPLE;
                        worklist.push(target as u32);
                    }
                }
            }
        }
        for (ord, &owner) in owners.iter().enumerate() {
            if owner == UNVISITED || owner == MULTIPLE || owner as usize == ord {
                continue;
            }
            let owner = owner as usize;
            if self.is(owner, self.nt.synthetic) || owner == self.root {
                continue;
            }
            // Each size fits in a u64; a corrupt file can still make the sum
            // overflow.
            let total = self
                .self_size(owner)
                .checked_add(self.self_size(ord))
                .with_context(|| {
                    format!(
                        "Corrupt snapshot: node {} and the objects it owns add up to 2^64 bytes \
                         or more, which no heap holds",
                        self.id(owner)
                    )
                })?;
            self.set_self_size(ord, 0);
            self.set_self_size(owner, total);
        }
        Ok(())
    }
}

/// DevTools' `getNodeClassIndex` as a pure function of a node's type and its
/// name after the DOM-state rename. Object and native nodes group by name,
/// with DOM elements cut to their tag (`<div id="a" class="x">` → `<div>`);
/// every other type groups by type, because their names are per-instance
/// content (a string's value, a function's name).
fn devtools_class_name<'a>(type_name: &str, name: &'a str) -> Cow<'a, str> {
    match type_name {
        "hidden" => "(system)".into(),
        "object" | "native" => {
            if name.starts_with('<') {
                if let Some(space) = name.find(' ') {
                    return format!("{}>", &name[..space]).into();
                }
            } else if let Some(rest) = name.strip_prefix("Detached <") {
                if let Some(space) = rest.find(' ') {
                    return format!("Detached <{}>", &rest[..space]).into();
                }
            }
            name.into()
        }
        "code" => "(compiled code)".into(),
        "closure" => "Function".into(),
        "regexp" => "RegExp".into(),
        other => format!("({other})").into(),
    }
}

/// Class name per node, interned so that grouping compares integers.
#[derive(Default)]
struct Classes {
    names: Vec<String>,
    of_node: Vec<u32>,
    ids: HashMap<String, u32>,
}

impl Classes {
    fn intern(&mut self, name: Cow<'_, str>) -> u32 {
        if let Some(&id) = self.ids.get(name.as_ref()) {
            return id;
        }
        let id = self.names.len() as u32;
        let name = name.into_owned();
        self.names.push(name.clone());
        self.ids.insert(name, id);
        id
    }
}

/// A node's class before interface definitions name plain objects.
fn node_class(snap: &Snap, ord: usize) -> String {
    let name = snap.name(ord);
    match snap.ty(ord) {
        // Schemas without a type field: the name is all there is.
        None => name.into_owned(),
        Some(_) => devtools_class_name(snap.type_name(ord), &name).into_owned(),
    }
}

/// Classify every node as DevTools does, naming plain objects with `defs`.
fn assign_classes(snap: &Snap, defs: &[InterfaceDefinition]) -> Classes {
    let mut classes = Classes::default();
    // Cache per type, and per (name string, renamed) for objects and natives,
    // so the per-node loop allocates only once per distinct class.
    let mut by_type: HashMap<u32, u32> = HashMap::new();
    let mut by_name: HashMap<(u32, bool), u32> = HashMap::new();
    let mut of_node = Vec::with_capacity(snap.node_count);
    for ord in 0..snap.node_count {
        let class = match snap.ty(ord) {
            Some(t) if Some(t) != snap.nt.object && Some(t) != snap.nt.native => *by_type
                .entry(t)
                .or_insert_with(|| classes.intern(node_class(snap, ord).into())),
            _ => *by_name
                .entry((snap.field(ord, snap.name_off), snap.renamed_detached(ord)))
                .or_insert_with(|| classes.intern(node_class(snap, ord).into())),
        };
        of_node.push(class);
    }
    classes.of_node = of_node;
    apply_interface_definitions(snap, defs, &mut classes);
    classes
}

/// A DevTools "interface": the class name given to plain `Object`s that share
/// a popular set of property names, such as `{x, y}`.
#[derive(Debug)]
struct InterfaceDefinition {
    name: String,
    properties: Vec<String>,
}

// Limits from HeapSnapshot.ts.
const MIN_INTERFACE_PROPERTY_COUNT: usize = 1;
const MAX_INTERFACE_NAME_LENGTH: usize = 120;
const MIN_OBJECT_COUNT_PER_INTERFACE: f64 = 2.0;
const MIN_OBJECT_PROPORTION_PER_INTERFACE: f64 = 1000.0;

fn is_plain_object(snap: &Snap, ord: usize) -> bool {
    snap.is(ord, snap.nt.object) && snap.raw_name(ord) == "Object"
}

/// JS compares strings by UTF-16 code unit, which orders some characters
/// differently from Rust's byte order. Interface matching depends on it.
fn cmp_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// DevTools' `formatPropertyName`: quote names that would make `{a, b}`
/// ambiguous, as `JSON.stringify` does.
fn format_property_name(name: &str) -> Cow<'_, str> {
    if name.contains([',', '\'', '"', '{', '}']) {
        Cow::Owned(serde_json::Value::from(name).to_string())
    } else {
        Cow::Borrowed(name)
    }
}

/// DevTools' `inferInterfaceDefinitions`: name each plain object after its
/// leading properties, then keep the names shared by at least 2 objects and
/// by at least 1 in 1000 plain objects, most common first.
fn infer_interface_definitions(snap: &Snap) -> Vec<InterfaceDefinition> {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut candidates: Vec<(InterfaceDefinition, usize)> = Vec::new();
    let mut total = 0usize;
    let mut name = String::new();
    let mut props: Vec<usize> = Vec::new();
    for ord in 0..snap.node_count {
        if !is_plain_object(snap, ord) {
            continue;
        }
        total += 1;
        name.clear();
        name.push('{');
        // DevTools measures in UTF-16 code units.
        let mut name_len = 1usize;
        props.clear();
        for e in snap.edge_range(ord) {
            if !snap.edge_is(e, snap.et.property) {
                continue;
            }
            let edge_name = snap.edge_name(e);
            if edge_name == "__proto__" {
                continue;
            }
            let formatted = format_property_name(&edge_name);
            let formatted_len = formatted.encode_utf16().count();
            if name_len > MIN_INTERFACE_PROPERTY_COUNT
                && name_len + formatted_len > MAX_INTERFACE_NAME_LENGTH
            {
                break;
            }
            if name_len != 1 {
                name.push_str(", ");
                name_len += 2;
            }
            name.push_str(&formatted);
            name_len += formatted_len;
            props.push(e);
        }
        if props.is_empty() {
            continue;
        }
        name.push('}');
        match index.get(&name) {
            Some(&i) => candidates[i].1 += 1,
            None => {
                index.insert(name.clone(), candidates.len());
                let properties = props
                    .iter()
                    .map(|&e| snap.edge_name(e).into_owned())
                    .collect();
                candidates.push((
                    InterfaceDefinition {
                        name: name.clone(),
                        properties,
                    },
                    1,
                ));
            }
        }
    }
    // Stable, like JS's sort: equal counts keep first-seen order.
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    let min_count =
        MIN_OBJECT_COUNT_PER_INTERFACE.max(total as f64 / MIN_OBJECT_PROPORTION_PER_INTERFACE);
    candidates
        .into_iter()
        .take_while(|(_, count)| *count as f64 >= min_count)
        .map(|(def, _)| def)
        .collect()
}

/// DevTools' `selectBetterMatch` over (property count, definition index):
/// more properties wins, then the earlier, more common definition. `None` is
/// the fallback match, which keeps the class "Object".
fn better_match(a: Option<(usize, usize)>, b: Option<(usize, usize)>) -> Option<(usize, usize)> {
    let key = |m: Option<(usize, usize)>| {
        m.map_or((0, Reverse(usize::MAX)), |(count, index)| {
            (count, Reverse(index))
        })
    };
    if key(b) > key(a) {
        b
    } else {
        a
    }
}

/// DevTools' `applyInterfaceDefinitions`: give each plain object the class of
/// the best definition whose properties it has. Comparisons apply the
/// current snapshot's definitions to the base too, so both sides use the
/// same names.
fn apply_interface_definitions(snap: &Snap, defs: &[InterfaceDefinition], classes: &mut Classes) {
    if defs.is_empty() {
        return;
    }
    let mut matcher = InterfaceMatcher::new(defs);
    let def_class: Vec<u32> = defs
        .iter()
        .map(|d| classes.intern(Cow::Borrowed(&d.name)))
        .collect();
    for ord in 0..snap.node_count {
        if !is_plain_object(snap, ord) {
            continue;
        }
        if let Some(i) = matcher.best(snap, ord) {
            classes.of_node[ord] = def_class[i];
        }
    }
}

#[derive(Default)]
struct TreeNode {
    next: HashMap<String, usize>,
    matched: Option<(usize, usize)>,
    greatest_next: Option<String>,
}

/// Picks a plain object's definition one object at a time, so that
/// inspecting one node does not classify the whole heap.
struct InterfaceMatcher<'s> {
    /// A trie over sorted property names; each definition ends at one node.
    tree: Vec<TreeNode>,
    // Reused from one object to the next.
    props: Vec<&'s str>,
    states: Vec<usize>,
    current: Vec<usize>,
}

impl<'s> InterfaceMatcher<'s> {
    fn new(defs: &[InterfaceDefinition]) -> Self {
        let mut tree = vec![TreeNode::default()];
        for (i, def) in defs.iter().enumerate() {
            let mut props: Vec<&str> = def.properties.iter().map(String::as_str).collect();
            props.sort_by(|a, b| cmp_utf16(a, b));
            let mut cur = 0;
            for p in &props {
                cur = match tree[cur].next.get(*p).copied() {
                    Some(next) => next,
                    None => {
                        let next = tree.len();
                        tree.push(TreeNode::default());
                        let node = &mut tree[cur];
                        node.next.insert(p.to_string(), next);
                        if node
                            .greatest_next
                            .as_deref()
                            .is_none_or(|g| cmp_utf16(g, p).is_lt())
                        {
                            node.greatest_next = Some(p.to_string());
                        }
                        next
                    }
                };
            }
            if tree[cur].matched.is_none() {
                tree[cur].matched = Some((props.len(), i));
            }
        }
        InterfaceMatcher {
            tree,
            props: Vec::new(),
            states: Vec::new(),
            current: Vec::new(),
        }
    }

    /// Index of the definition plain object `ord` is named after; `None`
    /// keeps it "Object".
    fn best(&mut self, snap: &'s Snap, ord: usize) -> Option<usize> {
        let Self {
            tree,
            props,
            states,
            current,
        } = self;
        props.clear();
        props.extend(
            snap.edge_range(ord)
                .filter(|&e| snap.edge_is(e, snap.et.property))
                .map(|e| snap.string_edge_name(e)),
        );
        props.sort_by(|a, b| cmp_utf16(a, b));
        // Several trie paths can be live at once, since an object may have
        // properties no definition lists. `states` mirrors the JS Set,
        // including its insertion order.
        states.clear();
        states.push(0);
        let mut best = better_match(None, tree[0].matched);
        for p in props.iter() {
            current.clear();
            current.extend_from_slice(states);
            for &state in current.iter() {
                let node = &tree[state];
                if node
                    .greatest_next
                    .as_deref()
                    .is_none_or(|g| !cmp_utf16(p, g).is_lt())
                {
                    states.retain(|&s| s != state);
                }
                if let Some(&next) = node.next.get(*p) {
                    if !states.contains(&next) {
                        states.push(next);
                    }
                    best = better_match(best, tree[next].matched);
                }
            }
        }
        best.map(|(_, i)| i)
    }
}

/// A class row's identity across two snapshots. DevTools splits `object`
/// nodes by where V8 says their constructor is defined (script id, line,
/// column, the last two from 0) when the snapshot records it, so two
/// constructors that share a name stay separate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ClassKey {
    name: String,
    location: Option<Location>,
}

/// (node id, self size) of each counted node, sorted by id. A size of `WIDE`
/// or more is stored as `WIDE`, its exact value in `Aggregates::wide`, which
/// keeps a member at 8 bytes on heaps of millions of objects.
type Members = Vec<(u32, u32)>;

/// One snapshot's class rows, keeping only what a diff needs.
#[derive(Default)]
struct Aggregates {
    classes: HashMap<ClassKey, Members>,
    /// Exact self size, by node id, of each member stored as `WIDE`.
    wide: HashMap<u32, u64>,
}

impl Aggregates {
    /// A class's members, empty when the snapshot has none of it.
    fn members(&self, key: &ClassKey) -> &[(u32, u32)] {
        self.classes.get(key).map_or(&[][..], Vec::as_slice)
    }

    fn size(&self, (id, size): (u32, u32)) -> u64 {
        match size {
            WIDE => self.wide[&id],
            size => size.into(),
        }
    }
}

/// DevTools' `buildAggregates`, keeping only what a diff needs.
fn build_class_aggregates(snap: &Snap, classes: &Classes) -> Aggregates {
    let mut groups: HashMap<(u32, Option<Location>), Members> = HashMap::new();
    let mut wide = HashMap::new();
    for ord in 0..snap.node_count {
        let size = snap.self_size(ord);
        // Zero-size nodes are in no class, as in DevTools: synthetic roots,
        // inline numbers, and the arrays and internals whose size
        // `transfer_owned_sizes` moved onto their owner.
        if size == 0 {
            continue;
        }
        let location = if snap.is(ord, snap.nt.object) {
            snap.locations.get(&(ord * snap.node_size)).copied()
        } else {
            None
        };
        let id = snap.id(ord);
        let size = narrow(size).unwrap_or_else(|| {
            wide.insert(id, size);
            WIDE
        });
        groups
            .entry((classes.of_node[ord], location))
            .or_default()
            .push((id, size));
    }
    let classes = groups
        .into_iter()
        .map(|((class, location), mut members)| {
            members.sort_unstable();
            let key = ClassKey {
                name: classes.names[class as usize].clone(),
                location,
            };
            (key, members)
        })
        .collect();
    Aggregates { classes, wide }
}

/// Read and index a .heapsnapshot file.
fn load_snapshot(path: &str) -> Result<Snap> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open heap snapshot file at: {path}"))?;
    let raw: RawSnapshot = serde_json::from_reader(BufReader::new(file)).with_context(|| {
        format!("Failed to deserialize heap snapshot file {path}. Ensure it is valid JSON.")
    })?;
    Snap::new(raw).with_context(|| format!("Failed to read heap snapshot {path}"))
}

/// One summary row of a diff. Holds only counters: per-object detail is
/// computed on demand by `compute_class_node_detail` for the one class asked
/// about.
#[derive(Debug, Clone)]
struct HeapSnapshotClassDiff {
    key: ClassKey,
    added_count: usize,
    removed_count: usize,
    added_size: u64,
    removed_size: u64,
}

impl HeapSnapshotClassDiff {
    fn count_delta(&self) -> i64 {
        self.added_count as i64 - self.removed_count as i64
    }

    /// Exact: `diff_snapshots` keeps both totals below 2^63.
    fn size_delta(&self) -> i64 {
        self.added_size as i64 - self.removed_size as i64
    }
}

/// Walk one class's id-sorted members in both snapshots like DevTools'
/// `calculateDiffForClass`, reporting `(added, id, size)` for each id found
/// on one side only.
fn merge_diff(
    base: &Aggregates,
    current: &Aggregates,
    key: &ClassKey,
    mut report: impl FnMut(bool, u32, u64),
) {
    let (b, c) = (base.members(key), current.members(key));
    let (mut i, mut j) = (0, 0);
    while i < b.len() && j < c.len() {
        match b[i].0.cmp(&c[j].0) {
            std::cmp::Ordering::Less => {
                report(false, b[i].0, base.size(b[i]));
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                report(true, c[j].0, current.size(c[j]));
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    for &m in &b[i..] {
        report(false, m.0, base.size(m));
    }
    for &m in &c[j..] {
        report(true, m.0, current.size(m));
    }
}

/// Per-class diff rows with any change, sorted by size delta descending, then
/// by class, so each `--class-index` names the same row on every run.
fn diff_snapshots(base: &Aggregates, current: &Aggregates) -> Result<Vec<HeapSnapshotClassDiff>> {
    let mut diffs = Vec::new();
    let keys = base.classes.keys().chain(
        current
            .classes
            .keys()
            .filter(|k| !base.classes.contains_key(*k)),
    );
    for key in keys {
        // Summed in u128, which fewer than 2^32 sizes below 2^64 cannot
        // overflow, and checked below.
        let (mut added_count, mut removed_count, mut added_size, mut removed_size) =
            (0, 0, 0u128, 0u128);
        merge_diff(base, current, key, |added, _, size| {
            if added {
                added_count += 1;
                added_size += u128::from(size);
            } else {
                removed_count += 1;
                removed_size += u128::from(size);
            }
        });
        if added_count > 0 || removed_count > 0 {
            // `size_delta` subtracts the totals as i64s. Only a corrupt file
            // reaches 2^63 bytes.
            let total = |size: u128, objects: &str| {
                u64::try_from(size)
                    .ok()
                    .filter(|&size| size < 1 << 63)
                    .with_context(|| {
                        format!(
                            "Corrupt snapshot: {objects} objects of class '{}' add up to 2^63 \
                             bytes or more, which no heap holds",
                            key.name
                        )
                    })
            };
            diffs.push(HeapSnapshotClassDiff {
                key: key.clone(),
                added_count,
                removed_count,
                added_size: total(added_size, "the current snapshot's added")?,
                removed_size: total(removed_size, "the base snapshot's removed")?,
            });
        }
    }
    diffs.sort_by(|a, b| {
        b.size_delta()
            .cmp(&a.size_delta())
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(diffs)
}

/// One added or deleted object of a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiffObject {
    added: bool,
    id: u32,
    self_size: u64,
}

/// The added and deleted objects of one class, largest first so the first
/// page shows what matters most.
fn compute_class_node_detail(
    base: &Aggregates,
    current: &Aggregates,
    key: &ClassKey,
) -> Vec<DiffObject> {
    let mut objects = Vec::new();
    merge_diff(base, current, key, |added, id, self_size| {
        objects.push(DiffObject {
            added,
            id,
            self_size,
        })
    });
    objects.sort_by(|a, b| {
        b.self_size
            .cmp(&a.self_size)
            .then_with(|| b.added.cmp(&a.added))
            .then_with(|| a.id.cmp(&b.id))
    });
    objects
}

/// Heuristic for snapshots that cannot meaningfully be diffed: V8 heap object
/// ids are only stable within one browser session, so two non-empty
/// snapshots whose counted objects share no id almost certainly come from
/// different sessions. Class-agnostic, because an object can change class
/// between snapshots but not its id.
fn snapshots_share_no_node_ids(base: &Aggregates, current: &Aggregates) -> bool {
    let base_ids: HashSet<u32> = base.classes.values().flatten().map(|&(id, _)| id).collect();
    let mut current_ids = current
        .classes
        .values()
        .flatten()
        .map(|&(id, _)| id)
        .peekable();
    !base_ids.is_empty()
        && current_ids.peek().is_some()
        && current_ids.all(|id| !base_ids.contains(&id))
}

/// The rows of `total` that `opts` selects.
fn page_bounds(total: usize, opts: &ListOptions) -> std::ops::Range<usize> {
    let start = opts.offset.min(total);
    start..start.saturating_add(opts.limit).min(total)
}

/// "showing 1-50 of 912; next page: --offset 50", so a reader knows how much
/// the page left out and how to get the rest.
fn page_summary(total: usize, offset: usize, shown: usize) -> String {
    if total == 0 {
        return "none".to_string();
    }
    if shown == 0 {
        return format!("showing 0 of {total} (--offset {offset} is past the end)");
    }
    let end = offset + shown;
    let mut summary = format!("showing {}-{} of {}", offset + 1, end, total);
    if end < total {
        summary.push_str(&format!("; next page: --offset {end}"));
    }
    summary
}

/// CSV-escape a field: names like `(closure)` are safe, but `{a, b}` or a
/// string node's content would break naive CSV parsing.
fn csv_escape(s: &str) -> Cow<'_, str> {
    if s.contains([',', '"', '\n', '\r']) {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
    }
}

/// A value as a CSV cell: strings escaped, numbers as written, null empty.
fn csv_cell(v: &serde_json::Value) -> Cow<'_, str> {
    match v {
        serde_json::Value::String(s) => csv_escape(s),
        serde_json::Value::Null => Cow::Borrowed(""),
        v => Cow::Owned(v.to_string()),
    }
}

/// Where a class's constructor is defined, as `scriptId:line:column` with
/// line and column counted from 1 as DevTools displays them (V8 stores them
/// from 0); null when the snapshot records none. It tells apart the rows of
/// two classes that share a name.
fn location_value(location: Option<Location>) -> serde_json::Value {
    match location {
        Some((script, line, column)) => json!(format!(
            "{script}:{}:{}",
            u64::from(line) + 1,
            u64::from(column) + 1
        )),
        None => serde_json::Value::Null,
    }
}

/// Columns of a diff row, in output order.
const DIFF_COLUMNS: [&str; 9] = [
    "idx",
    "className",
    "location",
    "addedCount",
    "removedCount",
    "countDelta",
    "addedSize",
    "removedSize",
    "sizeDelta",
];

/// A diff row's values in `DIFF_COLUMNS` order, the class name cut to length.
fn diff_values(
    idx: usize,
    d: &HeapSnapshotClassDiff,
    opts: &ListOptions,
) -> [serde_json::Value; 9] {
    [
        json!(idx),
        json!(crate::format::truncate_chars(
            &d.key.name,
            opts.max_name_length
        )),
        location_value(d.key.location),
        json!(d.added_count),
        json!(d.removed_count),
        json!(d.count_delta()),
        json!(d.added_size),
        json!(d.removed_size),
        json!(d.size_delta()),
    ]
}

fn diff_json(
    idx: usize,
    d: &HeapSnapshotClassDiff,
    opts: &ListOptions,
) -> serde_json::Map<String, serde_json::Value> {
    DIFF_COLUMNS
        .iter()
        .map(|c| c.to_string())
        .zip(diff_values(idx, d, opts))
        .collect()
}

fn csv_distance(distance: Option<u32>) -> String {
    distance.map(|d| d.to_string()).unwrap_or_default()
}

/// Format the summary diff table (one row per changed class).
fn format_class_diff_summary(
    diffs: &[HeapSnapshotClassDiff],
    opts: &ListOptions,
    format: OutputFormat,
) -> Result<String> {
    use std::fmt::Write;
    let range = page_bounds(diffs.len(), opts);
    let start = range.start;
    let page = &diffs[range];
    if !format.is_text() {
        let rows: Vec<serde_json::Value> = page
            .iter()
            .enumerate()
            .map(|(i, d)| diff_json(start + i, d, opts).into())
            .collect();
        return crate::format::format_structured(
            &json!({
                "totalClasses": diffs.len(),
                "offset": opts.offset,
                "limit": opts.limit,
                "diffs": rows,
            }),
            format,
        );
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Classes with changes, largest sizeDelta first: {}",
        page_summary(diffs.len(), opts.offset, page.len())
    );
    if !page.is_empty() {
        let _ = writeln!(out, "{}", DIFF_COLUMNS.join(","));
    }
    for (i, d) in page.iter().enumerate() {
        let values = diff_values(start + i, d, opts);
        let cells: Vec<Cow<str>> = values.iter().map(csv_cell).collect();
        let _ = writeln!(out, "{}", cells.join(","));
    }
    Ok(out)
}

/// Format one class's added/deleted objects. `idx` is the summary row it was
/// selected from, so the header can be matched back to the summary.
fn format_class_diff_detail(
    idx: usize,
    diff: &HeapSnapshotClassDiff,
    objects: &[DiffObject],
    opts: &ListOptions,
    format: OutputFormat,
) -> Result<String> {
    use std::fmt::Write;
    let page = &objects[page_bounds(objects.len(), opts)];
    let op = |o: &DiffObject| if o.added { "+" } else { "-" };
    if !format.is_text() {
        let nodes: Vec<serde_json::Value> = page
            .iter()
            .map(|o| json!({ "op": op(o), "nodeId": o.id, "selfSize": o.self_size }))
            .collect();
        let mut row = diff_json(idx, diff, opts);
        row.insert("totalNodes".into(), json!(objects.len()));
        row.insert("offset".into(), json!(opts.offset));
        row.insert("limit".into(), json!(opts.limit));
        row.insert("nodes".into(), json!(nodes));
        return crate::format::format_structured(&row.into(), format);
    }
    let values = diff_values(idx, diff, opts);
    let header: Vec<String> = DIFF_COLUMNS
        .iter()
        .zip(&values)
        .map(|(column, v)| format!("{column}:{}", csv_cell(v)))
        .collect();
    let mut out = String::new();
    let _ = writeln!(out, "{}", header.join(","));
    let _ = writeln!(
        out,
        "\nObjects, largest selfSize first: {}",
        page_summary(objects.len(), opts.offset, page.len())
    );
    if !page.is_empty() {
        out.push_str("op,nodeId,selfSize\n");
    }
    for o in page {
        let _ = writeln!(out, "{},{},{}", op(o), o.id, o.self_size);
    }
    Ok(out)
}

/// Offline implementation of `compare-heapsnapshots`. Parses both files,
/// diffs, and renders summary or per-class detail depending on `class_index`.
pub async fn compare_heapsnapshots_offline(
    base_path: &str,
    current_path: &str,
    class_index: Option<usize>,
    opts: ListOptions,
    format: OutputFormat,
) -> Result<CommandResult> {
    let (base_path, current_path) = (base_path.to_string(), current_path.to_string());
    let (out, no_overlap) = tokio::task::spawn_blocking(move || -> Result<(String, bool)> {
        // The current snapshot goes first because DevTools names plain
        // objects in both snapshots with the current one's interface
        // definitions. Each parsed file is dropped once its aggregates exist,
        // so only one is in memory at a time.
        let (current, defs) = {
            let snap = load_snapshot(&current_path)?;
            let defs = infer_interface_definitions(&snap);
            (
                build_class_aggregates(&snap, &assign_classes(&snap, &defs)),
                defs,
            )
        };
        let base = {
            let snap = load_snapshot(&base_path)?;
            build_class_aggregates(&snap, &assign_classes(&snap, &defs))
        };
        let diffs = diff_snapshots(&base, &current)?;
        // An object still in its class on both sides proves the ids overlap,
        // the usual case, which spares the whole-heap check.
        let removed: usize = diffs.iter().map(|d| d.removed_count).sum();
        let kept_one = base.classes.values().map(Vec::len).sum::<usize>() > removed;
        let no_overlap = !kept_one && snapshots_share_no_node_ids(&base, &current);

        let out = match class_index {
            None => format_class_diff_summary(&diffs, &opts, format)?,
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
                let objects = compute_class_node_detail(&base, &current, &diff.key);
                format_class_diff_detail(idx, diff, &objects, &opts, format)?
            }
        };
        Ok((out, no_overlap))
    })
    .await
    .map_err(|e| anyhow!("Failed to execute blocking snapshot diff: {e}"))??;

    if no_overlap {
        // Warning, not an error: the heuristic could misfire on exotic
        // snapshots, and the raw diff may still be wanted.
        eprintln!(
            "[warning] The two snapshots share no node IDs — they very likely come from \
             different Chrome sessions. Node IDs are only stable within a single session, \
             so this diff reports everything as added+removed and is not meaningful. \
             Re-take both snapshots within the same session."
        );
    }
    Ok(CommandResult::output(out))
}

/// DevTools skips the descriptor links of a `(map descriptors)` array when
/// ranking paths: maps share those arrays, so a link valid for one map is not
/// necessarily the right retainer for another.
fn skip_descriptor_link(snap: &Snap, ord: usize, e: usize) -> bool {
    if !snap.is(ord, snap.nt.array) || snap.raw_name(ord) != "(map descriptors)" {
        return false;
    }
    snap.edge_name(e)
        .parse::<usize>()
        .is_ok_and(|i| i >= 2 && i % 3 == 1)
}

/// Older V8 snapshots give a native context a `sloppy_function_map` shortcut
/// to a map. DevTools leaves that edge out of distance ranking: it is an
/// internal detail, not a retainer an application path should lose to.
fn skip_sloppy_function_map(snap: &Snap, ord: usize, e: usize) -> bool {
    snap.is(ord, snap.nt.hidden)
        && snap.raw_name(ord) == "system / NativeContext"
        && snap.edge_name(e) == "sloppy_function_map"
}

/// DevTools' `tryParseWeakMapEdgeName`. V8 gives a WeakMap value two
/// `internal` edges, one from the key and one from the WeakMap's table, both
/// named `<n> / part of key (<key> @<id>) -> value (<value> @<id>) pair in
/// WeakMap (table @<id>)` with only `<n>` differing. Returns the rest, which
/// pairs them. The names in it are any text, line breaks included: V8 embeds
/// the value's name, which can be a multiline string. DevTools' regex, whose
/// `.` stops at a line break, takes such a pair for two unrelated edges.
fn weak_map_pair(name: &str) -> Option<&str> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let shared = name.trim_start_matches(|c: char| c.is_ascii_digit());
    if shared.len() == name.len() {
        return None;
    }
    let (middle, table) = shared
        .strip_prefix(" / part of key (")?
        .strip_suffix(')')?
        .rsplit_once(" pair in WeakMap (table @")?;
    let (key_and_value, value_id) = middle.strip_suffix(')')?.rsplit_once(" @")?;
    if !digits(table) || !digits(value_id) {
        return None;
    }
    // `<key> @<id>) -> value (<value>`, where either name may itself
    // contain `) -> value (`.
    key_and_value
        .match_indices(") -> value (")
        .any(|(i, _)| {
            key_and_value[..i]
                .rsplit_once(" @")
                .is_some_and(|(_, id)| digits(id))
        })
        .then_some(shared)
}

/// The key's and the table's node ids embedded in a WeakMap pair edge's name.
/// Both halves carry both ids even when only one half is still present, which
/// is what lets a lone table retainer be marked conditional on its missing key.
fn weak_map_pair_ids(name: &str) -> Option<(u32, u32)> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let shared = name.trim_start_matches(|c: char| c.is_ascii_digit());
    if shared.len() == name.len() {
        return None;
    }
    let (middle, table) = shared
        .strip_prefix(" / part of key (")?
        .strip_suffix(')')?
        .rsplit_once(" pair in WeakMap (table @")?;
    if !digits(table) {
        return None;
    }
    let (key_and_value, _) = middle.strip_suffix(')')?.rsplit_once(" @")?;
    // The key's name may itself contain `) -> value (`, so take the first
    // separator that has a numeric id before it.
    let key = key_and_value
        .match_indices(") -> value (")
        .find_map(|(i, _)| {
            let id = key_and_value[..i].rsplit_once(" @")?.1;
            digits(id).then(|| id.parse::<u32>().ok()).flatten()
        })?;
    Some((key, table.parse().ok()?))
}

/// The other edge a WeakMap-pair retainer needs in order to keep the value
/// alive. Each half names both the key and the table, so this works even when
/// the other half is not a retainer (the missing-key case).
fn weak_map_condition(snap: &Snap, edge: usize) -> Option<PairedEdge> {
    let (key, table) = weak_map_pair_ids(snap.string_edge_name(edge))?;
    let source = snap.id(snap.edge_source(edge));
    let (role, node_id) = if source == table {
        ("key", key)
    } else if source == key {
        ("table", table)
    } else {
        return None;
    };
    Some(PairedEdge { role, node_id })
}

/// Hop count from the root and the edge that set it, by a BFS that mirrors
/// DevTools' `calculateDistances`: the user roots (`Window`, "(Document DOM
/// trees)") are expanded before the GC-root groups, so paths lead through
/// page objects rather than V8 internals when both exist. Weak edges keep
/// nothing alive and are skipped.
///
/// A WeakMap value stays alive only while both its key and the WeakMap's
/// table do, so as in DevTools the first of its pair of edges to arrive
/// waits for the second, which sets the distance. The third result maps each
/// node reached that way to the edge that waited.
/// The fourth result is whether each node was reached in the page-root pass
/// (or only in the GC-root pass), so retainers can be ranked page first.
fn bfs_from_root(snap: &Snap) -> (Vec<u32>, Vec<u32>, HashMap<usize, usize>, Vec<bool>) {
    let n = snap.node_count;
    let mut dist = vec![NONE; n];
    let mut parent = vec![NONE; n];
    let mut page = vec![false; n];
    let mut waited = HashMap::new();
    if n == 0 {
        return (dist, parent, waited, page);
    }
    let root = snap.root;
    dist[root] = 0;
    let mut queue: Vec<u32> = Vec::new();
    for e in snap.edge_range(root) {
        let to = snap.edge_to(e);
        if snap.edge_is(e, snap.et.weak) || dist[to] != NONE || !snap.is_user_root(to) {
            continue;
        }
        dist[to] = 1;
        parent[to] = e as u32;
        page[to] = true;
        queue.push(to as u32);
    }
    let mut head = 0;
    // First edge of each pair, by the part of the name the pair shares. It
    // carries over to the GC-root pass, as in DevTools.
    let mut pending: HashMap<&str, usize> = HashMap::new();
    let mut expand =
        |queue: &mut Vec<u32>, head: &mut usize, page: &mut [bool], from_page: bool| {
            while *head < queue.len() {
                let ord = queue[*head] as usize;
                *head += 1;
                for e in snap.edge_range(ord) {
                    let to = snap.edge_to(e);
                    if snap.edge_is(e, snap.et.weak)
                        || dist[to] != NONE
                        || skip_descriptor_link(snap, ord, e)
                        || skip_sloppy_function_map(snap, ord, e)
                    {
                        continue;
                    }
                    if snap.edge_is(e, snap.et.internal) {
                        if let Some(pair) = weak_map_pair(snap.string_edge_name(e)) {
                            match pending.remove(pair) {
                                Some(first) => {
                                    waited.insert(to, first);
                                }
                                None => {
                                    pending.insert(pair, e);
                                    continue;
                                }
                            }
                        }
                    }
                    dist[to] = dist[ord] + 1;
                    parent[to] = e as u32;
                    page[to] = from_page;
                    queue.push(to as u32);
                }
            }
        };
    expand(&mut queue, &mut head, &mut page, true);
    // Then whatever only the GC roots reach.
    queue.push(root as u32);
    expand(&mut queue, &mut head, &mut page, false);
    (dist, parent, waited, page)
}

/// Edges from the root to `target` along the BFS tree, or `None` when the
/// BFS did not reach it.
fn retaining_path(snap: &Snap, parent: &[u32], target: usize) -> Option<Vec<usize>> {
    let mut edges = Vec::new();
    let mut cur = target;
    while cur != snap.root {
        let e = parent[cur];
        if e == NONE {
            return None;
        }
        edges.push(e as usize);
        cur = snap.edge_source(e as usize);
    }
    edges.reverse();
    Some(edges)
}

/// (edge, retaining node) for every edge into `target`, filtered like
/// DevTools' retainers view: weak edges do not retain, and the synthetic
/// root's edges say nothing about the page.
fn direct_retainers(snap: &Snap, target: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for src in 0..snap.node_count {
        if src == snap.root {
            continue;
        }
        for e in snap.edge_range(src) {
            if snap.edge_to(e) == target && !snap.edge_is(e, snap.et.weak) {
                out.push((e, src));
            }
        }
    }
    out
}

fn dom_state_name(state: u8) -> &'static str {
    match state {
        DOM_ATTACHED => "attached",
        DOM_DETACHED => "detached",
        _ => "unknown",
    }
}

struct NodeInfo {
    id: u32,
    name: String,
    class_name: String,
    node_type: String,
    detachedness: &'static str,
    self_size: u64,
    distance: Option<u32>,
}

/// One edge plus the node at its far end: a retainer (the edge's source) or
/// a retaining-path step (the edge's target).
struct EdgeRow {
    edge_name: String,
    edge_type: String,
    node_id: u32,
    node_name: String,
    node_type: String,
    self_size: u64,
    distance: Option<u32>,
    /// For a path step or a direct retainer that is one edge of a WeakMap
    /// entry: the source of the other edge, which must stay alive too for it
    /// to retain anything.
    conditional_on: Option<PairedEdge>,
}

/// The other half of a WeakMap entry's pair of edges.
struct PairedEdge {
    /// "key" or "table": which half its source is.
    role: &'static str,
    node_id: u32,
}

struct NodeReport {
    node: NodeInfo,
    retainers_total: usize,
    retainers: Vec<EdgeRow>,
    /// Root first; `None` when the traversal from the root does not reach
    /// the node.
    path: Option<Vec<EdgeRow>>,
}

/// Everything `inspect-heapsnapshot-node` prints, with names already cut so
/// the snapshot can be dropped before formatting.
fn inspect_node(snap: &Snap, node_id: u64, opts: &ListOptions) -> Result<NodeReport> {
    let ord = u32::try_from(node_id)
        .ok()
        .and_then(|id| (0..snap.node_count).find(|&ord| snap.id(ord) == id))
        .ok_or_else(|| anyhow!("Node with ID {} not found in snapshot file", node_id))?;
    let truncate = crate::format::truncate_chars;
    let max = opts.max_name_length;
    // Interface definitions, which take the whole heap to infer, only ever
    // rename plain objects; any other node's class is its own.
    let class_name = if is_plain_object(snap, ord) {
        let defs = infer_interface_definitions(snap);
        match InterfaceMatcher::new(&defs).best(snap, ord) {
            Some(i) => defs[i].name.clone(),
            None => node_class(snap, ord),
        }
    } else {
        node_class(snap, ord)
    };
    let (dist, parent, waited, page) = bfs_from_root(snap);
    let distance = |ord: usize| (dist[ord] != NONE).then_some(dist[ord]);
    let row = |e: usize, ord: usize| EdgeRow {
        edge_name: truncate(&snap.edge_name(e), max).into_owned(),
        edge_type: snap.edge_type_name(e).to_string(),
        node_id: snap.id(ord),
        node_name: truncate(&snap.display_name(ord), max).into_owned(),
        node_type: snap.type_name(ord).to_string(),
        self_size: snap.self_size(ord),
        distance: distance(ord),
        conditional_on: None,
    };

    let node = NodeInfo {
        id: snap.id(ord),
        name: truncate(&snap.display_name(ord), max).into_owned(),
        class_name: truncate(&class_name, max).into_owned(),
        node_type: snap.type_name(ord).to_string(),
        detachedness: dom_state_name(snap.dom[ord]),
        self_size: snap.self_size(ord),
        distance: distance(ord),
    };

    let mut retainers = direct_retainers(snap, ord);
    // Page-root retainers come before GC-only ones even when the latter are a
    // hop closer: DevTools' page paths are the ones a fix can act on. Within
    // each pass, closest to the root first. Unreached retainers sort last.
    retainers.sort_by_key(|&(e, src)| (!page[src], dist[src], e));
    let page = &retainers[page_bounds(retainers.len(), opts)];
    // A WeakMap value's key and table are both listed as direct retainers, but
    // neither keeps it alive alone. Mark each half with the other source, just
    // as a conditional path step is marked, so a lone table in the list does
    // not read as a sufficient retainer.
    let retainer_rows = page
        .iter()
        .map(|&(e, src)| EdgeRow {
            conditional_on: weak_map_condition(snap, e),
            ..row(e, src)
        })
        .collect();

    let step = |e: usize| {
        let to = snap.edge_to(e);
        let conditional_on = waited.get(&to).map(|&first| {
            let node_id = snap.id(snap.edge_source(first));
            let table = format!("(table @{node_id})");
            PairedEdge {
                role: if snap.string_edge_name(e).ends_with(&table) {
                    "table"
                } else {
                    "key"
                },
                node_id,
            }
        });
        EdgeRow {
            conditional_on,
            ..row(e, to)
        }
    };
    let path =
        retaining_path(snap, &parent, ord).map(|edges| edges.into_iter().map(step).collect());

    Ok(NodeReport {
        node,
        retainers_total: retainers.len(),
        retainers: retainer_rows,
        path,
    })
}

fn edge_row_json(r: &EdgeRow) -> serde_json::Value {
    let mut row = json!({
        "edgeName": r.edge_name,
        "edgeType": r.edge_type,
        "nodeId": r.node_id,
        "nodeName": r.node_name,
        "type": r.node_type,
        "selfSize": r.self_size,
        "distance": r.distance,
    });
    // Only on the rare conditional step, so other paths keep their shape.
    if let Some(c) = &r.conditional_on {
        row["conditionalOn"] = json!({ "role": c.role, "nodeId": c.node_id });
    }
    row
}

/// Format the node, its retainers and its retaining path for display.
fn format_node_report(
    report: &NodeReport,
    opts: &ListOptions,
    format: OutputFormat,
) -> Result<String> {
    use std::fmt::Write;
    let n = &report.node;
    if !format.is_text() {
        return crate::format::format_structured(
            &json!({
                "nodeId": n.id,
                "nodeName": n.name,
                "className": n.class_name,
                "type": n.node_type,
                "detachedness": n.detachedness,
                "selfSize": n.self_size,
                "distance": n.distance,
                "retainers": {
                    "total": report.retainers_total,
                    "offset": opts.offset,
                    "limit": opts.limit,
                    "items": report.retainers.iter().map(edge_row_json).collect::<Vec<_>>(),
                },
                "retainingPath": report
                    .path
                    .as_ref()
                    .map(|p| p.iter().map(edge_row_json).collect::<Vec<_>>()),
            }),
            format,
        );
    }

    // Names are already cut to length; only CSV escaping is left.
    let mut out = String::new();
    out.push_str("nodeId,nodeName,className,type,detachedness,selfSize,distance\n");
    let _ = writeln!(
        out,
        "{},{},{},{},{},{},{}",
        n.id,
        csv_escape(&n.name),
        csv_escape(&n.class_name),
        csv_escape(&n.node_type),
        n.detachedness,
        n.self_size,
        csv_distance(n.distance),
    );

    let _ = writeln!(
        out,
        "\nRetainers, closest to the root first (weak edges excluded): {}",
        page_summary(report.retainers_total, opts.offset, report.retainers.len())
    );
    if !report.retainers.is_empty() {
        out.push_str("edgeName,edgeType,nodeId,nodeName,type,selfSize,distance\n");
    }
    for r in &report.retainers {
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{}",
            csv_escape(&r.edge_name),
            csv_escape(&r.edge_type),
            r.node_id,
            csv_escape(&r.node_name),
            csv_escape(&r.node_type),
            r.self_size,
            csv_distance(r.distance),
        );
    }
    for (i, r) in report.retainers.iter().enumerate() {
        if let Some(c) = &r.conditional_on {
            let other = if c.role == "table" {
                "the WeakMap's table"
            } else {
                "its key"
            };
            let _ = writeln!(
                out,
                "Retainer {} is conditional: as a WeakMap entry it keeps this value alive only \
                 while {other}, node {}, is alive too.",
                i + 1,
                c.node_id
            );
        }
    }

    out.push('\n');
    match &report.path {
        // No reason given, because the report does not know which applies:
        // only weak edges reach the node, or a WeakMap entry whose key or
        // table is never reached, or nothing at all.
        None => out.push_str("Retaining path: none found.\n"),
        Some(path) if path.is_empty() => out.push_str("Retaining path: this is the root.\n"),
        Some(path) => {
            let _ = writeln!(
                out,
                "Retaining path: shortest from the page's roots, else from the GC roots \
                 ({} edges, weak edges skipped):",
                path.len()
            );
            out.push_str("edgeName,edgeType,nodeId,nodeName,type,selfSize\n");
            for r in path {
                let _ = writeln!(
                    out,
                    "{},{},{},{},{},{}",
                    csv_escape(&r.edge_name),
                    csv_escape(&r.edge_type),
                    r.node_id,
                    csv_escape(&r.node_name),
                    csv_escape(&r.node_type),
                    r.self_size,
                );
            }
            for (i, r) in path.iter().enumerate() {
                if let Some(c) = &r.conditional_on {
                    let other = if c.role == "table" {
                        "the WeakMap's table"
                    } else {
                        "its key"
                    };
                    let _ = writeln!(
                        out,
                        "Step {} is conditional: as a WeakMap entry it keeps its value alive \
                         only while {other}, node {}, is alive too.",
                        i + 1,
                        c.node_id
                    );
                }
            }
        }
    }
    Ok(out)
}

/// Offline `inspect-heapsnapshot-node`: no Chrome connection or daemon, so it
/// works on any saved snapshot.
pub async fn inspect_heapsnapshot_node_offline(
    file_path: &str,
    node_id: u64,
    opts: ListOptions,
    format: OutputFormat,
) -> Result<CommandResult> {
    let file_path_owned = file_path.to_string();
    let report = tokio::task::spawn_blocking(move || -> Result<NodeReport> {
        inspect_node(&load_snapshot(&file_path_owned)?, node_id, &opts)
    })
    .await
    .map_err(|e| anyhow!("Failed to execute blocking snapshot parser: {e}"))??;

    Ok(CommandResult::output(format_node_report(
        &report, &opts, format,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const NODE_TYPES: [&str; 15] = [
        "hidden",
        "array",
        "string",
        "object",
        "code",
        "closure",
        "regexp",
        "number",
        "native",
        "synthetic",
        "concatenated string",
        "sliced string",
        "symbol",
        "bigint",
        "object shape",
    ];
    const EDGE_TYPES: [&str; 7] = [
        "context", "element", "property", "internal", "hidden", "shortcut", "weak",
    ];

    struct TestNode {
        ty: &'static str,
        name: String,
        id: u32,
        size: u64,
        detachedness: u32,
        edges: Vec<(&'static str, String, usize)>,
    }

    /// Builds a snapshot in V8's real schema, so tests go through the same
    /// deserializer and validation as files on disk. Node 0 is the root.
    #[derive(Default)]
    struct TestSnap {
        nodes: Vec<TestNode>,
        locations: Vec<(usize, u32, u32, u32)>,
    }

    impl TestSnap {
        /// A snapshot with a synthetic root (id 1) and a `Window` user root
        /// (id 3) hanging off it, like a real page snapshot.
        fn page() -> (Self, usize) {
            let mut t = TestSnap::default();
            let root = t.node("synthetic", "", 1, 0);
            let window = t.node("object", "Window / https://example.com", 3, 40);
            t.edge(root, "shortcut", "1", window);
            (t, window)
        }

        fn node(&mut self, ty: &'static str, name: &str, id: u32, size: u64) -> usize {
            self.nodes.push(TestNode {
                ty,
                name: name.to_string(),
                id,
                size,
                detachedness: 0,
                edges: Vec::new(),
            });
            self.nodes.len() - 1
        }

        fn edge(&mut self, from: usize, ty: &'static str, name: &str, to: usize) {
            self.nodes[from].edges.push((ty, name.to_string(), to));
        }

        fn json(&self) -> serde_json::Value {
            let mut strings: Vec<String> = Vec::new();
            let mut intern = |s: &str| {
                strings.iter().position(|x| x == s).unwrap_or_else(|| {
                    strings.push(s.to_string());
                    strings.len() - 1
                }) as u32
            };
            let ty_idx = |t: &str| NODE_TYPES.iter().position(|x| *x == t).unwrap() as u32;
            let ety_idx = |t: &str| EDGE_TYPES.iter().position(|x| *x == t).unwrap() as u32;
            let mut nodes: Vec<u64> = Vec::new();
            let mut edges = Vec::new();
            for n in &self.nodes {
                let name = intern(&n.name);
                nodes.extend([
                    ty_idx(n.ty).into(),
                    name.into(),
                    n.id.into(),
                    n.size,
                    n.edges.len() as u64,
                    n.detachedness.into(),
                ]);
                for (ty, name, to) in &n.edges {
                    let name_or_index = if *ty == "element" || *ty == "hidden" {
                        name.parse().unwrap()
                    } else {
                        intern(name)
                    };
                    edges.extend([ety_idx(ty), name_or_index, (*to * 6) as u32]);
                }
            }
            let locations: Vec<u32> = self
                .locations
                .iter()
                .flat_map(|&(ord, s, l, c)| [(ord * 6) as u32, s, l, c])
                .collect();
            json!({
                "snapshot": { "meta": {
                    "node_fields": ["type", "name", "id", "self_size", "edge_count", "detachedness"],
                    "node_types": [NODE_TYPES, "string", "number", "number", "number", "number"],
                    "edge_fields": ["type", "name_or_index", "to_node"],
                    "edge_types": [EDGE_TYPES, "string_or_number", "node"],
                    "location_fields": ["object_index", "script_id", "line", "column"],
                }},
                "nodes": nodes,
                "edges": edges,
                "locations": locations,
                "strings": strings,
            })
        }

        fn snap(&self) -> Snap {
            Snap::new(serde_json::from_value(self.json()).unwrap()).unwrap()
        }

        /// Aggregates as `compare-heapsnapshots` builds them for this
        /// snapshot on its own.
        fn aggregates(&self) -> Aggregates {
            let snap = self.snap();
            let defs = infer_interface_definitions(&snap);
            build_class_aggregates(&snap, &assign_classes(&snap, &defs))
        }

        fn file(&self) -> NamedTempFile {
            let mut file = NamedTempFile::new().unwrap();
            write!(file, "{}", self.json()).unwrap();
            file
        }
    }

    fn key(name: &str) -> ClassKey {
        ClassKey {
            name: name.to_string(),
            location: None,
        }
    }

    impl Classes {
        fn name_of(&self, ord: usize) -> &str {
            &self.names[self.of_node[ord] as usize]
        }
    }

    fn class_of(snap: &Snap, classes: &Classes, id: u32) -> String {
        let ord = (0..snap.node_count).find(|&o| snap.id(o) == id).unwrap();
        classes.name_of(ord).to_string()
    }

    #[test]
    fn test_devtools_class_name_mapping() {
        let cases = [
            ("hidden", "system / Context", "(system)"),
            ("code", "(code for foo)", "(compiled code)"),
            ("closure", "handleClick", "Function"),
            ("regexp", "/a+/g", "RegExp"),
            ("string", "hello world", "(string)"),
            ("concatenated string", "ab", "(concatenated string)"),
            ("object shape", "system / Map", "(object shape)"),
            ("array", "(object elements)", "(array)"),
            ("number", "heap number", "(number)"),
            ("object", "Foo", "Foo"),
            ("object", "<not a tag", "<not>"),
            ("native", "<div id=\"a\" class=\"x\">", "<div>"),
            ("native", "<div>", "<div>"),
            ("native", "Detached <div id=\"a\">", "Detached <div>"),
            ("native", "Detached <span>", "Detached <span>"),
            (
                "native",
                "Detached HTMLDivElement",
                "Detached HTMLDivElement",
            ),
            ("native", "InternalNode", "InternalNode"),
        ];
        for (ty, name, expected) in cases {
            assert_eq!(devtools_class_name(ty, name), expected, "{ty} {name}");
        }
    }

    #[test]
    fn test_classes_collapse_like_devtools() {
        let (mut t, window) = TestSnap::page();
        let f1 = t.node("closure", "onClick", 11, 32);
        let f2 = t.node("closure", "onLoad", 13, 32);
        let d1 = t.node("native", "<div id=\"a\" class=\"x\">", 15, 100);
        let d2 = t.node("native", "<div class=\"y\">", 17, 100);
        let s1 = t.node("string", "hello", 19, 16);
        let s2 = t.node("string", "world", 21, 16);
        let num = t.node("number", "smi number", 23, 0);
        for (i, to) in [f1, f2, d1, d2, s1, s2, num].into_iter().enumerate() {
            t.edge(window, "property", &format!("p{i}"), to);
        }
        let aggs = t.aggregates();
        assert_eq!(aggs.members(&key("Function")).len(), 2);
        assert_eq!(aggs.members(&key("<div>")).len(), 2);
        assert_eq!(aggs.members(&key("(string)")).len(), 2);
        // Zero-size nodes belong to no class, as in DevTools.
        assert!(!aggs.classes.contains_key(&key("(number)")));
        assert!(!aggs.classes.contains_key(&key("(synthetic)")));
        assert!(!aggs.classes.contains_key(&key("onClick")));
    }

    #[test]
    fn test_type_field_without_its_enum_groups_by_name() {
        let (mut t, window) = TestSnap::page();
        let foo = t.node("object", "Foo", 11, 20);
        let bar = t.node("object", "Bar", 13, 20);
        t.edge(window, "property", "foo", foo);
        t.edge(window, "property", "bar", bar);
        // Both share a type value, which names nothing without the enum.
        let mut v = t.json();
        v["snapshot"]["meta"]
            .as_object_mut()
            .unwrap()
            .remove("node_types");
        let snap = load_json(&v).unwrap();
        let aggs = build_class_aggregates(&snap, &assign_classes(&snap, &[]));
        assert_eq!(aggs.members(&key("Foo")), [(11, 20)]);
        assert_eq!(aggs.members(&key("Bar")), [(13, 20)]);
        let node = inspect_node(&snap, 11, &defaults()).unwrap().node;
        assert_eq!(
            (node.class_name.as_str(), node.node_type.as_str()),
            ("Foo", "unknown")
        );
    }

    #[test]
    fn test_object_class_keys_split_by_allocation_site() {
        let (mut t, window) = TestSnap::page();
        let a = t.node("object", "Foo", 11, 20);
        let b = t.node("object", "Foo", 13, 20);
        let c = t.node("object", "Foo", 15, 20);
        t.edge(window, "property", "a", a);
        t.edge(window, "property", "b", b);
        t.edge(window, "property", "c", c);
        t.locations.push((a, 7, 10, 2));
        t.locations.push((b, 7, 10, 2));
        let aggs = t.aggregates();
        let located = ClassKey {
            name: "Foo".into(),
            location: Some((7, 10, 2)),
        };
        assert_eq!(aggs.members(&located), [(11, 20), (13, 20)]);
        assert_eq!(aggs.members(&key("Foo")), [(15, 20)]);
    }

    #[test]
    fn test_detachedness_propagates_like_devtools() {
        let (mut t, window) = TestSnap::page();
        // Attached tree: Window -> document (attached) -> <div>.
        let doc = t.node("native", "HTMLDocument", 11, 10);
        let attached_div = t.node("native", "<div id=\"live\">", 13, 10);
        // Detached tree: a JS array holds a detached <div>, whose <span>
        // child has no state of its own, and whose JS wrapper is an object.
        let holder = t.node("array", "(object elements)", 15, 16);
        let detached_div = t.node("native", "<div id=\"gone\" class=\"x\">", 17, 10);
        let span = t.node("native", "<span>", 19, 10);
        let wrapper = t.node("object", "HTMLDivElement", 21, 10);
        // Reached from both trees: attached wins.
        let shared = t.node("native", "Text", 23, 10);
        // Only reached over a weak edge: stays unknown.
        let weakly = t.node("native", "<p>", 25, 10);
        t.nodes[doc].detachedness = 1;
        t.nodes[detached_div].detachedness = 2;
        t.edge(window, "property", "document", doc);
        t.edge(doc, "element", "0", attached_div);
        t.edge(attached_div, "element", "0", shared);
        t.edge(window, "property", "cache", holder);
        t.edge(holder, "element", "0", detached_div);
        t.edge(detached_div, "element", "0", span);
        t.edge(detached_div, "element", "1", shared);
        t.edge(detached_div, "internal", "wrapper", wrapper);
        t.edge(detached_div, "weak", "w", weakly);

        let snap = t.snap();
        let classes = assign_classes(&snap, &[]);
        assert_eq!(class_of(&snap, &classes, 13), "<div>");
        assert_eq!(class_of(&snap, &classes, 17), "Detached <div>");
        assert_eq!(class_of(&snap, &classes, 19), "Detached <span>");
        assert_eq!(class_of(&snap, &classes, 21), "HTMLDivElement");
        assert_eq!(class_of(&snap, &classes, 23), "Text");
        assert_eq!(class_of(&snap, &classes, 25), "<p>");
        assert_eq!(snap.dom[span], DOM_DETACHED);
        assert_eq!(snap.dom[shared], DOM_ATTACHED);
        assert_eq!(snap.dom[weakly], DOM_UNKNOWN);
        assert_eq!(
            snap.name(detached_div),
            "Detached <div id=\"gone\" class=\"x\">"
        );
    }

    #[test]
    fn test_detachedness_from_name_without_the_field() {
        // Snapshots older than the detachedness field: Blink named the node.
        let snap = Snap::new(
            serde_json::from_value(json!({
                "snapshot": { "meta": {
                    "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                    "node_types": [["native", "synthetic"], "string", "number", "number", "number"],
                }},
                "nodes": [1, 0, 1, 0, 0, 0, 1, 3, 10, 0],
                "strings": ["", "Detached HTMLDivElement"],
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(snap.dom[1], DOM_DETACHED);
        // Not renamed: the name already says it.
        assert_eq!(snap.name(1), "Detached HTMLDivElement");
        assert_eq!(
            assign_classes(&snap, &[]).name_of(1),
            "Detached HTMLDivElement"
        );
    }

    #[test]
    fn test_owned_sizes_move_to_their_owner() {
        let (mut t, window) = TestSnap::page();
        let obj = t.node("object", "Foo", 11, 24);
        let props = t.node("array", "(object properties)", 13, 40);
        let internal = t.node("hidden", "system / PropertyCell", 15, 8);
        let other = t.node("object", "Bar", 17, 24);
        let shared = t.node("array", "(shared elements)", 19, 64);
        t.edge(window, "property", "foo", obj);
        t.edge(window, "property", "bar", other);
        t.edge(obj, "internal", "properties", props);
        t.edge(props, "internal", "cell", internal);
        t.edge(obj, "internal", "elements", shared);
        t.edge(other, "internal", "elements", shared);
        let snap = t.snap();
        assert_eq!(snap.self_size(obj), 24 + 40 + 8);
        assert_eq!(snap.self_size(props), 0);
        assert_eq!(snap.self_size(internal), 0);
        // Two owners: keeps its own size.
        assert_eq!(snap.self_size(shared), 64);
        assert_eq!(snap.self_size(other), 24);
    }

    #[test]
    fn test_interface_definitions_name_plain_objects() {
        let (mut t, window) = TestSnap::page();
        for (i, props) in [
            &["x", "y"][..],
            &["x", "y"],
            &["y", "x"],
            &["w", "x", "y"],
            &["z"],
            &["a,b"],
            &["a,b"],
        ]
        .iter()
        .enumerate()
        {
            let id = 101 + 2 * i as u32;
            let obj = t.node("object", "Object", id, 16);
            t.edge(window, "property", &format!("o{i}"), obj);
            for p in *props {
                t.edge(obj, "property", p, window);
            }
            t.edge(obj, "property", "__proto__", window);
        }
        let snap = t.snap();
        let defs = infer_interface_definitions(&snap);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        // {z} and {w, x, y} occur once, below the minimum of 2.
        assert_eq!(names, ["{x, y}", "{\"a,b\"}"]);
        let classes = assign_classes(&snap, &defs);
        assert_eq!(class_of(&snap, &classes, 101), "{x, y}");
        // Same properties in another order, and a superset, match too.
        assert_eq!(class_of(&snap, &classes, 105), "{x, y}");
        assert_eq!(class_of(&snap, &classes, 107), "{x, y}");
        assert_eq!(class_of(&snap, &classes, 109), "Object");
        assert_eq!(class_of(&snap, &classes, 111), "{\"a,b\"}");
        // Inspecting one object names it as classifying the heap does.
        for id in (101..=113).step_by(2) {
            let node = inspect_node(&snap, id.into(), &defaults()).unwrap().node;
            assert_eq!(node.class_name, class_of(&snap, &classes, id), "{id}");
        }

        // A base snapshot is named with the current snapshot's definitions.
        let (mut base, window) = TestSnap::page();
        let lone = base.node("object", "Object", 201, 16);
        base.edge(window, "property", "o", lone);
        base.edge(lone, "property", "x", window);
        base.edge(lone, "property", "y", window);
        let base = base.snap();
        assert!(infer_interface_definitions(&base).is_empty());
        assert_eq!(
            class_of(&base, &assign_classes(&base, &defs), 201),
            "{x, y}"
        );
    }

    #[test]
    fn test_plain_objects_display_by_their_properties() {
        let (mut t, window) = TestSnap::page();
        let obj = t.node("object", "Object", 11, 16);
        t.edge(window, "property", "holder", obj);
        for p in ["a", "b", "keep", "c", "d", "e", "f", "g", "__proto__"] {
            t.edge(obj, "property", p, window);
        }
        t.edge(obj, "internal", "map", window);
        let empty = t.node("object", "Object", 13, 16);
        t.edge(window, "property", "empty", empty);
        let long = t.node("object", "Object", 15, 16);
        t.edge(window, "property", "long", long);
        for i in 0..40 {
            t.edge(long, "property", &format!("property{i:02}"), window);
        }
        // Other edges, and `__proto__`, at both ends and between the names.
        let mixed = t.node("object", "Object", 17, 16);
        t.edge(window, "property", "mixed", mixed);
        t.edge(mixed, "internal", "map", window);
        t.edge(mixed, "property", "__proto__", window);
        t.edge(mixed, "property", "x", window);
        t.edge(mixed, "element", "0", window);
        t.edge(mixed, "property", "a,b", window);
        t.edge(mixed, "property", "__proto__", window);
        t.edge(mixed, "hidden", "1", window);
        // Two names from the front, then one from the back, fill the name.
        let odd = t.node("object", "Object", 19, 16);
        t.edge(window, "property", "odd", odd);
        let thirty: Vec<String> = ('a'..='i')
            .map(|c| format!("{c}{}", "_".repeat(29)))
            .collect();
        for name in &thirty {
            t.edge(odd, "property", name, window);
        }
        // A name past the limit on its own is still shown.
        let huge = t.node("object", "Object", 21, 16);
        t.edge(window, "property", "huge", huge);
        t.edge(huge, "property", &"q".repeat(150), window);
        t.edge(huge, "property", &"r".repeat(150), window);
        let snap = t.snap();
        assert_eq!(snap.display_name(obj), "{a, b, keep, c, d, e, f, g}");
        assert_eq!(snap.display_name(empty), "{}");
        assert_eq!(
            snap.display_name(long),
            "{property00, property01, property02, property03, …, \
             property36, property37, property38, property39}"
        );
        assert_eq!(snap.display_name(mixed), "{x, \"a,b\"}");
        assert_eq!(
            snap.display_name(odd),
            format!("{{{}, {}, …, {}}}", thirty[0], thirty[1], thirty[8])
        );
        assert_eq!(
            snap.display_name(huge),
            format!("{{{}, …}}", "q".repeat(150))
        );
        // Only the displayed name changes; the class stays "Object".
        assert_eq!(assign_classes(&snap, &[]).name_of(obj), "Object");
        assert_eq!(snap.display_name(window), "Window / https://example.com");
    }

    #[test]
    fn test_plain_object_names_place_empty_names_like_devtools() {
        // What DevTools' `#plainObjectName` prints for each, run on its code.
        let (q, r) = ("q".repeat(150), "r".repeat(150));
        let cases: [(&[&str], String); 6] = [
            // An empty name first on its side adds nothing...
            (&["", "x", "y"], "{x, y}".into()),
            (&["x", "y", ""], "{x, y}".into()),
            (&["x", ""], "{x}".into()),
            // ...but one after a name keeps its separator,
            (&["x", "", "y"], "{x, , y}".into()),
            // and the back names follow a separator even after a bare brace.
            (&["", "x"], "{, x}".into()),
            // The 100-unit limit waits for the front to show a name.
            (&["", &q, &r], format!("{{{q}, {r}}}")),
        ];
        let (mut t, window) = TestSnap::page();
        let mut objs = Vec::new();
        for (i, (props, _)) in cases.iter().enumerate() {
            let obj = t.node("object", "Object", 101 + 2 * i as u32, 16);
            t.edge(window, "property", &format!("o{i}"), obj);
            for p in *props {
                t.edge(obj, "property", p, window);
            }
            objs.push(obj);
        }
        let snap = t.snap();
        for (obj, (props, expected)) in objs.into_iter().zip(&cases) {
            assert_eq!(snap.display_name(obj), *expected, "{props:?}");
        }
    }

    #[test]
    fn test_utf16_ordering_matches_js() {
        // U+FF5E sorts after U+1F600 in UTF-8 byte order but before it in
        // JS's UTF-16 order.
        assert!(cmp_utf16("\u{1F600}", "\u{FF5E}").is_lt());
        assert!("\u{1F600}" > "\u{FF5E}");
        assert_eq!(format_property_name("plain"), "plain");
        assert_eq!(format_property_name("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn test_pagination_bounds_and_summary() {
        let opts = |offset, limit| ListOptions {
            limit,
            offset,
            max_name_length: 100,
        };
        assert_eq!(page_bounds(912, &opts(0, 50)), 0..50);
        assert_eq!(page_bounds(912, &opts(900, 50)), 900..912);
        assert_eq!(page_bounds(10, &opts(20, 50)), 10..10);
        assert_eq!(
            page_summary(912, 0, 50),
            "showing 1-50 of 912; next page: --offset 50"
        );
        assert_eq!(page_summary(912, 900, 12), "showing 901-912 of 912");
        assert_eq!(
            page_summary(10, 20, 0),
            "showing 0 of 10 (--offset 20 is past the end)"
        );
        assert_eq!(page_summary(0, 0, 0), "none");
    }

    #[derive(clap::Parser)]
    struct Cmd {
        #[command(flatten)]
        list: ListOptions,
    }

    /// The options a command gets with no flags.
    fn defaults() -> ListOptions {
        use clap::Parser;
        Cmd::parse_from(["t"]).list
    }

    #[test]
    fn test_list_options_flags() {
        use clap::Parser;
        let d = defaults();
        assert_eq!((d.limit, d.offset, d.max_name_length), (50, 0, 100));
        let parsed = Cmd::try_parse_from([
            "t",
            "--limit",
            "5",
            "--offset",
            "10",
            "--max-name-length",
            "8",
        ])
        .unwrap()
        .list;
        assert_eq!(
            (parsed.limit, parsed.offset, parsed.max_name_length),
            (5, 10, 8)
        );
        assert!(Cmd::try_parse_from(["t", "--limit", "0"]).is_err());
        assert!(Cmd::try_parse_from(["t", "--max-name-length", "0"]).is_err());
    }

    #[test]
    fn test_edges_are_indexed_per_node() {
        let (mut t, window) = TestSnap::page();
        let a = t.node("object", "A", 11, 8);
        let b = t.node("object", "B", 13, 8);
        t.edge(window, "property", "a", a);
        t.edge(window, "element", "7", b);
        t.edge(b, "context", "ctx", a);
        let snap = t.snap();
        assert_eq!(snap.first_edge, vec![0, 3, 9, 9, 12]);
        let window_edges: Vec<(String, &str, usize)> = snap
            .edge_range(window)
            .map(|e| {
                (
                    snap.edge_name(e).into_owned(),
                    snap.edge_type_name(e),
                    snap.edge_to(e),
                )
            })
            .collect();
        assert_eq!(
            window_edges,
            [
                ("a".to_string(), "property", a),
                ("7".to_string(), "element", b)
            ]
        );
        // Node `a` has no edges, so the edge after it belongs to `b`.
        assert_eq!(snap.edge_source(9), b);
        assert_eq!(snap.edge_source(3), window);
    }

    #[test]
    fn test_rejects_corrupt_edges() {
        let (mut t, window) = TestSnap::page();
        let a = t.node("object", "A", 11, 8);
        t.edge(window, "property", "a", a);
        let mut v = t.json();
        // Edge count says 2 edges, the edges array holds 1.
        v["nodes"][4] = json!(2);
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(err.to_string().contains("edge_count adds up to"), "{err}");

        let mut v = t.json();
        v["edges"][2] = json!(7);
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(err.to_string().contains("invalid node index 7"), "{err}");
    }

    #[test]
    fn test_rejects_edge_types_outside_the_enum() {
        let (mut t, window) = TestSnap::page();
        let a = t.node("object", "A", 11, 8);
        t.edge(window, "weak", "a", a);
        // As declared, the weak edge keeps nothing alive.
        assert!(inspect_node(&t.snap(), 11, &defaults())
            .unwrap()
            .path
            .is_none());
        // Its type is field 3, after the root's edge. 7 is none of the seven
        // edge types, and must not pass for an edge that retains.
        let v = with_fields(t.json(), "edges", &[(3, json!(7))]);
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(
            err.to_string()
                .contains("edge 1 has type 7, but the snapshot declares 7 edge types"),
            "{err}"
        );
    }

    #[test]
    fn test_rejects_truncated_nodes_and_bad_strings() {
        let raw = |nodes: serde_json::Value| -> RawSnapshot {
            serde_json::from_value(json!({
                "snapshot": { "meta": { "node_fields": ["id", "name", "self_size"] } },
                "nodes": nodes,
                "strings": ["Map"],
            }))
            .unwrap()
        };
        let err = Snap::new(raw(json!([1, 0, 100, 2]))).err().unwrap();
        assert!(
            err.to_string().contains("not a multiple of node_size"),
            "{err}"
        );
        let err = Snap::new(raw(json!([1, 5, 100]))).err().unwrap();
        assert!(err.to_string().contains("out of bounds"), "{err}");
    }

    /// The native context's legacy map shortcut is skipped for distances, so
    /// the application path decides them. Other edges from that context, and
    /// the same edge name from another source, still set distances.
    #[test]
    fn test_sloppy_function_map_is_not_a_distance_shortcut() {
        let (mut t, window) = TestSnap::page();
        let native = t.node("hidden", "system / NativeContext", 5, 0);
        let a = t.node("object", "A", 11, 8);
        let b = t.node("object", "B", 13, 8);
        let target = t.node("object", "Leak", 15, 64);
        let other = t.node("object", "Other", 17, 64);
        t.edge(window, "internal", "context", native);
        t.edge(native, "internal", "sloppy_function_map", target);
        t.edge(native, "internal", "other", other);
        t.edge(window, "property", "a", a);
        t.edge(a, "property", "b", b);
        t.edge(b, "property", "leak", target);
        let snap = t.snap();
        let (dist, _, _, _) = bfs_from_root(&snap);
        assert_eq!(dist[target], 4);
        assert_eq!(dist[other], 3);

        t.nodes[native].edges[0].1 = "kept".to_string();
        let snap = t.snap();
        let (dist, _, _, _) = bfs_from_root(&snap);
        assert_eq!(dist[target], 3);
    }

    /// root -> Window -> list -> [3] -> target, plus a shorter route through
    /// the GC roots, a weak retainer, and a node only reached weakly.
    fn retention_fixture() -> TestSnap {
        let (mut t, window) = TestSnap::page();
        let gc_roots = t.node("synthetic", "(GC roots)", 5, 0);
        let list = t.node("object", "Array", 11, 16);
        let target = t.node("object", "Leak", 13, 64);
        let other = t.node("object", "Holder", 15, 16);
        let weak_holder = t.node("object", "WeakRef", 17, 16);
        let only_weak = t.node("object", "Orphan", 19, 16);
        t.edge(0, "element", "2", gc_roots);
        t.edge(gc_roots, "element", "0", target);
        t.edge(window, "property", "list", list);
        t.edge(window, "property", "other", other);
        t.edge(window, "property", "weakHolder", weak_holder);
        t.edge(list, "element", "3", target);
        t.edge(other, "property", "leak", target);
        t.edge(weak_holder, "weak", "target", target);
        t.edge(weak_holder, "weak", "orphan", only_weak);
        t
    }

    #[test]
    fn test_retainers_and_retaining_path() {
        let t = retention_fixture();
        let snap = t.snap();
        let report = inspect_node(&snap, 13, &defaults()).unwrap();
        assert_eq!(report.node.class_name, "Leak");
        assert_eq!(report.node.distance, Some(3));
        // Weak edges are not retainers. Page-root retainers rank before the
        // closer GC-root group, which stays in the list rather than being
        // dropped by a small limit.
        let retainers: Vec<(&str, &str, u32)> = report
            .retainers
            .iter()
            .map(|r| (r.edge_name.as_str(), r.edge_type.as_str(), r.node_id))
            .collect();
        assert_eq!(report.retainers_total, 3);
        assert_eq!(
            retainers,
            [
                ("3", "element", 11),
                ("leak", "property", 15),
                ("0", "element", 5)
            ]
        );
        let one = inspect_node(
            &snap,
            13,
            &ListOptions {
                limit: 1,
                ..defaults()
            },
        )
        .unwrap();
        assert_eq!(one.retainers_total, 3);
        assert_eq!(one.retainers[0].node_id, 11);
        // The path runs through the page's Window, not the GC-root shortcut,
        // because DevTools expands user roots first.
        let path: Vec<(&str, u32)> = report
            .path
            .as_ref()
            .unwrap()
            .iter()
            .map(|r| (r.edge_name.as_str(), r.node_id))
            .collect();
        assert_eq!(path, [("1", 3), ("list", 11), ("3", 13)]);

        let report = inspect_node(&snap, 19, &defaults()).unwrap();
        assert_eq!(report.retainers_total, 0);
        assert!(report.path.is_none());
        assert_eq!(report.node.distance, None);

        let report = inspect_node(&snap, 1, &defaults()).unwrap();
        assert_eq!(report.path.unwrap().len(), 0);

        assert!(inspect_node(&snap, 999, &defaults()).is_err());
    }

    #[test]
    fn test_inspect_pages_retainers_and_truncates_names() {
        let (mut t, window) = TestSnap::page();
        let long = "x".repeat(500);
        let target = t.node("string", &long, 11, 1000);
        for i in 0..5u32 {
            let holder = t.node("object", &format!("Holder{i}"), 21 + 2 * i, 16);
            t.edge(window, "property", &format!("h{i}"), holder);
            t.edge(holder, "property", "s", target);
        }
        let opts = ListOptions {
            limit: 2,
            offset: 2,
            max_name_length: 10,
        };
        let snap = t.snap();
        let report = inspect_node(&snap, 11, &opts).unwrap();
        assert_eq!(report.node.name, format!("{}…", "x".repeat(10)));
        assert_eq!(report.node.class_name, "(string)");
        assert_eq!(report.retainers_total, 5);
        let ids: Vec<u32> = report.retainers.iter().map(|r| r.node_id).collect();
        assert_eq!(ids, [25, 27]);

        let text = format_node_report(&report, &opts, OutputFormat::Text).unwrap();
        assert!(text.starts_with(
            "nodeId,nodeName,className,type,detachedness,selfSize,distance\n\
             11,xxxxxxxxxx…,(string),string,unknown,1000,3\n"
        ));
        assert!(text.contains("weak edges excluded): showing 3-4 of 5; next page: --offset 4\n"));
        assert!(text.contains("\ns,property,25,Holder2,object,16,2\n"));
        assert!(text.contains("(3 edges, weak edges skipped):\n"));
        assert!(!text.contains(&"x".repeat(11)));

        let json: serde_json::Value =
            serde_json::from_str(&format_node_report(&report, &opts, OutputFormat::Json).unwrap())
                .unwrap();
        assert_eq!(json["nodeId"], 11);
        assert_eq!(json["selfSize"], 1000);
        assert_eq!(json["retainers"]["total"], 5);
        assert_eq!(json["retainers"]["items"][0]["nodeId"], 25);
        assert_eq!(json["retainingPath"][2]["nodeId"], 11);
    }

    #[test]
    fn test_inspect_csv_escapes_names() {
        let (mut t, window) = TestSnap::page();
        let obj = t.node("object", "My,\"Class\"", 11, 8);
        t.edge(window, "property", "a,b", obj);
        let snap = t.snap();
        let report = inspect_node(&snap, 11, &defaults()).unwrap();
        let text = format_node_report(&report, &defaults(), OutputFormat::Text).unwrap();
        assert!(
            text.contains("\n11,\"My,\"\"Class\"\"\",\"My,\"\"Class\"\"\",object,unknown,8,2\n")
        );
        assert!(text.contains("\n\"a,b\",property,3,Window"));
        assert!(text.contains("\n\"a,b\",property,11,"));
    }

    /// A snapshot can declare a type label containing a comma: it must stay
    /// one CSV cell in the node, retainer and retaining-path sections.
    #[test]
    fn test_inspect_csv_escapes_type_labels() {
        let row = |name: &str, node_id| EdgeRow {
            edge_name: name.to_string(),
            edge_type: "future,type".into(),
            node_id,
            node_name: "holder".into(),
            node_type: "node,type".into(),
            self_size: 4,
            distance: Some(1),
            conditional_on: None,
        };
        let report = NodeReport {
            node: NodeInfo {
                id: 1,
                name: "target".into(),
                class_name: "Class".into(),
                node_type: "node,type".into(),
                detachedness: "unknown",
                self_size: 8,
                distance: Some(2),
            },
            retainers_total: 1,
            retainers: vec![row("a,b", 2)],
            path: Some(vec![row("p,q", 2)]),
        };
        let text = format_node_report(&report, &defaults(), OutputFormat::Text).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "1,target,Class,\"node,type\",unknown,8,2");
        assert!(lines.contains(&"\"a,b\",\"future,type\",2,holder,\"node,type\",4,1"));
        assert!(lines.contains(&"\"p,q\",\"future,type\",2,holder,\"node,type\",4"));
    }

    /// Nodes of one type hanging off Window, for diff tests.
    fn hanging(ty: &'static str, items: &[(u32, &str, u64)]) -> TestSnap {
        let (mut t, window) = TestSnap::page();
        for (i, &(id, name, size)) in items.iter().enumerate() {
            let node = t.node(ty, name, id, size);
            t.edge(window, "property", &format!("p{i}"), node);
        }
        t
    }

    /// Named objects hanging off Window.
    fn objects(items: &[(u32, &str, u64)]) -> Aggregates {
        hanging("object", items).aggregates()
    }

    #[test]
    fn test_diff_added_removed_and_retained() {
        let base = objects(&[(11, "Map", 100), (13, "Map", 100), (15, "Set", 50)]);
        let current = objects(&[(11, "Map", 100), (17, "Map", 150), (19, "Window", 500)]);
        let diffs = diff_snapshots(&base, &current).unwrap();
        let rows: Vec<(&str, usize, usize, i64)> = diffs
            .iter()
            .map(|d| {
                (
                    d.key.name.as_str(),
                    d.added_count,
                    d.removed_count,
                    d.size_delta(),
                )
            })
            .collect();
        // The page's own Window object (id 3) is unchanged, so no row.
        assert_eq!(
            rows,
            [("Window", 1, 0, 500), ("Map", 1, 1, 50), ("Set", 0, 1, -50)]
        );
        let detail = compute_class_node_detail(&base, &current, &key("Map"));
        assert_eq!(
            detail,
            [
                DiffObject {
                    added: true,
                    id: 17,
                    self_size: 150
                },
                DiffObject {
                    added: false,
                    id: 13,
                    self_size: 100
                },
            ]
        );
        let gone = compute_class_node_detail(&base, &current, &key("Set"));
        assert_eq!(
            gone,
            [DiffObject {
                added: false,
                id: 15,
                self_size: 50
            }]
        );
    }

    #[test]
    fn test_diff_filters_unchanged_classes_and_groups_strings() {
        let same = objects(&[(11, "Map", 100)]);
        assert!(diff_snapshots(&same, &same).unwrap().is_empty());

        let strings = |items: &[(u32, &str, u64)]| hanging("string", items).aggregates();
        let diffs = diff_snapshots(
            &strings(&[(11, "hello", 16), (13, "world", 16)]),
            &strings(&[(11, "hello", 16), (15, "foo", 16)]),
        )
        .unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].key.name, "(string)");
        assert_eq!((diffs[0].added_count, diffs[0].removed_count), (1, 1));
    }

    #[test]
    fn test_snapshots_share_no_node_ids() {
        let a = objects(&[(11, "Map", 100)]);
        let b = objects(&[(13, "Map", 100)]);
        // Window (id 3) is in both, so build disjoint aggregates by hand.
        let only = |class: &str, id: u32| Aggregates {
            classes: HashMap::from([(key(class), vec![(id, 100)])]),
            ..Default::default()
        };
        assert!(snapshots_share_no_node_ids(
            &only("Map", 1),
            &only("Map", 2)
        ));
        assert!(!snapshots_share_no_node_ids(&a, &b));
        // An id that changed class still counts as shared.
        assert!(!snapshots_share_no_node_ids(
            &only("Map", 1),
            &only("Set", 1)
        ));
        assert!(!snapshots_share_no_node_ids(
            &Aggregates::default(),
            &only("Map", 1)
        ));
    }

    #[test]
    fn test_summary_pages_and_detail_shares_indices() {
        let base = objects(&[(11, "Map", 100), (13, "Set", 50)]);
        let current = objects(&[(15, "Window", 500), (17, "Map", 150), (19, "Map", 90)]);
        let diffs = diff_snapshots(&base, &current).unwrap();
        let opts = ListOptions {
            limit: 1,
            offset: 1,
            max_name_length: 2,
        };
        let summary = format_class_diff_summary(&diffs, &opts, OutputFormat::Text).unwrap();
        assert_eq!(
            summary,
            "Classes with changes, largest sizeDelta first: showing 2-2 of 3; next page: --offset 2\n\
             idx,className,location,addedCount,removedCount,countDelta,addedSize,removedSize,sizeDelta\n\
             1,Ma…,,2,1,1,240,100,140\n"
        );
        let json: serde_json::Value = serde_json::from_str(
            &format_class_diff_summary(&diffs, &opts, OutputFormat::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(json["totalClasses"], 3);
        assert_eq!(json["diffs"][0]["idx"], 1);

        let objects = compute_class_node_detail(&base, &current, &diffs[1].key);
        let opts = ListOptions {
            limit: 2,
            ..defaults()
        };
        let detail =
            format_class_diff_detail(1, &diffs[1], &objects, &opts, OutputFormat::Text).unwrap();
        assert_eq!(
            detail,
            "idx:1,className:Map,location:,addedCount:2,removedCount:1,countDelta:1,addedSize:240,removedSize:100,sizeDelta:140\n\
             \nObjects, largest selfSize first: showing 1-2 of 3; next page: --offset 2\n\
             op,nodeId,selfSize\n+,17,150\n-,11,100\n"
        );
    }

    #[test]
    fn test_compare_offline_end_to_end_via_files() {
        let file = |items: &[(u32, &str, u64)]| hanging("object", items).file();
        let base = file(&[(11, "Map", 100), (13, "Set", 50)]);
        let current = file(&[(15, "Window", 500), (17, "Map", 150)]);
        let (base, current) = (
            base.path().to_str().unwrap(),
            current.path().to_str().unwrap(),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let run = |idx| {
            rt.block_on(compare_heapsnapshots_offline(
                base,
                current,
                idx,
                defaults(),
                OutputFormat::Text,
            ))
        };

        let summary = run(None).unwrap().output;
        assert!(summary.contains("showing 1-3 of 3\n"), "{summary}");
        assert!(summary.contains("\n0,Window,,1,0,1,500,0,500\n"));
        assert!(summary.contains("\n1,Map,"));

        let detail = run(Some(0)).unwrap().output;
        assert!(detail.contains("className:Window"));
        assert!(detail.contains("\n+,15,500\n"));

        let err = run(Some(99)).err().unwrap();
        let msg = err.to_string();
        assert!(msg.contains("Invalid classIndex: 99"), "{msg}");
        // A typed InvalidInput error gives main a stable exit code.
        let cli_err = err.downcast_ref::<CliError>().expect("expected a CliError");
        assert_eq!(cli_err.code(), ErrorCode::InvalidInput);
    }

    #[test]
    fn test_inspect_offline_end_to_end_via_file() {
        let file = retention_fixture().file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt
            .block_on(inspect_heapsnapshot_node_offline(
                file.path().to_str().unwrap(),
                13,
                defaults(),
                OutputFormat::Text,
            ))
            .unwrap()
            .output;
        assert!(out.starts_with(
            "nodeId,nodeName,className,type,detachedness,selfSize,distance\n13,Leak,Leak,object,unknown,64,3\n"
        ));
        // The fixture's GC-root route is shorter, so the heading says which
        // roots the path prefers rather than calling it the shortest.
        assert!(out.contains(
            "\nRetaining path: shortest from the page's roots, else from the GC roots \
             (3 edges, weak edges skipped):\n\
             edgeName,edgeType,nodeId,nodeName,type,selfSize\n\
             1,shortcut,3,Window / https://example.com,object,40\n"
        ));
    }

    /// The key and the table of a WeakMap entry each have an edge to the
    /// value, named `value`, `key_hops` and `table_hops` objects below
    /// Window.
    fn weak_map_fixture(key_hops: usize, table_hops: usize, value: &str) -> TestSnap {
        let (mut t, window) = TestSnap::page();
        let chain = |t: &mut TestSnap, hops: usize, first_id: u32| {
            let mut at = window;
            for i in 0..hops {
                let next = t.node("object", "Link", first_id + 2 * i as u32, 16);
                t.edge(at, "property", "next", next);
                at = next;
            }
            at
        };
        let key_holder = chain(&mut t, key_hops, 101);
        let wm_holder = chain(&mut t, table_hops, 201);
        let key = t.node("object", "Key", 11, 16);
        let wm = t.node("object", "WeakMap", 13, 24);
        let table = t.node("array", "", 15, 64);
        let entry =
            format!("part of key (Key @11) -> value ({value} @17) pair in WeakMap (table @15)");
        let value = t.node("object", value, 17, 16);
        t.edge(key_holder, "property", "key", key);
        t.edge(wm_holder, "property", "wm", wm);
        t.edge(wm, "internal", "table", table);
        t.edge(key, "internal", &format!("1 / {entry}"), value);
        t.edge(table, "internal", &format!("3 / {entry}"), value);
        t
    }

    #[test]
    fn test_weak_map_value_waits_for_its_key_and_table() {
        // Key 3 hops below Window, table 2 (Window -> wm -> table): DevTools
        // waits for the key's edge, so the value is 5 from the root, not 4.
        let snap = weak_map_fixture(2, 0, "Val").snap();
        let report = inspect_node(&snap, 17, &defaults()).unwrap();
        assert_eq!(report.node.distance, Some(5));
        let path = report.path.as_ref().unwrap();
        let ids: Vec<u32> = path.iter().map(|r| r.node_id).collect();
        assert_eq!(ids, [3, 101, 103, 11, 17]);
        assert!(path[..4].iter().all(|r| r.conditional_on.is_none()));
        let c = path[4].conditional_on.as_ref().unwrap();
        assert_eq!((c.role, c.node_id), ("table", 15));
        // Both halves are still listed as retainers, and each says what else
        // has to stay alive for it to retain the value.
        assert_eq!(report.retainers_total, 2);
        let conds: Vec<_> = report
            .retainers
            .iter()
            .map(|r| r.conditional_on.as_ref().map(|c| (c.role, c.node_id)))
            .collect();
        assert_eq!(conds, [Some(("key", 11)), Some(("table", 15))]);

        let text = format_node_report(&report, &defaults(), OutputFormat::Text).unwrap();
        assert!(text.contains(
            "Retainer 1 is conditional: as a WeakMap entry it keeps this value alive only \
             while its key, node 11, is alive too.\n"
        ));
        assert!(text.contains(
            "Retainer 2 is conditional: as a WeakMap entry it keeps this value alive only \
             while the WeakMap's table, node 15, is alive too.\n"
        ));
        assert!(text.ends_with(
            "\nStep 5 is conditional: as a WeakMap entry it keeps its value alive only \
             while the WeakMap's table, node 15, is alive too.\n"
        ));
        let json: serde_json::Value = serde_json::from_str(
            &format_node_report(&report, &defaults(), OutputFormat::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(
            json["retainingPath"][4]["conditionalOn"],
            json!({ "role": "table", "nodeId": 15 })
        );
        assert!(json["retainingPath"][3].get("conditionalOn").is_none());
        assert_eq!(
            json["retainers"]["items"][0]["conditionalOn"],
            json!({ "role": "key", "nodeId": 11 })
        );
        assert_eq!(
            json["retainers"]["items"][1]["conditionalOn"],
            json!({ "role": "table", "nodeId": 15 })
        );

        // The value's name, which both edge names embed, may span lines; the
        // value still waits for the key rather than taking the table's
        // shorter route.
        let snap = weak_map_fixture(2, 0, "line one\nline two").snap();
        let report = inspect_node(&snap, 17, &defaults()).unwrap();
        assert_eq!(report.node.distance, Some(5));
        let path = report.path.unwrap();
        let ids: Vec<u32> = path.iter().map(|r| r.node_id).collect();
        assert_eq!(ids, [3, 101, 103, 11, 17]);
        let c = path[4].conditional_on.as_ref().unwrap();
        assert_eq!((c.role, c.node_id), ("table", 15));

        // The other way round (key 1 hop, table 4: Window -> 2 links -> wm
        // -> table), the table's edge sets the distance and the key is the
        // condition.
        let snap = weak_map_fixture(0, 2, "Val").snap();
        let report = inspect_node(&snap, 17, &defaults()).unwrap();
        assert_eq!(report.node.distance, Some(6));
        let path = report.path.unwrap();
        assert_eq!(
            path[5].edge_name,
            "3 / part of key (Key @11) -> value (Val @17) pair in WeakMap (table @15)"
        );
        let c = path[5].conditional_on.as_ref().unwrap();
        assert_eq!((c.role, c.node_id), ("key", 11));

        // With no way to the key, the table alone does not retain the value.
        let mut t = weak_map_fixture(0, 0, "Val");
        t.nodes[1].edges.retain(|(_, name, _)| name != "key");
        let opts = ListOptions {
            limit: 1,
            ..defaults()
        };
        let report = inspect_node(&t.snap(), 17, &opts).unwrap();
        assert_eq!(report.node.distance, None);
        assert!(report.path.is_none());
        // The table's internal edge still reaches it, so the report does not
        // claim that only weak edges do. With just that one retainer shown, it
        // still says the missing key is required.
        assert_eq!(report.retainers_total, 2);
        assert_eq!(report.retainers.len(), 1);
        assert_eq!(
            report.retainers[0]
                .conditional_on
                .as_ref()
                .map(|c| (c.role, c.node_id)),
            Some(("key", 11))
        );
        let text = format_node_report(&report, &opts, OutputFormat::Text).unwrap();
        assert!(
            text.contains(
                "Retainer 1 is conditional: as a WeakMap entry it keeps this value alive only \
                 while its key, node 11, is alive too.\n"
            ),
            "{text}"
        );
        let json: serde_json::Value =
            serde_json::from_str(&format_node_report(&report, &opts, OutputFormat::Json).unwrap())
                .unwrap();
        assert_eq!(
            json["retainers"]["items"][0]["conditionalOn"],
            json!({ "role": "key", "nodeId": 11 })
        );
        assert!(
            text.ends_with("\n\nRetaining path: none found.\n"),
            "{text}"
        );
    }

    #[test]
    fn test_weak_map_edge_names_parse() {
        let name = "12 / part of key (Key @7) -> value (Val @9) pair in WeakMap (table @5)";
        assert_eq!(
            weak_map_pair(name),
            Some(" / part of key (Key @7) -> value (Val @9) pair in WeakMap (table @5)")
        );
        assert_eq!(weak_map_pair_ids(name), Some((7, 5)));
        assert_eq!(
            weak_map_pair_ids(
                "1 / part of key (a) -> value (b @1) @2) -> value (c @3) pair in WeakMap (table @4)"
            ),
            Some((2, 4))
        );
        // Names may contain the separators, and line breaks.
        for name in [
            "1 / part of key (a) -> value (b @1) @2) -> value (c @3) pair in WeakMap (table @4)",
            "1 / part of key ( @2) -> value ( @3) pair in WeakMap (table @4)",
            "1 / part of key (a\nb @2) -> value (c\r\n\u{2028}d @3) pair in WeakMap (table @4)",
        ] {
            assert!(weak_map_pair(name).is_some(), "{name}");
        }
        for not_a_pair in [
            "part of key -> value pair in ephemeron table",
            " / part of key (Key @7) -> value (Val @9) pair in WeakMap (table @5)",
            "1 / part of key (Key @7) -> value (Val @9) pair in WeakMap (table @)",
            "1 / part of key (Key @x) -> value (Val @9) pair in WeakMap (table @5)",
            "1 / part of key (Key @7) -> value (Val @9) pair in WeakMap (table @5) ",
            "1 / part of key (Key @7\n) -> value (Val @9) pair in WeakMap (table @5)",
            "1 / part of key (Key @7) -> value (Val @9\n) pair in WeakMap (table @5)",
            "1 / part of key (Key @7) -> value (Val @9) pair in WeakMap\n(table @5)",
            "map",
        ] {
            assert!(weak_map_pair(not_a_pair).is_none(), "{not_a_pair}");
        }
    }

    #[test]
    fn test_sizes_past_32_bits_load_from_a_file() {
        // A 5 GiB ArrayBuffer backing store, and a size that is exactly the
        // `WIDE` marker.
        let (mut t, window) = TestSnap::page();
        let buffer = t.node("object", "ArrayBuffer", 11, 80);
        let store = t.node("native", "system / JSArrayBufferData", 13, 5 << 30);
        let edge = t.node("native", "Edge", 15, u32::MAX.into());
        t.edge(window, "property", "buf", buffer);
        t.edge(buffer, "internal", "backing_store", store);
        t.edge(window, "property", "edge", edge);
        let file = t.file();
        let snap = load_snapshot(file.path().to_str().unwrap()).unwrap();
        assert_eq!(snap.self_size(store), 5 << 30);
        assert_eq!(snap.self_size(edge), u64::from(u32::MAX));
        assert_eq!(snap.self_size(buffer), 80);
        let report = inspect_node(&snap, 13, &defaults()).unwrap();
        assert_eq!(report.node.self_size, 5 << 30);

        let base = TestSnap::page().0.file();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let run = |idx| {
            rt.block_on(compare_heapsnapshots_offline(
                base.path().to_str().unwrap(),
                file.path().to_str().unwrap(),
                idx,
                defaults(),
                OutputFormat::Text,
            ))
            .unwrap()
            .output
        };
        let summary = run(None);
        assert!(
            summary.contains("\n0,system / JSArrayBufferData,,1,0,1,5368709120,0,5368709120\n"),
            "{summary}"
        );
        assert!(summary.contains("\n1,Edge,,1,0,1,4294967295,0,4294967295\n"));
        assert!(run(Some(0)).ends_with("\n+,13,5368709120\n"));

        // Only `self_size` may be that wide.
        let mut v = t.json();
        v["nodes"][2] = json!(1u64 << 32);
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(
            err.to_string().contains("node field 'id' holds 4294967296"),
            "{err}"
        );
    }

    #[test]
    fn test_owned_sizes_add_up_past_32_bits() {
        // A 64-byte owner of two 2 GiB arrays: 4 GiB + 64 bytes in all.
        let (mut t, window) = TestSnap::page();
        let owner = t.node("object", "Big", 11, 64);
        let elements = t.node("array", "(object elements)", 13, 2 << 30);
        let properties = t.node("array", "(object properties)", 15, 2 << 30);
        t.edge(window, "property", "big", owner);
        t.edge(owner, "internal", "elements", elements);
        t.edge(owner, "internal", "properties", properties);
        let snap = t.snap();
        assert_eq!(snap.self_size(owner), 4_294_967_360);
        assert_eq!(
            (snap.self_size(elements), snap.self_size(properties)),
            (0, 0)
        );

        let base = TestSnap::page().0.aggregates();
        let current = t.aggregates();
        let diffs = diff_snapshots(&base, &current).unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].added_size, 4_294_967_360);
        assert_eq!(
            compute_class_node_detail(&base, &current, &diffs[0].key)[0].self_size,
            4_294_967_360
        );
        let summary = format_class_diff_summary(&diffs, &defaults(), OutputFormat::Text).unwrap();
        assert!(
            summary.ends_with("\n0,Big,,1,0,1,4294967360,0,4294967360\n"),
            "{summary}"
        );
    }

    #[test]
    fn test_rejects_owned_sizes_past_64_bits() {
        // A 1-byte owner of an array as large as a u64 can say.
        let (mut t, window) = TestSnap::page();
        let owner = t.node("object", "Big", 11, 1);
        let elements = t.node("array", "(object elements)", 13, u64::MAX);
        t.edge(window, "property", "big", owner);
        t.edge(owner, "internal", "elements", elements);
        let err = Snap::new(serde_json::from_value(t.json()).unwrap())
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("node 11 and the objects it owns add up to 2^64 bytes or more"),
            "{err}"
        );
        // One byte less fits, and moves whole.
        t.nodes[elements].size = u64::MAX - 1;
        let snap = t.snap();
        assert_eq!(
            (snap.self_size(owner), snap.self_size(elements)),
            (u64::MAX, 0)
        );
    }

    #[test]
    fn test_rejects_class_totals_past_63_bits() {
        // Two objects of one class whose sizes add up past a u64.
        let page = TestSnap::page().0.aggregates();
        let foo = objects(&[(11, "Foo", u64::MAX), (13, "Foo", 1)]);
        let err = diff_snapshots(&page, &foo).err().unwrap();
        assert!(
            err.to_string().contains(
                "the current snapshot's added objects of class 'Foo' add up to 2^63 bytes or more"
            ),
            "{err}"
        );
        let err = diff_snapshots(&foo, &page).err().unwrap();
        assert!(
            err.to_string()
                .contains("the base snapshot's removed objects of class 'Foo' add up to"),
            "{err}"
        );
    }

    #[test]
    fn test_size_delta_keeps_its_sign() {
        // 2^63 bytes would read as a negative i64.
        let page = TestSnap::page().0.aggregates();
        let err = diff_snapshots(&page, &objects(&[(11, "Foo", 1 << 63)]))
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("add up to 2^63 bytes or more"),
            "{err}"
        );
        // One byte less sorts and prints with its sign, either way round.
        let big = objects(&[(11, "Foo", (1 << 63) - 1), (13, "Bar", 8)]);
        let deltas = |diffs: &[HeapSnapshotClassDiff]| -> Vec<(String, i64)> {
            diffs
                .iter()
                .map(|d| (d.key.name.clone(), d.size_delta()))
                .collect()
        };
        let grew = diff_snapshots(&page, &big).unwrap();
        assert_eq!(deltas(&grew), [("Foo".into(), i64::MAX), ("Bar".into(), 8)]);
        let summary = format_class_diff_summary(&grew, &defaults(), OutputFormat::Text).unwrap();
        assert!(
            summary.contains("\n0,Foo,,1,0,1,9223372036854775807,0,9223372036854775807\n"),
            "{summary}"
        );
        let shrank = diff_snapshots(&big, &page).unwrap();
        assert_eq!(
            deltas(&shrank),
            [("Bar".into(), -8), ("Foo".into(), -i64::MAX)]
        );
    }

    #[test]
    fn test_rows_split_by_location_show_it() {
        let (mut t, window) = TestSnap::page();
        for (i, location) in [None, Some((7, 0, 4)), Some((7, 9, 0))]
            .into_iter()
            .enumerate()
        {
            let foo = t.node("object", "Foo", 11 + 2 * i as u32, 20);
            t.edge(window, "property", &format!("p{i}"), foo);
            if let Some((script, line, column)) = location {
                t.locations.push((foo, script, line, column));
            }
        }
        let (base, current) = (TestSnap::page().0.aggregates(), t.aggregates());
        let diffs = diff_snapshots(&base, &current).unwrap();
        let summary = format_class_diff_summary(&diffs, &defaults(), OutputFormat::Text).unwrap();
        // Line and column count from 1; V8 stores them from 0.
        assert!(summary.ends_with(
            "\nidx,className,location,addedCount,removedCount,countDelta,addedSize,removedSize,sizeDelta\n\
             0,Foo,,1,0,1,20,0,20\n\
             1,Foo,7:1:5,1,0,1,20,0,20\n\
             2,Foo,7:10:1,1,0,1,20,0,20\n"
        ), "{summary}");
        let json: serde_json::Value = serde_json::from_str(
            &format_class_diff_summary(&diffs, &defaults(), OutputFormat::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(json["diffs"][0]["location"], serde_json::Value::Null);
        assert_eq!(json["diffs"][1]["location"], "7:1:5");

        let objects = compute_class_node_detail(&base, &current, &diffs[2].key);
        let detail =
            format_class_diff_detail(2, &diffs[2], &objects, &defaults(), OutputFormat::Text)
                .unwrap();
        assert!(
            detail.starts_with("idx:2,className:Foo,location:7:10:1,addedCount:1,"),
            "{detail}"
        );
        assert!(detail.ends_with("\n+,15,20\n"));
        let json: serde_json::Value = serde_json::from_str(
            &format_class_diff_detail(2, &diffs[2], &objects, &defaults(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(json["location"], "7:10:1");
    }

    /// `v` with the given fields of its `array` replaced.
    fn with_fields(
        mut v: serde_json::Value,
        array: &str,
        changes: &[(usize, serde_json::Value)],
    ) -> serde_json::Value {
        for (at, value) in changes {
            v[array][*at] = value.clone();
        }
        v
    }

    /// Loads `v` through a file, as the commands read snapshots.
    fn load_json(v: &serde_json::Value) -> Result<Snap> {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{v}").unwrap();
        load_snapshot(file.path().to_str().unwrap())
    }

    #[test]
    fn test_element_indices_past_i32_max_load() {
        let (mut t, window) = TestSnap::page();
        let a = t.node("object", "A", 11, 8);
        t.edge(window, "element", "0", a);
        t.edge(window, "hidden", "0", a);
        t.edge(window, "property", "p", a);
        // Window's edges 1-3 follow the root's edge 0: fields 3-5, 6-8 and
        // 9-11, each type, name_or_index, to_node. V8 prints an index of
        // 2^31 or more as a negative int.
        let v = with_fields(t.json(), "edges", &[(4, json!(i32::MIN)), (7, json!(-2))]);
        let snap = load_json(&v).unwrap();
        let names: Vec<String> = snap
            .edge_range(window)
            .map(|e| snap.edge_name(e).into_owned())
            .collect();
        assert_eq!(names, ["2147483648", "4294967294", "p"]);

        // Anywhere else a negative value is corrupt.
        for (at, edge, field) in [(3, 1, "type"), (5, 1, "to_node"), (10, 3, "name_or_index")] {
            let v = with_fields(t.json(), "edges", &[(at, json!(-1))]);
            let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
            assert!(
                err.to_string().contains(&format!(
                    "edge {edge} has {field} -1, but only an element or hidden edge's index"
                )),
                "{err}"
            );
        }
        // So is a value no 32-bit field holds.
        for value in [i64::from(i32::MIN) - 1, i64::from(u32::MAX) + 1] {
            let v = with_fields(t.json(), "edges", &[(4, json!(value))]);
            let err = serde_json::from_value::<RawSnapshot>(v).err().unwrap();
            assert!(
                err.to_string()
                    .contains("expected an integer from -2147483648 to 4294967295"),
                "{err}"
            );
        }
    }

    #[test]
    fn test_unknown_locations_leave_objects_unlocated() {
        let (mut t, window) = TestSnap::page();
        for i in 0..4 {
            let foo = t.node("object", "Foo", 11 + 2 * i as u32, 20);
            t.edge(window, "property", &format!("p{i}"), foo);
            t.locations.push((foo, 7, 0, 4));
        }
        // Four fields per record: object_index, script_id, line, column. V8
        // writes -1 for a line and column it could not work out, and older
        // versions printed that unsigned.
        let v = with_fields(
            t.json(),
            "locations",
            &[
                (6, json!(-1)),
                (7, json!(-1)),
                (10, json!(u32::MAX)),
                (11, json!(u32::MAX)),
            ],
        );
        let snap = load_json(&v).unwrap();
        let aggs = build_class_aggregates(&snap, &assign_classes(&snap, &[]));
        let located = ClassKey {
            name: "Foo".into(),
            location: Some((7, 0, 4)),
        };
        assert_eq!(aggs.members(&located), [(11, 20), (17, 20)]);
        assert_eq!(aggs.members(&key("Foo")), [(13, 20), (15, 20)]);

        // Any other negative location field is corrupt.
        for (at, value, message) in [
            (1, -1, "location field 'script_id' holds -1"),
            (6, -2, "location field 'line' holds -2"),
        ] {
            let v = with_fields(t.json(), "locations", &[(at, json!(value))]);
            let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
            assert!(err.to_string().contains(message), "{err}");
        }
    }

    #[test]
    fn test_rejects_partial_location_records() {
        let (mut t, window) = TestSnap::page();
        let foo = t.node("object", "Foo", 11, 20);
        t.edge(window, "property", "foo", foo);
        t.locations.push((foo, 7, 0, 4));
        let mut v = t.json();
        v["locations"].as_array_mut().unwrap().pop();
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(
            err.to_string()
                .contains("locations array length (3) is not a multiple of its 4 location fields"),
            "{err}"
        );
        // A snapshot may record no locations at all.
        let mut v = t.json();
        v["locations"] = json!([]);
        assert!(load_json(&v).unwrap().locations.is_empty());
        v.as_object_mut().unwrap().remove("locations");
        assert!(load_json(&v).unwrap().locations.is_empty());
    }

    #[test]
    fn test_rejects_locations_of_no_node() {
        let (mut t, window) = TestSnap::page();
        let foo = t.node("object", "Foo", 11, 20);
        t.edge(window, "property", "foo", foo);
        t.locations.push((foo, 7, 0, 4));
        assert_eq!(t.snap().locations.len(), 1);
        // Three nodes of six fields: an object_index is 0, 6 or 12. A record
        // has to name one even when its line is unknown.
        for (object_index, line) in [(7, 0), (18, 0), (u32::MAX, 0), (13, -1)] {
            let v = with_fields(
                t.json(),
                "locations",
                &[(0, json!(object_index)), (2, json!(line))],
            );
            let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
            assert!(
                err.to_string().contains(&format!(
                    "location 0 points to invalid node index {object_index}"
                )),
                "{err}"
            );
        }
    }

    #[test]
    fn test_rejects_edge_counts_that_overflow() {
        // Check the arithmetic on its own: the product or the running sum
        // can wrap, and a prefix past the array is out of bounds.
        assert!(edge_prefix(0, u32::MAX, usize::MAX, usize::MAX).is_err());
        assert!(edge_prefix(usize::MAX, 1, 1, usize::MAX).is_err());
        assert!(edge_prefix(0, u32::MAX, 1, 0).is_err());
        // A small fixture still covers the bounds check through Snap::new.
        let raw = RawSnapshot {
            snapshot: SnapshotMeta {
                meta: MetaDetails {
                    node_fields: ["id", "name", "self_size", "edge_count"]
                        .map(String::from)
                        .to_vec(),
                    node_types: Vec::new(),
                    edge_fields: vec![String::new()],
                    edge_types: Vec::new(),
                    location_fields: Vec::new(),
                },
                node_count: None,
                edge_count: None,
                root_index: 0,
            },
            nodes: NodeArray {
                fields: [0, 0, 0, u32::MAX].to_vec(),
                wide: Vec::new(),
            },
            edges: IntArray::default(),
            locations: IntArray::default(),
            strings: vec![String::new()],
        };
        let err = Snap::new(raw).err().unwrap();
        assert!(
            err.to_string()
                .contains("edge_count adds up to more than the 0 fields of the edges array"),
            "{err}"
        );
    }

    #[test]
    fn test_rejects_declared_record_counts_that_disagree_with_the_arrays() {
        let (mut t, window) = TestSnap::page();
        let a = t.node("object", "A", 11, 8);
        t.edge(window, "property", "a", a);
        let mut v = t.json();
        v["snapshot"]["node_count"] = json!(4);
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(err.to_string().contains("header declares 4 nodes"), "{err}");

        let mut v = t.json();
        v["snapshot"]["edge_count"] = json!(3);
        let err = Snap::new(serde_json::from_value(v).unwrap()).err().unwrap();
        assert!(err.to_string().contains("header declares 3 edges"), "{err}");

        // The counts V8 writes are accepted; leaving them out still works.
        let mut v = t.json();
        v["snapshot"]["node_count"] = json!(3);
        v["snapshot"]["edge_count"] = json!(2);
        assert!(Snap::new(serde_json::from_value(v).unwrap()).is_ok());
    }

    /// Disabling the heap profiler makes V8 forget its object ids, so the
    /// next snapshot of the tab would number every object afresh and a diff
    /// would pair unrelated objects. A capture leaves the profiler enabled.
    #[tokio::test]
    async fn test_take_heapsnapshot_leaves_the_profiler_enabled() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let browser = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut methods = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: serde_json::Value = serde_json::from_str(&text).unwrap();
                methods.push(req["method"].as_str().unwrap_or("").to_string());
                if req["method"] == "HeapProfiler.takeHeapSnapshot" {
                    let chunk = json!({
                        "method": "HeapProfiler.addHeapSnapshotChunk",
                        "params": { "chunk": "{}" },
                        "sessionId": "S",
                    });
                    ws.send(Message::Text(chunk.to_string())).await.unwrap();
                }
                let reply = json!({ "id": req["id"], "result": {} });
                if ws.send(Message::Text(reply.to_string())).await.is_err() {
                    break;
                }
            }
            methods
        });
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("s.heapsnapshot");
        let mut client = CdpClient::connect(&url, "Chrome").await.unwrap();
        take_heapsnapshot(
            &mut client,
            "S",
            output.to_str().unwrap(),
            OutputFormat::Text,
        )
        .await
        .unwrap();
        drop(client);
        assert_eq!(
            browser.await.unwrap(),
            ["HeapProfiler.enable", "HeapProfiler.takeHeapSnapshot"]
        );
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "{}");
    }
}

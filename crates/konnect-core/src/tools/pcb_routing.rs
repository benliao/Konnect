//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Every tool here edits the `.kicad_pcb` file directly. Routing used to run
//! over the KiCAD IPC API while vias were written to disk, so a session that
//! used both left KiCAD's in-memory board and the file disagreeing — whichever
//! saved last silently won. One backend means the `board` argument always names
//! the file that is actually changed, and the tools work headless.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, NetRef, ToolContext, ToolDef};
use konnect_sexp::parser::{parse_sexp, SexpNode};
use konnect_sexp::writer::{
    apply_edits, check_document, find_balanced_block, find_block_starts,
    find_block_with_leading_whitespace, new_uuid, write_atomic, SexpEdit,
};
use serde_json::json;
use std::path::Path;

/// KiCAD reloads a board from disk only on open/revert; every file writer here
/// says so rather than leaving the user staring at a stale window.
const RELOAD_NOTE: &str =
    "Written to the file. If the board is open in KiCAD, use File > Revert to see the change.";

// ─── S-expression helpers ─────────────────────────────────────────────────────

/// Offset of the root block's closing paren — where a new top-level item goes.
///
/// String-aware, unlike `rfind(')')`, which lands on whatever the last `)` byte
/// in the file happens to be (a `)` inside a quoted property value, say).
fn root_close_offset(content: &str) -> Option<usize> {
    find_balanced_block(content, 0).map(|(_, end)| end - 1)
}

/// True when the `(` at `open` opens a `(tag …)` block — whole tags only, so
/// `"segment"` never matches `(segment_thing`.
fn tag_opens_at(content: &str, open: usize, tag: &str) -> bool {
    let Some(rest) = content.get(open + 1..) else {
        return false;
    };
    rest.strip_prefix(tag)
        .is_some_and(|after| after.chars().next().is_none_or(|c| c.is_whitespace() || c == '(' || c == ')'))
}

/// Byte offsets of every **top-level** `(tag …)` block opening, in file order.
///
/// One string-aware pass. Unlike [`find_block_starts`] it ignores same-named
/// blocks nested inside a footprint, and unlike a `"\n\t(tag"` literal it does
/// not care whether the file is tab-indented (KiCAD) or space-indented (this
/// crate's writer).
fn top_level_block_starts(content: &str, tag: &str) -> Vec<usize> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut i) = (0usize, 0usize);
    let (mut in_string, mut escape) = (false, false);

    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
        } else if in_string {
            if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'(' => {
                    // depth 1 is inside the document root, so its direct
                    // children — the top-level items — open there.
                    if depth == 1 && tag_opens_at(content, i, tag) {
                        out.push(i);
                    }
                    depth += 1;
                }
                b')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// The board's numbered net table, `(net 7 "GND")` → `7: "GND"`.
///
/// Empty on KiCAD 10 (20260206+) boards, which dropped the table and reference
/// nets by name.
fn net_table(content: &str) -> std::collections::HashMap<i32, String> {
    let mut map = std::collections::HashMap::new();
    for s in find_block_starts(content, "net") {
        let rest = content[s + "(net".len()..].trim_start();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        let Some(inner) = rest[digits.len()..].trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = inner.find('"') else { continue };
        if let Ok(id) = digits.parse::<i32>() {
            map.entry(id).or_insert_with(|| inner[..end].to_string());
        }
    }
    map
}

/// Read a `(net …)` node in either format: `(net "GND")` on KiCAD 10, or
/// `(net 7)` / `(net 7 "GND")` on the older numbered boards, where the name is
/// looked up in `table`.
fn read_net_node(node: &SexpNode, table: &std::collections::HashMap<i32, String>) -> (Option<i32>, Option<String>) {
    match node.get(1) {
        Some(SexpNode::Str(s)) => (None, Some(s.clone())),
        Some(SexpNode::Atom(a)) => match a.parse::<i32>() {
            Ok(id) => {
                // `(net 7 "GND")` carries the name inline; a bare `(net 7)`
                // needs the table.
                let name = node
                    .get(2)
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                    .or_else(|| table.get(&id).cloned());
                (Some(id), name)
            }
            Err(_) => (None, Some(a.clone())),
        },
        _ => (None, None),
    }
}

/// The `(net …)` field a track/via carries, in this board's own convention.
fn net_field(net: &NetRef) -> String {
    match net {
        NetRef::Named(n) => format!("(net \"{n}\")"),
        NetRef::Numbered(id, _) => format!("(net {id})"),
    }
}

/// A track `(segment …)` as it exists on the board, with the byte range it
/// occupies so it can be deleted or replaced.
#[derive(Debug, Clone)]
struct TraceSegment {
    start: usize,
    end: usize,
    uuid: Option<String>,
    net_code: Option<i32>,
    net_name: Option<String>,
    layer: Option<String>,
    width: Option<f64>,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

/// Every top-level `(segment …)` on the board, in file order.
fn collect_segments(content: &str) -> Vec<TraceSegment> {
    let table = net_table(content);
    top_level_block_starts(content, "segment")
        .into_iter()
        .filter_map(|s| {
            let (start, end) = find_balanced_block(content, s)?;
            let node = parse_sexp(&content[start..end]).ok()?;
            let (net_code, net_name) = node
                .find("net")
                .map(|n| read_net_node(n, &table))
                .unwrap_or((None, None));
            let at = |tag: &str, i: usize| node.find(tag).and_then(|n| n.get_f64(i)).unwrap_or(0.0);
            Some(TraceSegment {
                start,
                end,
                uuid: node.find_str("uuid").map(str::to_string),
                net_code,
                net_name,
                layer: node.find_str("layer").map(str::to_string),
                width: node.find_f64("width"),
                x1: at("start", 1),
                y1: at("start", 2),
                x2: at("end", 1),
                y2: at("end", 2),
            })
        })
        .collect()
}

/// A `(segment …)` in KiCAD 10's format, read verbatim off a 20260206 board:
///
/// ```text
/// (segment
///     (start 111.8885 84.9173)
///     (end 110.83 85.9758)
///     (width 0.1)
///     (layer "F.Cu")
///     (net "+3V3")
///     (uuid "004ea16f-…")
/// )
/// ```
///
/// The net is referenced by name on this version and by number on older
/// boards; `net` carries whichever the board itself uses.
#[allow(clippy::too_many_arguments)]
fn format_segment(
    net: &NetRef,
    layer: &str,
    width: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    uuid: &str,
) -> String {
    let net_field = net_field(net);
    format!(
        "(segment\n\t\t(start {x1} {y1})\n\t\t(end {x2} {y2})\n\t\t(width {width})\n\t\t\
         (layer \"{layer}\")\n\t\t{net_field}\n\t\t(uuid \"{uuid}\")\n\t)"
    )
}

/// `format_segment` wrapped as a new top-level item.
#[allow(clippy::too_many_arguments)]
fn top_level_segment(
    net: &NetRef,
    layer: &str,
    width: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    uuid: &str,
) -> String {
    format!(
        "\n\t{}",
        format_segment(net, layer, width, x1, y1, x2, y2, uuid)
    )
}

/// Apply `edits` and write, but only if the result still parses as a board.
///
/// Every edit here is a raw byte splice, so a mis-computed offset does not fail
/// loudly — it writes a structurally different file that KiCAD refuses to open.
fn commit(board: &Path, content: String, edits: Vec<SexpEdit>) -> Result<(), CallToolResult> {
    let new_content = apply_edits(content, edits);
    if let Err(why) = check_document(&new_content, "kicad_pcb") {
        return Err(CallToolResult::error(format!(
            "Internal error: the edit would have corrupted '{}' ({why}) — nothing was written.",
            board.display()
        )));
    }
    write_atomic(board, &new_content).map_err(|e| {
        CallToolResult::error(format!("Failed to write '{}': {e}", board.display()))
    })
}

/// The insert offset for a new top-level item, or the error to return when the
/// file has no balanced root block.
fn root_close_or_error(content: &str, board: &Path) -> Result<usize, CallToolResult> {
    root_close_offset(content).ok_or_else(|| {
        CallToolResult::error(format!(
            "'{}' has no balanced (kicad_pcb …) root block — refusing to edit it.",
            board.display()
        ))
    })
}

fn format_zone(
    net: &crate::tools::NetRef,
    layer: &str,
    clearance: f64,
    min_w: f64,
    pts: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let net_fields = net.zone_fields();
    let pt_str: String = pts
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone {net_fields} (layer \"{layer}\") (uuid \"{uuid}\")\n    \
         (hatch edge 0.508)\n    (connect_pads (clearance {clearance}))\n    \
         (min_thickness {min_w})\n    (fill yes)\n    \
         (polygon (pts{pt_str}\n    ))\n  )"
    )
}


// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "add_net",
            "Add a new net entry to the PCB file (S-expression insert, no KiCAD IPC required).",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" }
                },
                "required": ["board", "net_name"]
            }),
            |args, ctx| async move { handle_add_net(args, ctx).await }
        ),
        tool!(
            "route_trace",
            "Route a trace segment between two points on a copper layer. Written directly to the \
             .kicad_pcb file; the net must already exist on the board.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string" },
                    "layer":    { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_trace(args, ctx).await }
        ),
        tool!(
            "route_pad_to_pad",
            "Route a direct trace between two pads of named components (L-bend routing), written \
             directly to the .kicad_pcb file. Both pads must already be on the same net; the tool \
             refuses to connect different nets rather than silently creating a short.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "net_name":    { "type": "string", "description": "Optional. If given, asserted against the pads' actual net; the board's assignment always wins" },
                    "ref1":        { "type": "string", "description": "First component reference" },
                    "pad1":        { "type": "string", "description": "First pad number" },
                    "ref2":        { "type": "string", "description": "Second component reference" },
                    "pad2":        { "type": "string", "description": "Second pad number" },
                    "layer":       { "type": "string", "default": "F.Cu" },
                    "width":       { "type": "number", "default": 0.25 }
                },
                "required": ["board", "ref1", "pad1", "ref2", "pad2"]
            }),
            |args, ctx| async move { handle_route_pad_to_pad(args, ctx).await }
        ),
        tool!(
            "add_via",
            "Add a via at a given position on an existing net. Written directly to the .kicad_pcb \
             file. The net must already exist on the board.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "drill":     { "type": "number", "description": "Drill diameter in mm", "default": 0.4 },
                    "pad_size":  { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 },
                    "start_layer": { "type": "string", "description": "Start copper layer", "default": "F.Cu" },
                    "end_layer":   { "type": "string", "description": "End copper layer", "default": "B.Cu" }
                },
                "required": ["board", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_add_via(args, ctx).await }
        ),
        tool!(
            "add_copper_pour",
            "Add a copper fill zone polygon on a layer/net via S-expression file insert.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "points": {
                        "type": "array",
                        "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
                    },
                    "clearance": { "type": "number", "default": 0.2 },
                    "min_width": { "type": "number", "default": 0.25 }
                },
                "required": ["board", "net_name", "layer", "points"]
            }),
            |args, ctx| async move { handle_add_copper_pour(args, ctx).await }
        ),
        tool!(
            "delete_trace",
            "Delete a trace segment identified by its UUID (as returned by query_traces or \
             route_trace) from the .kicad_pcb file.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the track segment to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_trace(args, ctx).await }
        ),
        tool!(
            "query_traces",
            "List trace segments read from the .kicad_pcb file, optionally filtered by net and/or \
             layer. Each entry carries the uuid that delete_trace and modify_trace take.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_traces(args, ctx).await }
        ),
        tool!(
            "get_nets_list",
            "Return the nets present on the PCB, read from the file's pads (and its net table, \
             where the format version still has one).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_nets_list(args, ctx).await }
        ),
        tool!(
            "modify_trace",
            "Rewrite an existing trace segment, identified by uuid, with new endpoints/net/layer/\
             width. Edits the .kicad_pcb file in place and keeps the same uuid.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "uuid":      { "type": "string" },
                    "net_name":  { "type": "string" },
                    "layer":     { "type": "string" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width":     { "type": "number", "default": 0.25 }
                },
                "required": ["board", "uuid", "net_name", "layer", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_modify_trace(args, ctx).await }
        ),
        tool!(
            "create_netclass",
            "Add a netclass definition to the board's design rules (S-expression file insert).",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "name":         { "type": "string", "description": "Netclass name (e.g. 'Power')" },
                    "clearance":    { "type": "number", "description": "Clearance in mm", "default": 0.2 },
                    "trace_width":  { "type": "number", "description": "Default trace width in mm", "default": 0.25 },
                    "via_drill":    { "type": "number", "description": "Via drill diameter in mm", "default": 0.4 },
                    "via_diameter": { "type": "number", "description": "Via pad diameter in mm", "default": 0.8 }
                },
                "required": ["board", "name"]
            }),
            |args, ctx| async move { handle_create_netclass(args, ctx).await }
        ),
        tool!(
            "assign_net_to_class",
            "Assign a net to an existing netclass in the PCB file (S-expression edit).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file" },
                    "net_name":  { "type": "string", "description": "Net name to assign" },
                    "netclass":  { "type": "string", "description": "Netclass name to assign the net to" }
                },
                "required": ["board", "net_name", "netclass"]
            }),
            |args, ctx| async move { handle_assign_net_to_class(args, ctx).await }
        ),
        tool!(
            "route_differential_pair",
            "Route a differential pair — two parallel trace segments offset either side of the \
             given line — into the .kicad_pcb file. Both nets must already exist on the board.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_pos":  { "type": "string", "description": "Positive net name" },
                    "net_neg":  { "type": "string", "description": "Negative net name" },
                    "layer":    { "type": "string", "default": "F.Cu" },
                    "x1": { "type": "number" }, "y1": { "type": "number" },
                    "x2": { "type": "number" }, "y2": { "type": "number" },
                    "width": { "type": "number", "default": 0.1 },
                    "gap":   { "type": "number", "description": "Gap between pair traces in mm", "default": 0.1 }
                },
                "required": ["board", "net_pos", "net_neg", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_route_diff_pair(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_net(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;

    if let Some(existing) = crate::tools::resolve_net(&content, &net_name) {
        return Ok(CallToolResult::json(&json!({
            "net_name": existing.name(), "net_id": existing.code(),
            "note": "net already exists on this board"
        })));
    }

    // The highest declared net id, or None on a board with no numeric net
    // table. Counting `(net ` occurrences — the old approach — also counted
    // every pad's net reference, so on a real board the "next id" was ~142.
    let highest = konnect_sexp::writer::find_block_starts(&content, "net")
        .into_iter()
        .filter_map(|s| {
            let rest = content[s + "(net".len()..].trim_start();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            (!digits.is_empty() && rest[digits.len()..].trim_start().starts_with('"'))
                .then(|| digits.parse::<i32>().ok())
                .flatten()
        })
        .max();

    let Some(highest) = highest else {
        // KiCAD 10 (20260206+) has no net table: nets exist because pads
        // reference them, so there is nothing to add here. Writing a stray
        // top-level entry parses but does nothing.
        return Ok(CallToolResult::error(format!(
            "This board has no numeric net table — KiCAD 10 derives nets from the \
             pads that reference them, so '{net_name}' cannot be added as a \
             standalone entry. Assign the net to a pad (or import the netlist \
             from the schematic) instead."
        )));
    };

    let net_id = highest + 1;
    let net_sexp = format!("\n  (net {net_id} \"{net_name}\")");
    let close_pos = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };
    if let Err(e) = commit(
        &board_path,
        content,
        vec![SexpEdit::insert(close_pos, net_sexp)],
    ) {
        return Ok(e);
    }

    Ok(CallToolResult::json(
        &json!({ "net_id": net_id, "net_name": net_name, "source": "file" }),
    ))
}

async fn handle_route_trace(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let content = std::fs::read_to_string(&board_path)?;
    // A track that resolves to no net is copper connected to nothing — DRC does
    // not flag it, so it must be an error here rather than a silent net 0.
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };
    let close = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };

    let uuid = new_uuid();
    let block = top_level_segment(&net, &layer, width, x1, y1, x2, y2, &uuid);
    if let Err(e) = commit(&board_path, content, vec![SexpEdit::insert(close, block)]) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "uuid": uuid,
        "net": net.name(), "net_code": net.code(),
        "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

async fn handle_route_pad_to_pad(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    // Optional: the board's own pad assignments are the source of truth.
    let net_name = args["net_name"].as_str().unwrap_or("").to_string();
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad1 = match require_str(args, "pad1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pad2 = match require_str(args, "pad2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let width = args["width"].as_f64().unwrap_or(0.25);

    // Pad geometry and the tracks now come from and go to the same file, so
    // there is no longer a way for this to read coordinates out of one board
    // and write the tracks onto another — which is what the IPC path did
    // whenever KiCAD had a different file open.
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    // Both pads must already be on the same net. Trusting the caller's
    // `net_name` let a mistyped argument draw a trace between two different
    // nets — a short that exists nowhere in the schematic, reported as
    // `routed: true` and only findable later by DRC.
    let net1 = find_pad_net(&tree, &ref1, &pad1)?;
    let net2 = find_pad_net(&tree, &ref2, &pad2)?;
    let resolved = match (&net1, &net2) {
        (Some(a), Some(b)) if a == b => a.clone(),
        (Some(a), Some(b)) => {
            return Ok(CallToolResult::error(format!(
                "Refusing to route: {ref1}.{pad1} is on net '{a}' but {ref2}.{pad2} \
                 is on net '{b}'. Connecting them would short two different nets. \
                 Check the pad numbers, or fix the connection in the schematic and \
                 re-import the netlist."
            )));
        }
        (None, _) | (_, None) => {
            let (r, p) = if net1.is_none() {
                (&ref1, &pad1)
            } else {
                (&ref2, &pad2)
            };
            return Ok(CallToolResult::error(format!(
                "Refusing to route: {r}.{p} is not assigned to any net on this \
                 board, so a trace to it would not belong to a net. Import the \
                 netlist from the schematic first."
            )));
        }
    };
    // `net_name` degrades to an optional assertion against the board's truth.
    if !net_name.is_empty() && net_name != resolved {
        return Ok(CallToolResult::error(format!(
            "Refusing to route: net_name '{net_name}' was given, but {ref1}.{pad1} \
             and {ref2}.{pad2} are both on '{resolved}'. Pass net_name='{resolved}' \
             or omit it."
        )));
    }
    let net_name = resolved;

    // The pads reference the net, so the board always knows it; this only
    // decides which of the two on-disk formats to write it in.
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };

    let (x1, y1) = find_pad_board_position(&tree, &ref1, &pad1)?;
    let (x2, y2) = find_pad_board_position(&tree, &ref2, &pad2)?;

    let close = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };

    // Route an L-bend: horizontal first, then vertical. Axis-aligned pads get a
    // single segment instead of a zero-length stub.
    let mut uuids: Vec<String> = Vec::new();
    let mut blocks = String::new();
    {
        let mut push = |x1: f64, y1: f64, x2: f64, y2: f64| {
            let uuid = new_uuid();
            blocks.push_str(&top_level_segment(&net, &layer, width, x1, y1, x2, y2, &uuid));
            uuids.push(uuid);
        };
        if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
            push(x1, y1, x2, y2);
        } else {
            // L-bend: horizontal leg, then vertical.
            let (mid_x, mid_y) = (x2, y1);
            push(x1, y1, mid_x, mid_y);
            push(mid_x, mid_y, x2, y2);
        }
    }

    if let Err(e) = commit(&board_path, content, vec![SexpEdit::insert(close, blocks)]) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "uuids": uuids,
        "net": net.name(), "net_code": net.code(),
        "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

/// Look up a pad's board-space (x, y) position from the parsed PCB S-expression tree.
/// The net a pad belongs to, as recorded on the board.
///
/// KiCAD 10 writes `(net "GND")` on the pad; older boards write
/// `(net 1 "GND")`. Returns `None` for an unconnected pad.
fn find_pad_net(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
    pad_number: &str,
) -> anyhow::Result<Option<String>> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let pad = fp_node
        .find_all("pad")
        .into_iter()
        .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(pad_number))
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;

    let Some(net) = pad.find("net") else {
        return Ok(None);
    };
    // `(net "GND")` -> arg 1 is the name; `(net 1 "GND")` -> arg 2 is.
    let name = net
        .get(1)
        .and_then(|n| n.as_str())
        .filter(|s| s.parse::<i32>().is_err())
        .or_else(|| net.get(2).and_then(|n| n.as_str()));
    Ok(name.filter(|s| !s.is_empty()).map(str::to_string))
}

fn find_pad_board_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
    pad_number: &str,
) -> anyhow::Result<(f64, f64)> {
    let fp_node = tree
        .find_all("footprint")
        .into_iter()
        .find(|fp| {
            fp.find_all("property").iter().any(|p| {
                p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                    && p.get(2).and_then(|n| n.as_str()) == Some(reference)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found on board", reference))?;

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pad = fp_node
        .find_all("pad")
        .into_iter()
        .find(|p| p.get(1).and_then(|n| n.as_str()) == Some(pad_number))
        .ok_or_else(|| anyhow::anyhow!("Pad '{}' not found on '{}'", pad_number, reference))?;

    let pad_at = pad
        .find("at")
        .ok_or_else(|| anyhow::anyhow!("Pad has no (at) node"))?;
    let local_x = pad_at.get_f64(1).unwrap_or(0.0);
    let local_y = pad_at.get_f64(2).unwrap_or(0.0);

    // Transform local pad coords to board space (rotation only).
    // Uses the canonical KiCAD transform — see konnect_sexp::geometry.
    Ok(konnect_sexp::geometry::transform_pad(
        local_x, local_y, fp_x, fp_y, fp_rot,
    ))
}

/// A `(via …)` in KiCAD 10's format, read off a real 20260206 board:
///
/// ```text
/// (via (at X Y) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net "+3V3") (uuid …))
/// ```
fn format_via(
    net: &crate::tools::NetRef,
    x: f64,
    y: f64,
    drill: f64,
    size: f64,
    start_layer: &str,
    end_layer: &str,
) -> String {
    let uuid = new_uuid();
    // The via's net field follows the board's own convention, same as a zone's.
    let net_field = net_field(net);
    format!(
        "\n\t(via\n\t\t(at {x} {y})\n\t\t(size {size})\n\t\t(drill {drill})\n\t\t         (layers \"{start_layer}\" \"{end_layer}\")\n\t\t{net_field}\n\t\t(uuid \"{uuid}\")\n\t)"
    )
}

async fn handle_add_via(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let drill = args["drill"].as_f64().unwrap_or(0.4);
    let pad_size = args["pad_size"].as_f64().unwrap_or(0.8);
    let start_layer = args["start_layer"].as_str().unwrap_or("F.Cu");
    let end_layer = args["end_layer"].as_str().unwrap_or("B.Cu");

    if pad_size <= drill {
        return Ok(CallToolResult::error(format!(
            "Via pad_size ({pad_size}mm) must be larger than drill ({drill}mm)."
        )));
    }

    // Written to the file rather than over IPC: KiCAD 10.0.4 rejects the Via
    // message this client builds ("could not unpack PCB_VIA from request"),
    // and its actual protobuf schema is not shipped with the application, so
    // the mismatch cannot be verified from here. The file format is known and
    // testable, and vias are the only way to change layers — leaving this on a
    // broken transport makes multilayer boards impossible to finish.
    let content = std::fs::read_to_string(&board_path)?;
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };

    let via = format_via(&net, x, y, drill, pad_size, start_layer, end_layer);
    let close_pos = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };
    if let Err(e) = commit(&board_path, content, vec![SexpEdit::insert(close_pos, via)]) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "net": net.name(), "net_code": net.code(), "x": x, "y": y,
        "drill": drill, "pad_size": pad_size,
        "layers": [start_layer, end_layer],
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

async fn handle_add_copper_pour(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let min_w = args["min_width"].as_f64().unwrap_or(0.25);
    let pts_arr = match args["points"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'points' array")),
    };

    let pts: Vec<(f64, f64)> = pts_arr
        .iter()
        .filter_map(|p| Some((p["x"].as_f64()?, p["y"].as_f64()?)))
        .collect();
    if pts.len() < 3 {
        return Ok(CallToolResult::error("Zone requires at least 3 points"));
    }

    let content = std::fs::read_to_string(&board_path)?;
    // Silently falling back to net 0 here produced an isolated pour — copper
    // that looks like a ground plane but is connected to nothing.
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };
    let zone_s = format_zone(&net, &layer, clearance, min_w, &pts);
    let close = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };
    if let Err(e) = commit(&board_path, content, vec![SexpEdit::insert(close, zone_s)]) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "net": net.name(), "layer": layer, "points": pts.len(),
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let Some(seg) = collect_segments(&content)
        .into_iter()
        .find(|s| s.uuid.as_deref() == Some(uuid.as_str()))
    else {
        return Ok(CallToolResult::error(unknown_trace_message(
            &content,
            &uuid,
            &board_path,
        )));
    };

    // The delete range is refused rather than defaulted. An earlier version of
    // this code fell back to offset 0 when it could not locate the block and
    // erased the whole file; a failed lookup must stay a failed tool call.
    let Some((del_start, del_end)) = find_block_with_leading_whitespace(&content, seg.start) else {
        return Ok(CallToolResult::error(format!(
            "Refusing to delete trace {uuid}: its byte range in '{}' could not be \
             established. Nothing was written.",
            board_path.display()
        )));
    };
    if del_start > seg.start || del_end != seg.end {
        return Ok(CallToolResult::error(format!(
            "Refusing to delete trace {uuid}: the computed range {del_start}..{del_end} \
             does not match the segment block at {}..{}. Nothing was written.",
            seg.start, seg.end
        )));
    }

    if let Err(e) = commit(
        &board_path,
        content,
        vec![SexpEdit::delete(del_start, del_end)],
    ) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "deleted_uuid": uuid,
        "net": seg.net_name, "layer": seg.layer, "width": seg.width,
        "from": { "x": seg.x1, "y": seg.y1 }, "to": { "x": seg.x2, "y": seg.y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

/// Why a uuid did not name a trace — a via or an arc carries a uuid too, and
/// "not found" alone sends the caller looking in the wrong place.
fn unknown_trace_message(content: &str, uuid: &str, board: &Path) -> String {
    for tag in ["via", "arc", "zone", "footprint", "gr_line"] {
        let owns = top_level_block_starts(content, tag).into_iter().any(|s| {
            find_balanced_block(content, s)
                .and_then(|(a, b)| parse_sexp(&content[a..b]).ok())
                .and_then(|n| n.find_str("uuid").map(str::to_string))
                .as_deref()
                == Some(uuid)
        });
        if owns {
            return format!(
                "'{uuid}' is the uuid of a ({tag}) on '{}', not a trace segment. \
                 delete_trace only deletes (segment …) tracks.",
                board.display()
            );
        }
    }
    format!(
        "No trace segment with uuid '{uuid}' on '{}'. Run query_traces to list the \
         segments actually on this board.",
        board.display()
    )
}

async fn handle_query_traces(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let content = std::fs::read_to_string(&board_path)?;
    let items: Vec<serde_json::Value> = collect_segments(&content)
        .into_iter()
        .filter(|s| net.as_deref().is_none_or(|n| s.net_name.as_deref() == Some(n)))
        .filter(|s| layer.as_deref().is_none_or(|l| s.layer.as_deref() == Some(l)))
        .map(|s| {
            json!({
                // uuid is what delete_trace and modify_trace take — without it
                // a queried trace could not be edited.
                "uuid": s.uuid,
                "net": s.net_name, "net_code": s.net_code,
                "layer": s.layer, "width": s.width,
                "x1": s.x1, "y1": s.y1,
                "x2": s.x2, "y2": s.y2
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "traces": items,
        "target": board_path.display().to_string(),
        "source": "file"
    })))
}

async fn handle_get_nets_list(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;
    let table = net_table(&content);

    // Name -> netcode, where the board still has codes. KiCAD 10 dropped the
    // net table, so on those boards the pads are the only record that a net
    // exists at all.
    let mut nets: std::collections::BTreeMap<String, Option<i32>> = table
        .iter()
        .filter(|(_, name)| !name.is_empty())
        .map(|(id, name)| (name.clone(), Some(*id)))
        .collect();

    for fp in tree.find_all("footprint") {
        for pad in fp.find_all("pad") {
            let Some(node) = pad.find("net") else { continue };
            let (code, name) = read_net_node(node, &table);
            let Some(name) = name.filter(|n| !n.is_empty()) else {
                continue;
            };
            nets.entry(name).or_insert(code);
        }
    }

    let items: Vec<serde_json::Value> = nets
        .into_iter()
        .map(|(name, netcode)| json!({ "name": name, "netcode": netcode }))
        .collect();
    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "nets": items,
        "target": board_path.display().to_string(),
        "source": "file"
    })))
}

async fn handle_modify_trace(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.25);

    let content = std::fs::read_to_string(&board_path)?;
    let Some(seg) = collect_segments(&content)
        .into_iter()
        .find(|s| s.uuid.as_deref() == Some(uuid.as_str()))
    else {
        return Ok(CallToolResult::error(unknown_trace_message(
            &content,
            &uuid,
            &board_path,
        )));
    };
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };

    // Rewritten in place rather than deleted and re-appended, so the uuid the
    // caller holds keeps pointing at the same trace.
    let block = format_segment(&net, &layer, width, x1, y1, x2, y2, &uuid);
    if let Err(e) = commit(
        &board_path,
        content,
        vec![SexpEdit::replace(seg.start, seg.end, block)],
    ) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net.name(), "net_code": net.code(),
        "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

async fn handle_create_netclass(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let name = match require_str(args, "name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let trace_width = args["trace_width"].as_f64().unwrap_or(0.25);
    let via_drill = args["via_drill"].as_f64().unwrap_or(0.4);
    let via_dia = args["via_diameter"].as_f64().unwrap_or(0.8);

    let netclass_sexp = format!(
        "\n      (netclass \"{name}\"\n        (clearance {clearance})\n        \
         (trace_width {trace_width})\n        (via_drill {via_drill})\n        \
         (via_diameter {via_dia})\n      )"
    );

    let content = std::fs::read_to_string(&board_path)?;
    // Find (net_classes block or (net_settings block to insert into
    let insert_pos = if let Some(nc_pos) = content.find("(net_classes") {
        // Find closing paren of (net_classes ...)
        let block = &content[nc_pos..];
        nc_pos
            + block
                .find("\n    )")
                .unwrap_or(block.find(')').unwrap_or(block.len() - 1))
    } else {
        // No net_classes block; insert before last )
        content.rfind(')').unwrap_or(content.len())
    };

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_pos, netclass_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "created_netclass": name,
        "clearance": clearance, "trace_width": trace_width,
        "via_drill": via_drill, "via_diameter": via_dia
    })))
}

async fn handle_assign_net_to_class(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let netclass = match require_str(args, "netclass") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;

    // Locate the netclass block. `find_block_starts` skips quoted strings, and
    // `find_balanced_block` counts parens the same way — a netclass or net name
    // containing a stray '(' used to shift every offset after it.
    let nc_range = find_block_starts(&content, "netclass")
        .into_iter()
        .filter_map(|s| find_balanced_block(&content, s))
        .find(|&(s, e)| {
            parse_sexp(&content[s..e])
                .ok()
                .and_then(|n| n.get(1).and_then(|a| a.as_str()).map(str::to_string))
                .as_deref()
                == Some(netclass.as_str())
        });
    let Some((nc_pos, block_end)) = nc_range else {
        return Ok(CallToolResult::error(format!(
            "Netclass '{netclass}' not found in board file"
        )));
    };
    // Insert before the block's own closing paren.
    let nc_end = block_end - 1;

    // Check if net is already assigned
    let nc_block = &content[nc_pos..nc_end];
    let net_check = format!("(net \"{}\")", net_name);
    if nc_block.contains(&net_check) {
        return Ok(CallToolResult::json(&json!({
            "already_assigned": true,
            "net_name": net_name,
            "netclass": netclass
        })));
    }

    // Insert the net assignment before the closing paren of the netclass block
    let net_entry = format!("\n        (net \"{}\")", net_name);
    if let Err(e) = commit(&board_path, content, vec![SexpEdit::insert(nc_end, net_entry)]) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass,
        "source": "file"
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net_pos = match require_str(args, "net_pos") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let net_neg = match require_str(args, "net_neg") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();
    let x1 = match require_f64(args, "x1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y1 = match require_f64(args, "y1") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x2 = match require_f64(args, "x2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y2 = match require_f64(args, "y2") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let width = args["width"].as_f64().unwrap_or(0.1);
    let gap = args["gap"].as_f64().unwrap_or(0.1);
    let offset = (gap + width) / 2.0;

    // Route two parallel traces offset perpendicular to the direction
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let perp_x = -dy / len * offset;
    let perp_y = dx / len * offset;

    let content = std::fs::read_to_string(&board_path)?;
    // Both halves must resolve before either is written: half a differential
    // pair is worse than none, and a net that silently became 0 would leave one
    // half connected to nothing.
    let Some(np) = crate::tools::resolve_net(&content, &net_pos) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_pos));
    };
    let Some(nn) = crate::tools::resolve_net(&content, &net_neg) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_neg));
    };
    let close = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };

    let (uuid_pos, uuid_neg) = (new_uuid(), new_uuid());
    let blocks = format!(
        "{}{}",
        top_level_segment(
            &np,
            &layer,
            width,
            x1 + perp_x,
            y1 + perp_y,
            x2 + perp_x,
            y2 + perp_y,
            &uuid_pos,
        ),
        top_level_segment(
            &nn,
            &layer,
            width,
            x1 - perp_x,
            y1 - perp_y,
            x2 - perp_x,
            y2 - perp_y,
            &uuid_neg,
        )
    );
    if let Err(e) = commit(&board_path, content, vec![SexpEdit::insert(close, blocks)]) {
        return Ok(e);
    }

    Ok(CallToolResult::json(&json!({
        "net_pos": np.name(), "net_neg": nn.name(),
        "uuid_pos": uuid_pos, "uuid_neg": uuid_neg,
        "layer": layer, "width": width, "gap": gap,
        "target": board_path.display().to_string(),
        "source": "file",
        "note": RELOAD_NOTE
    })))
}

#[cfg(test)]
mod routing_safety_tests {
    use super::*;
    use crate::tools::NetRef;
    use konnect_sexp::parser::parse_sexp;

    /// Tab-indented, KiCAD 10 by-name nets. TP3.1 is +3V3, C12.1 is /OSC_OUT —
    /// the pair from the short-circuit report.
    const BOARD: &str = "(kicad_pcb\n\t(version 20260206)\n\
\t(footprint \"TP\"\n\t\t(at 10 10)\n\t\t(property \"Reference\" \"TP3\")\n\
\t\t(pad \"1\" smd rect\n\t\t\t(at 0 0)\n\t\t\t(net \"+3V3\")\n\t\t)\n\t)\n\
\t(footprint \"C\"\n\t\t(at 20 20)\n\t\t(property \"Reference\" \"C12\")\n\
\t\t(pad \"1\" smd rect\n\t\t\t(at 0 0)\n\t\t\t(net \"/OSC_OUT\")\n\t\t)\n\
\t\t(pad \"2\" smd rect\n\t\t\t(at 1 0)\n\t\t)\n\t)\n)\n";

    fn tree() -> konnect_sexp::parser::SexpNode {
        parse_sexp(BOARD).unwrap()
    }

    #[test]
    fn reads_a_pads_actual_net_by_name() {
        assert_eq!(
            find_pad_net(&tree(), "TP3", "1").unwrap(),
            Some("+3V3".to_string())
        );
        assert_eq!(
            find_pad_net(&tree(), "C12", "1").unwrap(),
            Some("/OSC_OUT".to_string())
        );
        // A pad with no (net …) is unassigned, not net "".
        assert_eq!(find_pad_net(&tree(), "C12", "2").unwrap(), None);
    }

    #[test]
    fn reads_a_pads_net_in_the_numbered_format_too() {
        let old = "(kicad_pcb\n\t(footprint \"R\"\n\t\t(property \"Reference\" \"R1\")\n\
\t\t(pad \"1\" smd rect\n\t\t\t(net 7 \"GND\")\n\t\t)\n\t)\n)\n";
        assert_eq!(
            find_pad_net(&parse_sexp(old).unwrap(), "R1", "1").unwrap(),
            Some("GND".to_string())
        );
    }

    /// The via format is taken verbatim from a real KiCAD 10 board; KiCAD's own
    /// DRC reports one written this way as "Via [+3V3] on F.Cu - B.Cu".
    #[test]
    fn via_is_written_in_the_boards_net_convention() {
        let named = format_via(&NetRef::Named("+3V3".into()), 58.92, 60.0, 0.3, 0.6, "F.Cu", "B.Cu");
        assert!(named.contains("(net \"+3V3\")"), "{named}");
        assert!(named.contains("(size 0.6)") && named.contains("(drill 0.3)"));
        assert!(named.contains("(layers \"F.Cu\" \"B.Cu\")"));
        assert!(!named.contains("net_name"), "vias carry no net_name field");

        let numbered = format_via(&NetRef::Numbered(7, "GND".into()), 1.0, 2.0, 0.3, 0.6, "F.Cu", "B.Cu");
        assert!(numbered.contains("(net 7)"), "{numbered}");

        // Must drop into a board and still parse.
        let board = BOARD.trim_end().trim_end_matches(')').to_string() + &named + "\n)\n";
        konnect_sexp::writer::check_document(&board, "kicad_pcb")
            .expect("a board with the via must still parse");
    }
}

#[cfg(test)]
mod trace_file_tests {
    //! The fixtures are **tab-indented**, like every file KiCAD 10 writes,
    //! while this crate's own writer emits two spaces. A matcher with hardcoded
    //! leading whitespace passes against self-written files and silently finds
    //! nothing in the user's real board — the dominant bug class in this repo.

    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        // Empty ipc_address: nothing in this toolset talks to KiCAD any more,
        // and a test that needed a live KiCAD would prove the opposite.
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    /// KiCAD 10 (20260206): tab-indented, nets referenced by name, no net table.
    /// Contains an unbalanced `(` inside a quoted string and the literal text
    /// `(segment` inside another — both must be invisible to every scanner here.
    fn kicad10_board() -> String {
        [
            "(kicad_pcb",
            "\t(version 20260206)",
            "\t(generator \"pcbnew\")",
            "\t(gr_text \"REV (A\"",
            "\t\t(at 5 5 0)",
            "\t\t(layer \"F.SilkS\")",
            "\t\t(uuid \"text-1\")",
            "\t)",
            "\t(footprint \"Resistor_SMD:R_0603\"",
            "\t\t(layer \"F.Cu\")",
            "\t\t(at 10 10)",
            "\t\t(property \"Reference\" \"R1\")",
            "\t\t(property \"Description\" \"shunt (segment sense)\")",
            "\t\t(pad \"1\" smd rect",
            "\t\t\t(at 0 0)",
            "\t\t\t(net \"+3V3\")",
            "\t\t\t(uuid \"pad-r1-1\")",
            "\t\t)",
            "\t\t(pad \"2\" smd rect",
            "\t\t\t(at 2 0)",
            "\t\t\t(net \"GND\")",
            "\t\t\t(uuid \"pad-r1-2\")",
            "\t\t)",
            "\t)",
            "\t(footprint \"Capacitor_SMD:C_0603\"",
            "\t\t(layer \"F.Cu\")",
            "\t\t(at 20 15)",
            "\t\t(property \"Reference\" \"C1\")",
            "\t\t(pad \"1\" smd rect",
            "\t\t\t(at 0 0)",
            "\t\t\t(net \"+3V3\")",
            "\t\t\t(uuid \"pad-c1-1\")",
            "\t\t)",
            "\t)",
            "\t(segment",
            "\t\t(start 1 1)",
            "\t\t(end 2 2)",
            "\t\t(width 0.2)",
            "\t\t(layer \"B.Cu\")",
            "\t\t(net \"GND\")",
            "\t\t(uuid \"seg-existing\")",
            "\t)",
            "\t(via",
            "\t\t(at 3 3)",
            "\t\t(size 0.6)",
            "\t\t(drill 0.3)",
            "\t\t(layers \"F.Cu\" \"B.Cu\")",
            "\t\t(net \"GND\")",
            "\t\t(uuid \"via-1\")",
            "\t)",
            ")",
            "",
        ]
        .join("\n")
    }

    /// The older format (~20250513): a numbered net table, `(net 7)` references.
    fn numbered_board() -> String {
        [
            "(kicad_pcb",
            "\t(version 20250513)",
            "\t(net 0 \"\")",
            "\t(net 7 \"GND\")",
            "\t(net 8 \"+3V3\")",
            "\t(footprint \"Resistor_SMD:R_0603\"",
            "\t\t(at 10 10)",
            "\t\t(property \"Reference\" \"R1\")",
            "\t\t(pad \"1\" smd rect",
            "\t\t\t(at 0 0)",
            "\t\t\t(net 7 \"GND\")",
            "\t\t)",
            "\t)",
            "\t(segment",
            "\t\t(start 4 4)",
            "\t\t(end 5 5)",
            "\t\t(width 0.25)",
            "\t\t(layer \"F.Cu\")",
            "\t\t(net 7)",
            "\t\t(uuid \"old-seg\")",
            "\t)",
            ")",
            "",
        ]
        .join("\n")
    }

    fn board_file(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let p = dir.join("test.kicad_pcb");
        std::fs::write(&p, content).unwrap();
        p
    }

    fn body(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => match serde_json::from_str(text) {
                Ok(v) => v,
                // Errors are plain text, not JSON.
                Err(_) => json!({ "text": text }),
            },
            _ => panic!("expected text content"),
        }
    }

    fn error_text(result: &CallToolResult) -> String {
        assert!(result.is_error, "expected an error result");
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }

    // ─── Block location ───────────────────────────────────────────────────────

    #[test]
    fn collect_segments_sees_only_real_top_level_segments() {
        let content = kicad10_board();
        let segs = collect_segments(&content);
        // The `(segment` inside the Description string is not a block, and the
        // unbalanced `(` in the gr_text must not shift the depth counter.
        assert_eq!(segs.len(), 1, "{segs:#?}");
        let s = &segs[0];
        assert_eq!(s.uuid.as_deref(), Some("seg-existing"));
        assert_eq!(s.net_name.as_deref(), Some("GND"));
        assert_eq!(s.layer.as_deref(), Some("B.Cu"));
        assert_eq!(s.width, Some(0.2));
        assert_eq!((s.x1, s.y1, s.x2, s.y2), (1.0, 1.0, 2.0, 2.0));
        // The located range really is the block it claims to be.
        assert!(content[s.start..s.end].starts_with("(segment"));
        assert!(content[s.start..s.end].ends_with(')'));
    }

    #[test]
    fn numbered_segments_resolve_their_name_through_the_net_table() {
        let content = numbered_board();
        let segs = collect_segments(&content);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].net_code, Some(7));
        assert_eq!(segs[0].net_name.as_deref(), Some("GND"));
    }

    // ─── Round trip ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn route_query_delete_round_trip_leaves_the_file_byte_intact() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();

        // Route.
        let added = handle_route_trace(
            &json!({
                "board": board.to_str().unwrap(), "net_name": "+3V3", "layer": "F.Cu",
                "x1": 111.8885, "y1": 84.9173, "x2": 110.83, "y2": 85.9758, "width": 0.1
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!added.is_error, "{:?}", body(&added));
        let added = body(&added);
        assert_eq!(added["source"], json!("file"));
        let uuid = added["uuid"].as_str().unwrap().to_string();

        let after_add = std::fs::read_to_string(&board).unwrap();
        check_document(&after_add, "kicad_pcb").expect("board must still parse");
        // KiCAD 10 references the net by name; a numeric field here would be a
        // different net (or none).
        assert!(after_add.contains("(net \"+3V3\")"));
        assert!(!after_add.contains("(net 0)"));

        // Query it back.
        let q = body(
            &handle_query_traces(&json!({ "board": board.to_str().unwrap() }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(q["count"], json!(2));
        let t = q["traces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["uuid"] == json!(uuid))
            .expect("the new trace must come back with its uuid");
        assert_eq!(t["net"], json!("+3V3"));
        assert_eq!(t["layer"], json!("F.Cu"));
        assert_eq!(t["width"], json!(0.1));
        assert_eq!(t["x1"], json!(111.8885));
        assert_eq!(t["y1"], json!(84.9173));
        assert_eq!(t["x2"], json!(110.83));
        assert_eq!(t["y2"], json!(85.9758));

        // Delete exactly it.
        let d = handle_delete_trace(
            &json!({ "board": board.to_str().unwrap(), "uuid": uuid }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!d.is_error, "{:?}", body(&d));

        let after_delete = std::fs::read_to_string(&board).unwrap();
        check_document(&after_delete, "kicad_pcb").expect("board must still parse");
        assert_eq!(
            after_delete, original,
            "deleting the added trace must restore the file byte for byte"
        );
        // The pre-existing segment, via, footprints and text all survive.
        let left = collect_segments(&after_delete);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].uuid.as_deref(), Some("seg-existing"));
        assert!(after_delete.contains("via-1") && after_delete.contains("text-1"));
    }

    #[tokio::test]
    async fn delete_removes_only_the_named_trace_of_several() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let mut uuids = Vec::new();
        for i in 0..3 {
            let r = handle_route_trace(
                &json!({
                    "board": board.to_str().unwrap(), "net_name": "GND", "layer": "F.Cu",
                    "x1": i as f64, "y1": 0.0, "x2": i as f64 + 1.0, "y2": 1.0
                }),
                &ctx,
            )
            .await
            .unwrap();
            uuids.push(body(&r)["uuid"].as_str().unwrap().to_string());
        }

        let r = handle_delete_trace(
            &json!({ "board": board.to_str().unwrap(), "uuid": uuids[1] }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error);

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        let left: Vec<String> = collect_segments(&content)
            .into_iter()
            .filter_map(|s| s.uuid)
            .collect();
        assert_eq!(
            left,
            vec![
                "seg-existing".to_string(),
                uuids[0].clone(),
                uuids[2].clone()
            ]
        );
    }

    #[tokio::test]
    async fn delete_refuses_a_uuid_that_is_not_a_trace() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();

        // A via's uuid: the message must say so rather than deleting something.
        let r = handle_delete_trace(
            &json!({ "board": board.to_str().unwrap(), "uuid": "via-1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(error_text(&r).contains("(via)"), "{}", error_text(&r));

        // A uuid on nothing at all.
        let r = handle_delete_trace(
            &json!({ "board": board.to_str().unwrap(), "uuid": "no-such-uuid" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(error_text(&r).contains("no-such-uuid"));

        // Neither refusal may touch the file — an earlier bug fell back to
        // offset 0 and erased it.
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    // ─── Net formats ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_numbered_board_gets_a_numeric_net_field() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &numbered_board());
        let ctx = test_ctx();

        let r = handle_route_trace(
            &json!({
                "board": board.to_str().unwrap(), "net_name": "GND", "layer": "F.Cu",
                "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 1.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        assert_eq!(body(&r)["net_code"], json!(7));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        // Writing `(net "GND")` on a numbered board is a net the board does not
        // have; writing `(net 0)` is copper connected to nothing.
        assert!(content.contains("\t\t(net 7)\n"), "{content}");
        assert!(!content.contains("(net \"GND\")"));

        // And it reads back with the name resolved through the table.
        let q = body(
            &handle_query_traces(
                &json!({ "board": board.to_str().unwrap(), "net_name": "GND" }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(q["count"], json!(2));
        assert!(q["traces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["net"] == json!("GND") && t["net_code"] == json!(7)));
    }

    #[tokio::test]
    async fn an_unknown_net_is_an_error_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();

        for args in [
            json!({
                "board": board.to_str().unwrap(), "net_name": "VBUS", "layer": "F.Cu",
                "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 1.0
            }),
            json!({
                "board": board.to_str().unwrap(), "net_name": "", "layer": "F.Cu",
                "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 1.0
            }),
        ] {
            let r = handle_route_trace(&args, &ctx).await.unwrap();
            assert!(r.is_error, "an unresolvable net must not be routed");
        }

        // Same for a differential pair, including when only one half is unknown.
        let r = handle_route_diff_pair(
            &json!({
                "board": board.to_str().unwrap(), "net_pos": "+3V3", "net_neg": "USB_D-",
                "x1": 0.0, "y1": 0.0, "x2": 10.0, "y2": 0.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(error_text(&r).contains("USB_D-"));

        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    // ─── The remaining handlers ───────────────────────────────────────────────

    #[tokio::test]
    async fn query_filters_by_net_and_layer() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();
        handle_route_trace(
            &json!({
                "board": board.to_str().unwrap(), "net_name": "+3V3", "layer": "F.Cu",
                "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 1.0
            }),
            &ctx,
        )
        .await
        .unwrap();

        let by_net = body(
            &handle_query_traces(
                &json!({ "board": board.to_str().unwrap(), "net_name": "GND" }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(by_net["count"], json!(1));
        assert_eq!(by_net["traces"][0]["uuid"], json!("seg-existing"));

        let by_layer = body(
            &handle_query_traces(
                &json!({ "board": board.to_str().unwrap(), "layer": "F.Cu" }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(by_layer["count"], json!(1));
        assert_eq!(by_layer["traces"][0]["net"], json!("+3V3"));

        let both = body(
            &handle_query_traces(
                &json!({ "board": board.to_str().unwrap(), "net_name": "GND", "layer": "F.Cu" }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(both["count"], json!(0));
    }

    #[tokio::test]
    async fn modify_rewrites_in_place_and_keeps_the_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let r = handle_modify_trace(
            &json!({
                "board": board.to_str().unwrap(), "uuid": "seg-existing",
                "net_name": "+3V3", "layer": "F.Cu",
                "x1": 7.0, "y1": 7.0, "x2": 8.0, "y2": 8.0, "width": 0.15
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        let segs = collect_segments(&content);
        assert_eq!(segs.len(), 1, "modify must not duplicate the segment");
        let s = &segs[0];
        assert_eq!(s.uuid.as_deref(), Some("seg-existing"));
        assert_eq!(s.net_name.as_deref(), Some("+3V3"));
        assert_eq!(s.layer.as_deref(), Some("F.Cu"));
        assert_eq!(s.width, Some(0.15));
        assert_eq!((s.x1, s.y1, s.x2, s.y2), (7.0, 7.0, 8.0, 8.0));

        // An unknown uuid is refused, not silently appended.
        let r = handle_modify_trace(
            &json!({
                "board": board.to_str().unwrap(), "uuid": "nope",
                "net_name": "+3V3", "layer": "F.Cu",
                "x1": 0.0, "y1": 0.0, "x2": 1.0, "y2": 1.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(r.is_error);
        assert_eq!(std::fs::read_to_string(&board).unwrap(), content);
    }

    #[tokio::test]
    async fn diff_pair_writes_two_parallel_segments() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let r = handle_route_diff_pair(
            &json!({
                "board": board.to_str().unwrap(), "net_pos": "+3V3", "net_neg": "GND",
                "layer": "F.Cu", "x1": 0.0, "y1": 0.0, "x2": 10.0, "y2": 0.0,
                "width": 0.1, "gap": 0.1
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        let b = body(&r);

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        let new: Vec<TraceSegment> = collect_segments(&content)
            .into_iter()
            .filter(|s| s.uuid.as_deref() != Some("seg-existing"))
            .collect();
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].uuid, b["uuid_pos"].as_str().map(String::from));
        assert_eq!(new[1].uuid, b["uuid_neg"].as_str().map(String::from));
        assert_eq!(new[0].net_name.as_deref(), Some("+3V3"));
        assert_eq!(new[1].net_name.as_deref(), Some("GND"));
        // Parallel, (gap + width) apart, either side of the requested line.
        assert!((new[0].y1 - 0.1).abs() < 1e-9, "{:?}", new[0]);
        assert!((new[1].y1 + 0.1).abs() < 1e-9, "{:?}", new[1]);
        assert!((new[0].y1 - new[1].y1 - (0.1 + 0.1)).abs() < 1e-9);
        assert_eq!(new[0].y1, new[0].y2);
        assert_eq!(new[1].y1, new[1].y2);
    }

    #[tokio::test]
    async fn pad_to_pad_writes_the_l_bend_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        // R1.1 and C1.1 are both +3V3 — diagonal, so an L-bend of two segments.
        let r = handle_route_pad_to_pad(
            &json!({
                "board": board.to_str().unwrap(),
                "ref1": "R1", "pad1": "1", "ref2": "C1", "pad2": "1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        let b = body(&r);
        assert_eq!(b["source"], json!("file"));
        assert_eq!(b["net"], json!("+3V3"));
        assert_eq!(b["uuids"].as_array().unwrap().len(), 2);

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        let new: Vec<TraceSegment> = collect_segments(&content)
            .into_iter()
            .filter(|s| s.uuid.as_deref() != Some("seg-existing"))
            .collect();
        assert_eq!(new.len(), 2);
        assert!(new.iter().all(|s| s.net_name.as_deref() == Some("+3V3")));
        // Contiguous: the bend's end is the second segment's start.
        assert_eq!((new[0].x2, new[0].y2), (new[1].x1, new[1].y1));
        assert_eq!((new[0].x1, new[0].y1), (10.0, 10.0));
        assert_eq!((new[1].x2, new[1].y2), (20.0, 15.0));
    }

    #[tokio::test]
    async fn pad_to_pad_still_refuses_to_short_two_nets() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();

        // R1.1 is +3V3, R1.2 is GND.
        let r = handle_route_pad_to_pad(
            &json!({
                "board": board.to_str().unwrap(),
                "ref1": "R1", "pad1": "1", "ref2": "R1", "pad2": "2"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(error_text(&r).contains("short"), "{}", error_text(&r));

        // A net_name that disagrees with the board is refused too.
        let r = handle_route_pad_to_pad(
            &json!({
                "board": board.to_str().unwrap(), "net_name": "GND",
                "ref1": "R1", "pad1": "1", "ref2": "C1", "pad2": "1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(r.is_error);

        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn nets_list_comes_from_the_pads() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx();

        // KiCAD 10: no net table at all, so the pads are the only record.
        let board = board_file(dir.path(), &kicad10_board());
        let b = body(
            &handle_get_nets_list(&json!({ "board": board.to_str().unwrap() }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(b["source"], json!("file"));
        assert_eq!(b["count"], json!(2));
        let names: Vec<&str> = b["nets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["+3V3", "GND"]);
        assert!(b["nets"][0]["netcode"].is_null());

        // Numbered board: the codes come back with the names.
        let dir2 = tempfile::tempdir().unwrap();
        let old = board_file(dir2.path(), &numbered_board());
        let b = body(
            &handle_get_nets_list(&json!({ "board": old.to_str().unwrap() }), &ctx)
                .await
                .unwrap(),
        );
        // The unnamed net 0 is not a net anyone can route to.
        assert_eq!(b["count"], json!(2));
        assert_eq!(b["nets"][0]["name"], json!("+3V3"));
        assert_eq!(b["nets"][0]["netcode"], json!(8));
        assert_eq!(b["nets"][1]["name"], json!("GND"));
        assert_eq!(b["nets"][1]["netcode"], json!(7));
    }
}

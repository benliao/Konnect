//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Every tool here edits the `.kicad_pcb` file directly. Routing used to run
//! over the KiCAD IPC API while vias were written to disk, so a session that
//! used both left KiCAD's in-memory board and the file disagreeing — whichever
//! saved last silently won. One backend means the `board` argument always names
//! the file that is actually changed, and the tools work headless.
//!
//! Writes go through `crate::tools::write_board_synced`, so a KiCAD that has
//! this exact board open saves before and reloads after; with no KiCAD (or a
//! different board open) it is a plain validated write.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, require_f64, require_str, write_board_synced, BoardSync, NetRef, ToolContext, ToolDef,
};
use konnect_sexp::parser::{parse_sexp, SexpNode};
use konnect_sexp::writer::{
    apply_edits, check_document, find_balanced_block, find_block_starts,
    find_block_with_leading_whitespace, new_uuid, SexpEdit,
};
use serde_json::json;
use std::path::Path;

/// What to tell the caller about KiCAD's own window after a write.
///
/// KiCAD reloads a board from disk only on open/revert, so a stale editor that
/// later saves would undo the write; `write_board_synced` normally handles that,
/// and this says so when it could not.
fn write_note(sync: &BoardSync) -> String {
    sync.note().unwrap_or_else(|| {
        match sync {
            BoardSync::Reloaded => "Written to the file; KiCAD reloaded the board.",
            _ => {
                "Written to the file. KiCAD does not have this board open, so nothing \
                  needed reloading."
            }
        }
        .to_string()
    })
}

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
    rest.strip_prefix(tag).is_some_and(|after| {
        after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '(' || c == ')')
    })
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
fn read_net_node(
    node: &SexpNode,
    table: &std::collections::HashMap<i32, String>,
) -> (Option<i32>, Option<String>) {
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
/// The write goes through `write_board_synced`, so a KiCAD that has this exact
/// board open flushes first and reloads after instead of holding a stale copy
/// it would later save over the top.
async fn commit(
    ipc_address: &str,
    board: &Path,
    content: String,
    edits: Vec<SexpEdit>,
) -> Result<BoardSync, CallToolResult> {
    let new_content = apply_edits(content, edits);
    if let Err(why) = check_document(&new_content, "kicad_pcb") {
        return Err(CallToolResult::error(format!(
            "Internal error: the edit would have corrupted '{}' ({why}) — nothing was written.",
            board.display()
        )));
    }
    write_board_synced(ipc_address, board, &new_content)
        .await
        .map_err(|e| CallToolResult::error(format!("Failed to write '{}': {e}", board.display())))
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
        tool!(
            "query_vias",
            "List the vias on the board, read from the .kicad_pcb file, optionally filtered by \
             net and/or layer. Each entry carries the uuid that delete_via takes.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer — matches either end of the via's layer pair (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_vias(args, ctx).await }
        ),
        tool!(
            "delete_via",
            "Delete a via identified by its uuid (as returned by query_vias) from the .kicad_pcb \
             file.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the via to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_via(args, ctx).await }
        ),
        tool!(
            "query_zones",
            "List the copper zones on the board, read from the .kicad_pcb file: net, layer, \
             outline size and whether the pour is filled. `has_net: false` marks a zone that \
             belongs to no net — copper connected to nothing, which DRC does not flag — and \
             set_zone_net repairs it.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "net_name": { "type": "string", "description": "Filter by net (optional)" },
                    "layer":    { "type": "string", "description": "Filter by layer (optional)" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_query_zones(args, ctx).await }
        ),
        tool!(
            "set_zone_net",
            "Reassign a copper zone (identified by uuid) to an existing net, rewriting the net \
             fields in the board's own format. This is the repair for a pour that query_zones \
             reports as has_net: false. The net must already exist on the board.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string" },
                    "uuid":     { "type": "string", "description": "UUID of the zone to reassign" },
                    "net_name": { "type": "string", "description": "Net the pour should belong to" }
                },
                "required": ["board", "uuid", "net_name"]
            }),
            |args, ctx| async move { handle_set_zone_net(args, ctx).await }
        ),
        tool!(
            "delete_zone",
            "Delete a copper zone identified by its uuid (as returned by query_zones) from the \
             .kicad_pcb file.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "uuid":  { "type": "string", "description": "UUID of the zone to delete" }
                },
                "required": ["board", "uuid"]
            }),
            |args, ctx| async move { handle_delete_zone(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_add_net(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(close_pos, net_sexp)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "net_id": net_id, "net_name": net_name,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_route_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(close, block)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "uuid": uuid,
        "net": net.name(), "net_code": net.code(),
        "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_route_pad_to_pad(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
            blocks.push_str(&top_level_segment(
                &net, &layer, width, x1, y1, x2, y2, &uuid,
            ));
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

    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(close, blocks)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "uuids": uuids,
        "net": net.name(), "net_code": net.code(),
        "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
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
/// Returns the block and the uuid it was given, so the caller can hand the
/// uuid back — `delete_via` and `query_vias` are keyed on it, and returning it
/// saves a round-trip through `query_vias` just to learn what was created.
fn format_via(
    net: &crate::tools::NetRef,
    x: f64,
    y: f64,
    drill: f64,
    size: f64,
    start_layer: &str,
    end_layer: &str,
) -> (String, String) {
    let uuid = new_uuid();
    // The via's net field follows the board's own convention, same as a zone's.
    let net_field = net_field(net);
    let block = format!(
        "\n\t(via\n\t\t(at {x} {y})\n\t\t(size {size})\n\t\t(drill {drill})\
         \n\t\t(layers \"{start_layer}\" \"{end_layer}\")\n\t\t{net_field}\
         \n\t\t(uuid \"{uuid}\")\n\t)"
    );
    (block, uuid)
}

async fn handle_add_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
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

    let (via, via_uuid) = format_via(&net, x, y, drill, pad_size, start_layer, end_layer);
    let close_pos = match root_close_or_error(&content, &board_path) {
        Ok(p) => p,
        Err(e) => return Ok(e),
    };
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(close_pos, via)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "uuid": via_uuid,
        "net": net.name(), "net_code": net.code(), "x": x, "y": y,
        "drill": drill, "pad_size": pad_size,
        "layers": [start_layer, end_layer],
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_add_copper_pour(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(close, zone_s)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "net": net.name(), "layer": layer, "points": pts.len(),
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
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

    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::delete(del_start, del_end)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "deleted_uuid": uuid,
        "net": seg.net_name, "layer": seg.layer, "width": seg.width,
        "from": { "x": seg.x1, "y": seg.y1 }, "to": { "x": seg.x2, "y": seg.y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
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
        .filter(|s| {
            net.as_deref()
                .is_none_or(|n| s.net_name.as_deref() == Some(n))
        })
        .filter(|s| {
            layer
                .as_deref()
                .is_none_or(|l| s.layer.as_deref() == Some(l))
        })
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
            let Some(node) = pad.find("net") else {
                continue;
            };
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
    ctx: &ToolContext,
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
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::replace(seg.start, seg.end, block)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net.name(), "net_code": net.code(),
        "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 },
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_create_netclass(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
    // Inside the board's own (net_classes …)/(net_settings …) block if it has
    // one, otherwise as a new top-level item. Located by balanced-paren search:
    // the old `block.find("\n    )")` matched nothing in a tab-indented file and
    // fell back to the *first* `)` after `(net_classes`, which nested the new
    // netclass inside whichever entry happened to be first.
    let container = ["net_classes", "net_settings"].iter().find_map(|tag| {
        find_block_starts(&content, tag)
            .into_iter()
            .find_map(|s| find_balanced_block(&content, s))
    });
    let insert_pos = match container {
        Some((_, end)) => end - 1,
        None => match root_close_or_error(&content, &board_path) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        },
    };

    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(insert_pos, netclass_sexp)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "created_netclass": name,
        "clearance": clearance, "trace_width": trace_width,
        "via_drill": via_drill, "via_diameter": via_dia,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_assign_net_to_class(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(nc_end, net_entry)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    ctx: &ToolContext,
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
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::insert(close, blocks)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "net_pos": np.name(), "net_neg": nn.name(),
        "uuid_pos": uuid_pos, "uuid_neg": uuid_neg,
        "layer": layer, "width": width, "gap": gap,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

// ─── Vias and zones already on the board ─────────────────────────────────────
//
// Vias and pours used to be write-only: a tool could create one and nothing
// could look at it again, so a mistake could only be undone by hand in KiCAD.
// The worst case is silent — a `(zone (net 0) (net_name "GND") …)` is a ground
// plane joined to nothing, and DRC does not flag isolated copper — so reading
// and repairing them has to be possible from here.

/// Byte offsets of every **direct child** `(tag …)` of the block opening at
/// `block_start`, in file order.
///
/// The same string-aware scan as [`top_level_block_starts`], scoped to one
/// block: a zone's own `(net …)` is a direct child, one buried deeper inside it
/// is not, and neither is anything that merely looks like a block inside a
/// quoted string.
fn child_block_starts(content: &str, block_start: usize, block_end: usize, tag: &str) -> Vec<usize> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut i) = (0usize, block_start);
    let (mut in_string, mut escape) = (false, false);
    let stop = block_end.min(bytes.len());

    while i < stop {
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
                    // The block's own `(` is seen at depth 0, so its direct
                    // children are the ones that open at depth 1.
                    if depth == 1 && tag_opens_at(content, i, tag) {
                        out.push(i);
                    }
                    depth += 1;
                }
                b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break; // the block's own closing paren
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    out
}

/// The string arguments of a `(tag "a" "b" …)` node — a via's layer pair, say.
fn node_strings(node: &SexpNode) -> Vec<String> {
    node.children()
        .unwrap_or(&[])
        .iter()
        .skip(1)
        .filter_map(|n| n.as_str())
        .map(str::to_string)
        .collect()
}

/// The indentation of the line `offset` sits on, when only whitespace precedes
/// it there; `None` when the offset is mid-line.
///
/// Lets an inserted field copy the file's own layout without ever assuming
/// *which* whitespace it uses: KiCAD 10 indents with tabs and this crate's
/// writer with two spaces.
fn line_indent_at(content: &str, offset: usize) -> Option<&str> {
    let line_start = content.get(..offset)?.rfind('\n').map(|i| i + 1)?;
    let ws = content.get(line_start..offset)?;
    ws.chars().all(|c| c == ' ' || c == '\t').then_some(ws)
}

/// A `(via …)` as it exists on the board, with the byte range it occupies.
#[derive(Debug, Clone)]
struct BoardVia {
    start: usize,
    end: usize,
    uuid: Option<String>,
    net_code: Option<i32>,
    net_name: Option<String>,
    x: f64,
    y: f64,
    size: Option<f64>,
    drill: Option<f64>,
    layers: Vec<String>,
}

/// Every **top-level** `(via …)`, in file order. A via inside a footprint is
/// part of that footprint's definition and is never touched here.
fn collect_vias(content: &str) -> Vec<BoardVia> {
    let table = net_table(content);
    top_level_block_starts(content, "via")
        .into_iter()
        .filter_map(|s| {
            let (start, end) = find_balanced_block(content, s)?;
            let node = parse_sexp(&content[start..end]).ok()?;
            let (net_code, net_name) = node
                .find("net")
                .map(|n| read_net_node(n, &table))
                .unwrap_or((None, None));
            let at = node.find("at");
            Some(BoardVia {
                start,
                end,
                uuid: node.find_str("uuid").map(str::to_string),
                net_code,
                net_name,
                x: at.and_then(|a| a.get_f64(1)).unwrap_or(0.0),
                y: at.and_then(|a| a.get_f64(2)).unwrap_or(0.0),
                size: node.find_f64("size"),
                drill: node.find_f64("drill"),
                layers: node.find("layers").map(node_strings).unwrap_or_default(),
            })
        })
        .collect()
}

/// A `(zone …)` as it exists on the board, with the byte range it occupies.
#[derive(Debug, Clone)]
struct BoardZone {
    start: usize,
    end: usize,
    uuid: Option<String>,
    net_code: Option<i32>,
    /// The name the `(net …)` node itself carries (via the numbered table where
    /// the board has one).
    net_name: Option<String>,
    /// The separate `(net_name "…")` label of the numbered format. It is only a
    /// label: `(net 0) (net_name "GND")` is *not* on GND.
    net_label: Option<String>,
    layers: Vec<String>,
    points: usize,
    bbox: Option<(f64, f64, f64, f64)>,
    filled: bool,
    /// A `(keepout …)` zone is a rule area, not copper. It carries no net by
    /// design, so it must never be reported as a netless pour — telling a
    /// caller to "repair" one with set_zone_net would turn a keepout into a
    /// copper pour and silently damage the board.
    keepout: bool,
    /// The zone's `(name "…")`, e.g. "ANT_KEEPOUT".
    name: Option<String>,
}

impl BoardZone {
    /// Whether this pour actually belongs to a net.
    ///
    /// Net 0 is KiCAD's "no net", and stays that even with a stale
    /// `(net_name "GND")` beside it — the exact shape of the isolated ground
    /// plane `set_zone_net` exists to repair.
    fn has_net(&self) -> bool {
        match self.net_code {
            Some(0) => false,
            Some(_) => true,
            None => self.net_name.as_deref().is_some_and(|n| !n.is_empty()),
        }
    }

    /// Whether this is a copper pour that should be on a net but is not.
    /// Keepouts are rule areas and are excluded.
    fn is_orphaned_pour(&self) -> bool {
        !self.keepout && !self.has_net()
    }

    /// The net this zone is on, or `None` when it is on none. The stale label
    /// of a net-less zone is deliberately not reported as its net.
    fn net(&self) -> Option<&str> {
        self.has_net().then_some(self.net_name.as_deref()).flatten()
    }
}

/// Every **top-level** `(zone …)`, in file order.
fn collect_zones(content: &str) -> Vec<BoardZone> {
    let table = net_table(content);
    top_level_block_starts(content, "zone")
        .into_iter()
        .filter_map(|s| {
            let (start, end) = find_balanced_block(content, s)?;
            let node = parse_sexp(&content[start..end]).ok()?;
            let (net_code, net_name) = node
                .find("net")
                .map(|n| read_net_node(n, &table))
                .unwrap_or((None, None));

            // A zone is on one layer, `(layer "In1.Cu")`, or several,
            // `(layers "F.Cu" "B.Cu")`.
            let mut layers: Vec<String> = node
                .find("layer")
                .and_then(|n| n.get(1))
                .and_then(|n| n.as_str())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default();
            if let Some(multi) = node.find("layers") {
                layers.extend(node_strings(multi));
            }

            // The outline, not the fill: `(filled_polygon …)` is KiCAD's cached
            // copper and is regenerated by the next fill.
            let mut pts: Vec<(f64, f64)> = Vec::new();
            for poly in node.find_all("polygon") {
                let Some(list) = poly.find("pts") else { continue };
                for xy in list.find_all("xy") {
                    if let (Some(x), Some(y)) = (xy.get_f64(1), xy.get_f64(2)) {
                        pts.push((x, y));
                    }
                }
            }
            let bbox = pts.iter().fold(None, |acc: Option<(f64, f64, f64, f64)>, &(x, y)| {
                Some(match acc {
                    None => (x, y, x, y),
                    Some((min_x, min_y, max_x, max_y)) => {
                        (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
                    }
                })
            });

            Some(BoardZone {
                keepout: node.find("keepout").is_some(),
                name: node.find_str("name").map(str::to_string),
                start,
                end,
                uuid: node.find_str("uuid").map(str::to_string),
                net_code,
                net_name,
                net_label: node.find_str("net_name").map(str::to_string),
                layers,
                points: pts.len(),
                bbox,
                filled: node.find("filled_polygon").is_some(),
            })
        })
        .collect()
}

/// Why a uuid did not name a `want` — the uuid may belong to another kind of
/// item, and a bare "not found" sends the caller looking in the wrong place.
fn unknown_item_message(content: &str, uuid: &str, board: &Path, want: &str, tool: &str) -> String {
    for tag in ["segment", "via", "zone", "arc", "footprint", "gr_line"] {
        if tag == want {
            continue;
        }
        let owns = top_level_block_starts(content, tag).into_iter().any(|s| {
            find_balanced_block(content, s)
                .and_then(|(a, b)| parse_sexp(&content[a..b]).ok())
                .and_then(|n| n.find_str("uuid").map(str::to_string))
                .as_deref()
                == Some(uuid)
        });
        if owns {
            return format!(
                "'{uuid}' is the uuid of a ({tag}) on '{}', not a ({want}). This tool \
                 only deletes ({want} …) items.",
                board.display()
            );
        }
    }
    format!(
        "No ({want}) with uuid '{uuid}' on '{}'. Run {tool} to list the items actually \
         on this board.",
        board.display()
    )
}

/// The byte range to delete for the top-level block at `block_start`, or the
/// reason to refuse.
///
/// Never falls back to a default offset: an earlier version of the delete path
/// defaulted to 0 when it could not locate a block and erased the whole file, so
/// a lookup that does not land exactly on the block stays a failed tool call.
fn delete_range(
    content: &str,
    block_start: usize,
    block_end: usize,
    what: &str,
    board: &Path,
) -> Result<(usize, usize), String> {
    let Some((del_start, del_end)) = find_block_with_leading_whitespace(content, block_start) else {
        return Err(format!(
            "Refusing to delete {what}: its byte range in '{}' could not be established. \
             Nothing was written.",
            board.display()
        ));
    };
    if del_start > block_start || del_end != block_end {
        return Err(format!(
            "Refusing to delete {what}: the computed range {del_start}..{del_end} does not \
             match the block at {block_start}..{block_end}. Nothing was written."
        ));
    }
    Ok((del_start, del_end))
}

/// The edits that put `zone` on `net`, written in the board's own convention.
///
/// The two formats are not interchangeable, and mixing them is how a pour ends
/// up belonging to nothing:
///
/// * numbered boards (up to ~20250513) want `(net 7) (net_name "GND")`, the
///   number being what actually connects the copper;
/// * KiCAD 10 (20260206+) dropped the table and wants `(net "GND")` alone — any
///   surviving `(net_name …)` is a leftover label from the old format and is
///   removed, because a label next to a `(net 0)` is precisely what makes a
///   disconnected pour look connected.
fn zone_net_edits(
    content: &str,
    board: &Path,
    zone: &BoardZone,
    net: &NetRef,
) -> Result<Vec<SexpEdit>, String> {
    let net_range = child_block_starts(content, zone.start, zone.end, "net")
        .into_iter()
        .next()
        .and_then(|s| find_balanced_block(content, s));
    let label_range = child_block_starts(content, zone.start, zone.end, "net_name")
        .into_iter()
        .next()
        .and_then(|s| find_balanced_block(content, s));

    let (field, label) = match net {
        NetRef::Named(name) => (format!("(net \"{name}\")"), None),
        NetRef::Numbered(id, name) => {
            (format!("(net {id})"), Some(format!("(net_name \"{name}\")")))
        }
    };

    let mut edits = Vec::new();
    // Where the net field ends up, and where a `(net_name …)` would follow it.
    let (field_start, field_end) = match net_range {
        Some((s, e)) => {
            edits.push(SexpEdit::replace(s, e, field));
            (s, e)
        }
        None => {
            // A zone with no `(net …)` at all — put one directly after the tag
            // rather than guessing at a position further in.
            let after_tag = zone.start + "(zone".len();
            if !content.is_char_boundary(after_tag) || !content[zone.start..].starts_with("(zone") {
                return Err(format!(
                    "Refusing to set the net: the zone block at {} is not a (zone …). \
                     Nothing was written.",
                    zone.start
                ));
            }
            edits.push(SexpEdit::insert(after_tag, format!(" {field}")));
            (after_tag, after_tag)
        }
    };

    match (label, label_range) {
        // Numbered board, label already there: keep it in step with the number.
        (Some(label), Some((s, e))) => edits.push(SexpEdit::replace(s, e, label)),
        // Numbered board, no label yet: follow the file's own layout — its own
        // line where fields are on their own lines, inline where they are not.
        (Some(label), None) => {
            let sep = line_indent_at(content, field_start)
                .map(|ws| format!("\n{ws}"))
                .unwrap_or_else(|| " ".to_string());
            edits.push(SexpEdit::insert(field_end, format!("{sep}{label}")));
        }
        // KiCAD 10: drop the stale label rather than leaving it to contradict
        // the net field.
        (None, Some((s, e))) => {
            let (del_start, del_end) =
                delete_range(content, s, e, "the zone's stale (net_name …)", board)?;
            // It must sit after the net field, or the two edits would overlap.
            if del_start < field_end {
                return Err(format!(
                    "Refusing to rewrite the zone's net: its (net_name …) at {del_start} \
                     overlaps the (net …) field at {field_start}..{field_end}. Nothing was \
                     written."
                ));
            }
            edits.push(SexpEdit::delete(del_start, del_end));
        }
        (None, None) => {}
    }

    Ok(edits)
}

async fn handle_query_vias(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let content = std::fs::read_to_string(&board_path)?;
    let items: Vec<serde_json::Value> = collect_vias(&content)
        .into_iter()
        .filter(|v| net.as_deref().is_none_or(|n| v.net_name.as_deref() == Some(n)))
        // A via spans a layer pair, so either end matching is a match.
        .filter(|v| {
            layer
                .as_deref()
                .is_none_or(|l| v.layers.iter().any(|have| have == l))
        })
        .map(|v| {
            json!({
                // uuid is what delete_via takes — without it a queried via
                // could not be removed.
                "uuid": v.uuid,
                "net": v.net_name, "net_code": v.net_code,
                "x": v.x, "y": v.y,
                "size": v.size, "drill": v.drill,
                "layers": v.layers
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "vias": items,
        "target": board_path.display().to_string(),
        "source": "file"
    })))
}

async fn handle_delete_via(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let Some(via) = collect_vias(&content)
        .into_iter()
        .find(|v| v.uuid.as_deref() == Some(uuid.as_str()))
    else {
        return Ok(CallToolResult::error(unknown_item_message(
            &content,
            &uuid,
            &board_path,
            "via",
            "query_vias",
        )));
    };

    let (del_start, del_end) = match delete_range(
        &content,
        via.start,
        via.end,
        &format!("via {uuid}"),
        &board_path,
    ) {
        Ok(r) => r,
        Err(e) => return Ok(CallToolResult::error(e)),
    };

    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::delete(del_start, del_end)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "deleted_uuid": uuid,
        "net": via.net_name, "net_code": via.net_code,
        "x": via.x, "y": via.y,
        "size": via.size, "drill": via.drill, "layers": via.layers,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

/// One zone as JSON, shared by `query_zones` and the tools that report a zone
/// they just changed.
fn zone_json(z: &BoardZone) -> serde_json::Value {
    json!({
        // uuid is what set_zone_net and delete_zone take.
        "uuid": z.uuid,
        "net": z.net(), "net_code": z.net_code,
        // A pour on no net is copper connected to nothing. DRC does not flag
        // isolated copper, so this flag is the only warning there is.
        "has_net": z.has_net(),
        // A keepout is a rule area, not copper; it carries no net by design.
        "keepout": z.keepout,
        "name": z.name,
        // The `(net_name …)` label as written, which on a net-less zone is the
        // net it was *meant* to be on.
        "net_label": z.net_label,
        "layers": z.layers,
        "points": z.points,
        "bbox": z.bbox.map(|(min_x, min_y, max_x, max_y)| json!({
            "min_x": min_x, "min_y": min_y, "max_x": max_x, "max_y": max_y
        })),
        "filled": z.filled
    })
}

async fn handle_query_zones(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let content = std::fs::read_to_string(&board_path)?;
    let zones: Vec<BoardZone> = collect_zones(&content)
        .into_iter()
        .filter(|z| net.as_deref().is_none_or(|n| z.net() == Some(n)))
        .filter(|z| {
            layer
                .as_deref()
                .is_none_or(|l| z.layers.iter().any(|have| have == l))
        })
        .collect();

    // Keepouts are excluded: they legitimately have no net, and "repairing" one
    // with set_zone_net would turn a rule area into a copper pour.
    let netless: Vec<String> = zones
        .iter()
        .filter(|z| z.is_orphaned_pour())
        .filter_map(|z| z.uuid.clone())
        .collect();
    let items: Vec<serde_json::Value> = zones.iter().map(zone_json).collect();

    let mut out = json!({
        "count": items.len(),
        "zones": items,
        "netless_zones": netless.len(),
        "target": board_path.display().to_string(),
        "source": "file"
    });
    if !netless.is_empty() {
        out["note"] = json!(format!(
            "{} zone(s) belong to no net — copper connected to nothing, which DRC does not \
             flag. Fix each with set_zone_net(uuid, net_name): {}",
            netless.len(),
            netless.join(", ")
        ));
    }
    Ok(CallToolResult::json(&out))
}

async fn handle_set_zone_net(
    args: &serde_json::Value,
    ctx: &ToolContext,
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

    let content = std::fs::read_to_string(&board_path)?;
    let Some(zone) = collect_zones(&content)
        .into_iter()
        .find(|z| z.uuid.as_deref() == Some(uuid.as_str()))
    else {
        return Ok(CallToolResult::error(unknown_item_message(
            &content,
            &uuid,
            &board_path,
            "zone",
            "query_zones",
        )));
    };
    // A keepout is a rule area, not copper. Giving it a net converts it into a
    // pour — an antenna or connector clearance area silently becomes filled
    // copper. Refuse rather than let a caller "repair" one.
    if zone.keepout {
        return Ok(CallToolResult::error(format!(
            "Zone {uuid}{} is a keepout (rule area), not a copper pour. Keepouts \
             carry no net by design; assigning one would turn it into copper. \
             Use delete_zone if you meant to remove it.",
            zone.name
                .as_deref()
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default()
        )));
    }

    // The whole point of this tool is that a pour belongs to a real net, so an
    // unresolvable name is an error — never a fallback to net 0, which is the
    // defect being repaired.
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };

    let was_net = zone.net().map(str::to_string);
    let was_code = zone.net_code;
    let had_net = zone.has_net();

    let edits = match zone_net_edits(&content, &board_path, &zone, &net) {
        Ok(e) => e,
        Err(e) => return Ok(CallToolResult::error(e)),
    };
    let sync = match commit(&ctx.config.ipc_address, &board_path, content, edits).await {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "uuid": uuid,
        "net": net.name(), "net_code": net.code(),
        "previous_net": was_net, "previous_net_code": was_code,
        "previously_had_net": had_net,
        "layers": zone.layers,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        // The stored fill is the old net's copper until KiCAD refills it.
        "refill_required": zone.filled,
        "note": write_note(&sync)
    })))
}

async fn handle_delete_zone(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let Some(zone) = collect_zones(&content)
        .into_iter()
        .find(|z| z.uuid.as_deref() == Some(uuid.as_str()))
    else {
        return Ok(CallToolResult::error(unknown_item_message(
            &content,
            &uuid,
            &board_path,
            "zone",
            "query_zones",
        )));
    };

    let (del_start, del_end) = match delete_range(
        &content,
        zone.start,
        zone.end,
        &format!("zone {uuid}"),
        &board_path,
    ) {
        Ok(r) => r,
        Err(e) => return Ok(CallToolResult::error(e)),
    };

    let removed = zone_json(&zone);
    let sync = match commit(
        &ctx.config.ipc_address,
        &board_path,
        content,
        vec![SexpEdit::delete(del_start, del_end)],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "deleted_uuid": uuid,
        "deleted": removed,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
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
        let (named, _) = format_via(
            &NetRef::Named("+3V3".into()),
            58.92,
            60.0,
            0.3,
            0.6,
            "F.Cu",
            "B.Cu",
        );
        assert!(named.contains("(net \"+3V3\")"), "{named}");
        assert!(named.contains("(size 0.6)") && named.contains("(drill 0.3)"));
        assert!(named.contains("(layers \"F.Cu\" \"B.Cu\")"));
        assert!(!named.contains("net_name"), "vias carry no net_name field");

        let (numbered, _) = format_via(
            &NetRef::Numbered(7, "GND".into()),
            1.0,
            2.0,
            0.3,
            0.6,
            "F.Cu",
            "B.Cu",
        );
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
        // No KiCAD to synchronise with: the write still lands, which is what
        // makes the toolset work headless.
        assert_eq!(added["kicad_sync"], json!("not_open"));
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

#[cfg(test)]
mod via_zone_file_tests {
    //! Vias and pours, read back and repaired.
    //!
    //! The fixtures are **tab-indented**, like every file KiCAD writes, and come
    //! in both net formats. They also carry a via and a zone *inside* a
    //! footprint, an unbalanced `(` inside a quoted string, and the literal text
    //! `(zone` inside another — all of which must be invisible to every scanner
    //! here.

    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        // Empty ipc_address: these tools are file-based, and a test that needed
        // a live KiCAD would prove the opposite.
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

    /// KiCAD 10 (20260206): nets by name, no net table. `zone-broken` is the
    /// defect from the field report — an In1.Cu pour left on net 0 with a
    /// leftover `(net_name "GND")` label, i.e. a ground plane joined to nothing.
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
            "\t\t(property \"Description\" \"shunt (zone sense)\")",
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
            "\t\t(zone",
            "\t\t\t(net \"GND\")",
            "\t\t\t(layer \"F.Cu\")",
            "\t\t\t(uuid \"fp-zone\")",
            "\t\t)",
            "\t\t(via",
            "\t\t\t(at 1 1)",
            "\t\t\t(size 0.6)",
            "\t\t\t(drill 0.3)",
            "\t\t\t(layers \"F.Cu\" \"B.Cu\")",
            "\t\t\t(net \"GND\")",
            "\t\t\t(uuid \"fp-via\")",
            "\t\t)",
            "\t)",
            "\t(via",
            "\t\t(at 3 3)",
            "\t\t(size 0.6)",
            "\t\t(drill 0.3)",
            "\t\t(layers \"F.Cu\" \"B.Cu\")",
            "\t\t(net \"GND\")",
            "\t\t(uuid \"via-1\")",
            "\t)",
            "\t(via",
            "\t\t(at 4 4)",
            "\t\t(size 0.8)",
            "\t\t(drill 0.4)",
            "\t\t(layers \"F.Cu\" \"In1.Cu\")",
            "\t\t(net \"+3V3\")",
            "\t\t(uuid \"via-2\")",
            "\t)",
            "\t(zone",
            "\t\t(net 0)",
            "\t\t(net_name \"GND\")",
            "\t\t(layer \"In1.Cu\")",
            "\t\t(uuid \"zone-broken\")",
            "\t\t(hatch edge 0.508)",
            "\t\t(polygon",
            "\t\t\t(pts",
            "\t\t\t\t(xy 0 0)",
            "\t\t\t\t(xy 20 0)",
            "\t\t\t\t(xy 20 20)",
            "\t\t\t\t(xy 0 20)",
            "\t\t\t)",
            "\t\t)",
            "\t\t(filled_polygon",
            "\t\t\t(layer \"In1.Cu\")",
            "\t\t\t(pts",
            "\t\t\t\t(xy 1 1)",
            "\t\t\t\t(xy 19 1)",
            "\t\t\t\t(xy 19 19)",
            "\t\t\t)",
            "\t\t)",
            "\t)",
            "\t(zone",
            "\t\t(net \"+3V3\")",
            "\t\t(layer \"F.Cu\")",
            "\t\t(uuid \"zone-ok\")",
            "\t\t(polygon",
            "\t\t\t(pts",
            "\t\t\t\t(xy 30 30)",
            "\t\t\t\t(xy 40 30)",
            "\t\t\t\t(xy 40 40)",
            "\t\t\t)",
            "\t\t)",
            "\t)",
            ")",
            "",
        ]
        .join("\n")
    }

    /// The older format (~20250513): a numbered net table, `(net 7)` references
    /// plus a separate `(net_name …)` label. `zone-nolabel` has no label at all,
    /// so repairing it has to add one.
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
            "\t\t(zone",
            "\t\t\t(net 7)",
            "\t\t\t(net_name \"GND\")",
            "\t\t\t(layer \"F.Cu\")",
            "\t\t\t(uuid \"fp-zone\")",
            "\t\t)",
            "\t)",
            "\t(via",
            "\t\t(at 3 3)",
            "\t\t(size 0.6)",
            "\t\t(drill 0.3)",
            "\t\t(layers \"F.Cu\" \"B.Cu\")",
            "\t\t(net 7)",
            "\t\t(uuid \"via-old\")",
            "\t)",
            "\t(zone",
            "\t\t(net 0)",
            "\t\t(net_name \"GND\")",
            "\t\t(layer \"In1.Cu\")",
            "\t\t(uuid \"zone-broken\")",
            "\t\t(polygon",
            "\t\t\t(pts",
            "\t\t\t\t(xy 0 0)",
            "\t\t\t\t(xy 10 0)",
            "\t\t\t\t(xy 10 10)",
            "\t\t\t)",
            "\t\t)",
            "\t)",
            "\t(zone",
            "\t\t(net 0)",
            "\t\t(layer \"In2.Cu\")",
            "\t\t(uuid \"zone-nolabel\")",
            "\t\t(polygon",
            "\t\t\t(pts",
            "\t\t\t\t(xy 0 0)",
            "\t\t\t\t(xy 5 0)",
            "\t\t\t\t(xy 5 5)",
            "\t\t\t)",
            "\t\t)",
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

    fn zone_by_uuid(content: &str, uuid: &str) -> BoardZone {
        collect_zones(content)
            .into_iter()
            .find(|z| z.uuid.as_deref() == Some(uuid))
            .unwrap_or_else(|| panic!("no zone '{uuid}' in:\n{content}"))
    }

    // ─── Block location ───────────────────────────────────────────────────────

    #[test]
    fn collectors_see_only_top_level_items() {
        let content = kicad10_board();

        // The via and zone inside the footprint belong to that footprint's
        // definition; deleting or re-netting them here would edit a component.
        let vias: Vec<String> = collect_vias(&content)
            .into_iter()
            .filter_map(|v| v.uuid)
            .collect();
        assert_eq!(vias, vec!["via-1".to_string(), "via-2".to_string()]);

        let zones: Vec<String> = collect_zones(&content)
            .into_iter()
            .filter_map(|z| z.uuid)
            .collect();
        assert_eq!(
            zones,
            vec!["zone-broken".to_string(), "zone-ok".to_string()],
            "the footprint's zone and the `(zone` inside a quoted string are not board items"
        );

        // The located ranges really are the blocks they claim to be.
        for z in collect_zones(&content) {
            assert!(content[z.start..z.end].starts_with("(zone"));
            assert!(content[z.start..z.end].ends_with(')'));
        }
        for v in collect_vias(&content) {
            assert!(content[v.start..v.end].starts_with("(via"));
        }
    }

    #[test]
    fn a_zone_reports_its_geometry_fill_and_missing_net() {
        let content = kicad10_board();

        let broken = zone_by_uuid(&content, "zone-broken");
        assert!(!broken.has_net(), "net 0 is no net, label or not");
        assert_eq!(broken.net(), None);
        assert_eq!(broken.net_label.as_deref(), Some("GND"));
        assert_eq!(broken.layers, vec!["In1.Cu".to_string()]);
        // The outline is counted, not the cached fill.
        assert_eq!(broken.points, 4);
        assert_eq!(broken.bbox, Some((0.0, 0.0, 20.0, 20.0)));
        assert!(broken.filled);

        let ok = zone_by_uuid(&content, "zone-ok");
        assert!(ok.has_net());
        assert_eq!(ok.net(), Some("+3V3"));
        assert_eq!(ok.points, 3);
        assert!(!ok.filled);

        // Numbered board: the name comes back through the net table.
        let old = numbered_board();
        assert_eq!(zone_by_uuid(&old, "zone-broken").net_code, Some(0));
        assert!(!zone_by_uuid(&old, "zone-broken").has_net());
        assert_eq!(collect_vias(&old)[0].net_name.as_deref(), Some("GND"));
        assert_eq!(collect_vias(&old)[0].net_code, Some(7));
    }

    // ─── Vias: round trip ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn add_query_delete_via_leaves_the_file_byte_intact() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();

        let added = handle_add_via(
            &json!({
                "board": board.to_str().unwrap(), "net_name": "+3V3",
                "x": 58.92, "y": 60.0, "drill": 0.3, "pad_size": 0.6
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!added.is_error, "{:?}", body(&added));

        let after_add = std::fs::read_to_string(&board).unwrap();
        check_document(&after_add, "kicad_pcb").expect("board must still parse");

        // Query it back — the uuid query_vias returns is what deletes it.
        let q = body(
            &handle_query_vias(&json!({ "board": board.to_str().unwrap() }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(q["count"], json!(3));
        let v = q["vias"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["x"] == json!(58.92))
            .expect("the new via must come back");
        assert_eq!(v["net"], json!("+3V3"));
        assert_eq!(v["y"], json!(60.0));
        assert_eq!(v["drill"], json!(0.3));
        assert_eq!(v["size"], json!(0.6));
        assert_eq!(v["layers"], json!(["F.Cu", "B.Cu"]));
        let uuid = v["uuid"].as_str().unwrap().to_string();

        let d = handle_delete_via(&json!({ "board": board.to_str().unwrap(), "uuid": uuid }), &ctx)
            .await
            .unwrap();
        assert!(!d.is_error, "{:?}", body(&d));
        assert_eq!(body(&d)["deleted_uuid"], json!(uuid));

        let after_delete = std::fs::read_to_string(&board).unwrap();
        check_document(&after_delete, "kicad_pcb").expect("board must still parse");
        assert_eq!(
            after_delete, original,
            "deleting the added via must restore the file byte for byte"
        );
        // Everything else is still there.
        assert!(after_delete.contains("fp-via") && after_delete.contains("text-1"));
        assert_eq!(collect_vias(&after_delete).len(), 2);
        assert_eq!(collect_zones(&after_delete).len(), 2);
    }

    #[tokio::test]
    async fn delete_via_removes_only_the_named_one() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let r = handle_delete_via(
            &json!({ "board": board.to_str().unwrap(), "uuid": "via-1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        let left: Vec<String> = collect_vias(&content)
            .into_iter()
            .filter_map(|v| v.uuid)
            .collect();
        assert_eq!(left, vec!["via-2".to_string()]);
        // The footprint's own via is untouched.
        assert!(content.contains("fp-via"));
    }

    #[tokio::test]
    async fn query_vias_filters_by_net_and_layer() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();
        let b = board.to_str().unwrap();

        let by_net = body(
            &handle_query_vias(&json!({ "board": b, "net_name": "GND" }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(by_net["count"], json!(1));
        assert_eq!(by_net["vias"][0]["uuid"], json!("via-1"));

        // A layer filter matches either end of the pair.
        let by_layer = body(
            &handle_query_vias(&json!({ "board": b, "layer": "In1.Cu" }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(by_layer["count"], json!(1));
        assert_eq!(by_layer["vias"][0]["uuid"], json!("via-2"));

        let both = body(
            &handle_query_vias(
                &json!({ "board": b, "net_name": "GND", "layer": "In1.Cu" }),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(both["count"], json!(0));
    }

    // ─── Zones: reading ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn query_zones_flags_the_netless_pour_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();
        let b = board.to_str().unwrap();

        let all = body(&handle_query_zones(&json!({ "board": b }), &ctx).await.unwrap());
        assert_eq!(all["count"], json!(2));
        assert_eq!(all["netless_zones"], json!(1));
        // The note has to name the repair, or the finding is not actionable.
        let note = all["note"].as_str().unwrap();
        assert!(note.contains("set_zone_net") && note.contains("zone-broken"), "{note}");

        let broken = &all["zones"][0];
        assert_eq!(broken["uuid"], json!("zone-broken"));
        assert_eq!(broken["has_net"], json!(false));
        assert_eq!(broken["net"], json!(null), "a net-0 pour is on no net");
        assert_eq!(broken["net_label"], json!("GND"));
        assert_eq!(broken["layers"], json!(["In1.Cu"]));
        assert_eq!(broken["points"], json!(4));
        assert_eq!(broken["filled"], json!(true));
        assert_eq!(broken["bbox"]["max_x"], json!(20.0));

        // Filters: the net-less pour is not on GND, whatever its label says.
        let by_net = body(
            &handle_query_zones(&json!({ "board": b, "net_name": "GND" }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(by_net["count"], json!(0));
        let by_layer = body(
            &handle_query_zones(&json!({ "board": b, "layer": "F.Cu" }), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(by_layer["count"], json!(1));
        assert_eq!(by_layer["zones"][0]["uuid"], json!("zone-ok"));
        assert_eq!(by_layer["netless_zones"], json!(0));
        assert!(by_layer["note"].is_null());
    }

    // ─── Zones: the repair, in both formats ───────────────────────────────────

    #[tokio::test]
    async fn set_zone_net_repairs_a_netless_pour_on_a_kicad10_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let r = handle_set_zone_net(
            &json!({ "board": board.to_str().unwrap(), "uuid": "zone-broken", "net_name": "GND" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        let b = body(&r);
        assert_eq!(b["net"], json!("GND"));
        assert_eq!(b["net_code"], json!(null));
        assert_eq!(b["previously_had_net"], json!(false));
        assert_eq!(b["refill_required"], json!(true));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").expect("board must still parse");

        let z = zone_by_uuid(&content, "zone-broken");
        assert!(z.has_net());
        assert_eq!(z.net(), Some("GND"));
        assert_eq!(
            z.net_label, None,
            "the stale numbered-format label must not survive next to a by-name net"
        );
        // Written in the file's own convention and its own (tab) indentation.
        assert!(
            content.contains("\t\t(net \"GND\")\n\t\t(layer \"In1.Cu\")"),
            "{content}"
        );
        assert!(!content.contains("(net 0)"));
        // Nothing else moved.
        assert_eq!(collect_zones(&content).len(), 2);
        assert_eq!(zone_by_uuid(&content, "zone-ok").net(), Some("+3V3"));
        assert!(content.contains("fp-zone") && content.contains("via-2"));
    }

    #[tokio::test]
    async fn set_zone_net_repairs_a_netless_pour_on_a_numbered_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &numbered_board());
        let ctx = test_ctx();
        let b = board.to_str().unwrap();

        // A zone that already carries a (net_name …): the number is what
        // connects the copper, and the label is kept in step with it.
        let r = handle_set_zone_net(
            &json!({ "board": b, "uuid": "zone-broken", "net_name": "GND" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        assert_eq!(body(&r)["net_code"], json!(7));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").expect("board must still parse");
        assert!(
            content.contains("\t\t(net 7)\n\t\t(net_name \"GND\")"),
            "{content}"
        );
        // A by-name net field here would name a net this board does not have.
        assert!(!content.contains("(net \"GND\")"));
        let z = zone_by_uuid(&content, "zone-broken");
        assert!(z.has_net());
        assert_eq!(z.net(), Some("GND"));

        // A zone with no label at all: one is added, on its own line with the
        // file's own indentation.
        let r = handle_set_zone_net(
            &json!({ "board": b, "uuid": "zone-nolabel", "net_name": "+3V3" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").expect("board must still parse");
        assert!(
            content.contains("\t\t(net 8)\n\t\t(net_name \"+3V3\")\n\t\t(layer \"In2.Cu\")"),
            "{content}"
        );
        let z = zone_by_uuid(&content, "zone-nolabel");
        assert_eq!(z.net(), Some("+3V3"));
        assert_eq!(z.net_code, Some(8));

        // The footprint's own zone and the net table are untouched.
        assert!(content.contains("\t\t\t(uuid \"fp-zone\")"));
        assert!(content.contains("\t(net 7 \"GND\")"));
    }

    #[tokio::test]
    async fn set_zone_net_refuses_an_unknown_net_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx();

        for original in [kicad10_board(), numbered_board()] {
            let d = tempfile::tempdir().unwrap();
            let board = board_file(d.path(), &original);
            let b = board.to_str().unwrap();

            let r = handle_set_zone_net(
                &json!({ "board": b, "uuid": "zone-broken", "net_name": "VBUS" }),
                &ctx,
            )
            .await
            .unwrap();
            // Never net 0: writing the net the caller asked for when the board
            // does not have it is how the pour became disconnected.
            assert!(error_text(&r).contains("VBUS"), "{}", error_text(&r));
            assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
        }

        // A uuid that is not a zone is refused too, and says what it is.
        let board = board_file(dir.path(), &kicad10_board());
        let r = handle_set_zone_net(
            &json!({ "board": board.to_str().unwrap(), "uuid": "via-1", "net_name": "GND" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(error_text(&r).contains("(via)"), "{}", error_text(&r));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), kicad10_board());
    }

    // ─── Zones: delete ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_zone_removes_exactly_one_pour() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let r = handle_delete_zone(
            &json!({ "board": board.to_str().unwrap(), "uuid": "zone-broken" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        // What was removed is reported, so a mistaken delete can be re-created.
        assert_eq!(body(&r)["deleted"]["points"], json!(4));
        assert_eq!(body(&r)["deleted"]["has_net"], json!(false));

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").expect("board must still parse");
        let left: Vec<String> = collect_zones(&content)
            .into_iter()
            .filter_map(|z| z.uuid)
            .collect();
        assert_eq!(left, vec!["zone-ok".to_string()]);
        // The footprint's zone, both vias and the graphics survive.
        assert!(content.contains("fp-zone"));
        assert_eq!(collect_vias(&content).len(), 2);
        assert!(content.contains("text-1"));
        assert!(!content.contains("zone-broken"));
    }

    #[tokio::test]
    async fn deletes_refuse_footprint_items_and_unknown_uuids() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();
        let b = board.to_str().unwrap();

        // Inside a footprint: part of the component, not a board item.
        for args in [
            json!({ "board": b, "uuid": "fp-via" }),
            json!({ "board": b, "uuid": "no-such-uuid" }),
        ] {
            let r = handle_delete_via(&args, &ctx).await.unwrap();
            assert!(r.is_error, "{:?}", body(&r));
        }
        for args in [
            json!({ "board": b, "uuid": "fp-zone" }),
            json!({ "board": b, "uuid": "no-such-uuid" }),
        ] {
            let r = handle_delete_zone(&args, &ctx).await.unwrap();
            assert!(r.is_error, "{:?}", body(&r));
        }

        // A via's uuid handed to delete_zone says which item it really is.
        let r = handle_delete_zone(&json!({ "board": b, "uuid": "via-1" }), &ctx)
            .await
            .unwrap();
        assert!(error_text(&r).contains("(via)"), "{}", error_text(&r));

        // Not one of those refusals may touch the file — an earlier bug in this
        // codebase fell back to offset 0 and erased it.
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn set_zone_net_is_idempotent_and_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();
        let b = board.to_str().unwrap();

        let args = json!({ "board": b, "uuid": "zone-ok", "net_name": "GND" });
        let r = handle_set_zone_net(&args, &ctx).await.unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        assert_eq!(body(&r)["previous_net"], json!("+3V3"));
        let once = std::fs::read_to_string(&board).unwrap();

        // Setting the same net again changes nothing.
        let r = handle_set_zone_net(&args, &ctx).await.unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), once);

        // And it can be put back.
        let r = handle_set_zone_net(
            &json!({ "board": b, "uuid": "zone-ok", "net_name": "+3V3" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!r.is_error, "{:?}", body(&r));
        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        assert_eq!(content, kicad10_board());
    }
}

#[cfg(test)]
mod keepout_tests {
    use super::*;

    /// A real board carried `ANT_KEEPOUT`: a rule area spanning all four
    /// copper layers with no `(net …)` at all. Keepouts carry no net by
    /// design, so reporting one as a net-less pour tells a caller to "repair"
    /// it — and assigning a net converts an antenna clearance area into filled
    /// copper.
    const BOARD: &str = "(kicad_pcb\n\t(version 20260206)\n\
\t(zone\n\t\t(net \"GND\")\n\t\t(layer \"F.Cu\")\n\t\t(uuid \"pour-ok\")\n\
\t\t(polygon (pts (xy 0 0) (xy 1 0) (xy 1 1)))\n\t)\n\
\t(zone\n\t\t(net 0)\n\t\t(layer \"F.Cu\")\n\t\t(uuid \"pour-broken\")\n\
\t\t(polygon (pts (xy 0 0) (xy 1 0) (xy 1 1)))\n\t)\n\
\t(zone\n\t\t(layers \"F.Cu\" \"B.Cu\" \"In1.Cu\" \"In2.Cu\")\n\t\t(uuid \"ant-keepout\")\n\
\t\t(name \"ANT_KEEPOUT\")\n\t\t(keepout\n\t\t\t(tracks allowed)\n\t\t\t(copperpour not_allowed)\n\t\t)\n\
\t\t(polygon (pts (xy 0 0) (xy 1 0) (xy 1 1)))\n\t)\n)\n";

    fn zone(uuid: &str) -> BoardZone {
        collect_zones(BOARD)
            .into_iter()
            .find(|z| z.uuid.as_deref() == Some(uuid))
            .expect(uuid)
    }

    #[test]
    fn a_keepout_is_recognised_and_not_called_netless() {
        let k = zone("ant-keepout");
        assert!(k.keepout, "(keepout …) must be detected");
        assert_eq!(k.name.as_deref(), Some("ANT_KEEPOUT"));
        assert!(!k.has_net(), "it genuinely carries no net");
        assert!(
            !k.is_orphaned_pour(),
            "but it is NOT a broken pour — flagging it would invite a caller to \
             turn a rule area into copper"
        );
    }

    #[test]
    fn a_real_netless_pour_is_still_flagged() {
        let broken = zone("pour-broken");
        assert!(!broken.keepout);
        assert!(broken.is_orphaned_pour(), "net 0 pour must still be reported");
        assert!(!zone("pour-ok").is_orphaned_pour());
    }
}

//! `pcb_routing` toolset — traces, vias, copper pours, nets, netclasses, and diff pairs.
//!
//! Routing operations use the KiCAD IPC API; `add_net`, `create_netclass`, and
//! `add_copper_pour` use S-expression file manipulation.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{apply_edits, new_uuid, write_atomic, SexpEdit};
use serde_json::json;

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(&KiCadIpcClient::new(&addr))).await {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(e.to_string())),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

macro_rules! ipc {
    ($ctx:expr, |$c:ident| $body:expr) => {{
        let addr = $ctx.config.ipc_address.clone();
        match with_ipc(addr, move |$c| $body).await? {
            Ok(v) => v,
            Err(msg) => {
                return Ok(CallToolResult::error(format!(
                    "KiCAD must be running with the board loaded (IPC error: {})",
                    msg
                )))
            }
        }
    }};
}

// ─── S-expression helpers ─────────────────────────────────────────────────────

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
            "Route a trace segment between two points on a copper layer via KiCAD IPC.",
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
            "Route a direct trace between two pads of named components (L-bend routing) via KiCAD IPC. \
             Both pads must already be on the same net; the tool refuses to connect different \
             nets rather than silently creating a short.",
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
            "Delete a trace segment identified by its UUID via KiCAD IPC.",
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
            "List trace segments on the board, optionally filtered by net and/or layer.",
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
            "Return all nets defined on the PCB via KiCAD IPC.",
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
            "Modify a trace segment by deleting and re-adding it with new parameters.",
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
            "Route a differential pair (two parallel traces with a specified gap).",
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
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, net_sexp)]);
    if let Err(why) = konnect_sexp::writer::check_document(&new_content, "kicad_pcb") {
        return Ok(CallToolResult::error(format!(
            "Internal error: adding the net would have corrupted the board ({why}) — \
             nothing was written."
        )));
    }
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net_id": net_id, "net_name": net_name }),
    ))
}

async fn handle_route_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
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

    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, |c| c
        .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
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

    // Pad geometry is read from `board_path` but the tracks are created over
    // IPC, which lands on whatever board KiCAD has open. Without this gate the
    // tool reads coordinates out of one file and routes them onto a different
    // board. There is no file fallback for track creation here, so refuse.
    if !crate::tools::pcb_board::ipc_targets_board(ctx.config.ipc_address.clone(), &board_path)
        .await
    {
        return Ok(CallToolResult::error(format!(
            "route_pad_to_pad creates tracks through KiCAD's IPC API, which acts on \
             the board KiCAD currently has open — and that is not '{}'. Open exactly \
             that file in KiCAD and retry; routing now would read pad positions from \
             the named file and write the tracks onto a different board.",
            board_path.display()
        )));
    }

    // Look up pad positions from the PCB S-expression file
    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

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

    let pos1 = find_pad_board_position(&tree, &ref1, &pad1)?;
    let pos2 = find_pad_board_position(&tree, &ref2, &pad2)?;

    // Route an L-bend: horizontal first, then vertical
    let (x1, y1) = pos1;
    let (x2, y2) = pos2;
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();

    if (x1 - x2).abs() < 0.01 || (y1 - y2).abs() < 0.01 {
        // Already axis-aligned: single segment
        ipc!(ctx, |c| c
            .add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2));
    } else {
        // L-bend: horizontal then vertical
        let mid_x = x2;
        let mid_y = y1;
        let net_a = net_name.clone();
        let net_b = net_name.clone();
        let layer_a = layer.clone();
        let layer_b = layer.clone();
        ipc!(ctx, |c| {
            c.add_track(&net_a, &layer_a, width, x1, y1, mid_x, mid_y)?;
            c.add_track(&net_b, &layer_b, width, mid_x, mid_y, x2, y2)?;
            Ok(())
        });
    }

    Ok(CallToolResult::json(&json!({
        "routed": true,
        "net": net_name, "layer": layer, "width": width,
        "from": { "ref": ref1, "pad": pad1, "x": x1, "y": y1 },
        "to":   { "ref": ref2, "pad": pad2, "x": x2, "y": y2 }
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
    let net_field = match net {
        crate::tools::NetRef::Named(n) => format!("(net \"{n}\")"),
        crate::tools::NetRef::Numbered(id, _) => format!("(net {id})"),
    };
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
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, via)]);
    if let Err(why) = konnect_sexp::writer::check_document(&new_content, "kicad_pcb") {
        return Ok(CallToolResult::error(format!(
            "Internal error: adding the via would have corrupted '{}' ({why}) — \
             nothing was written.",
            board_path.display()
        )));
    }
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net.name(), "x": x, "y": y,
        "drill": drill, "pad_size": pad_size,
        "layers": [start_layer, end_layer],
        "source": "file",
        "note": "KiCAD reloads the board from disk; if it is open, use File > Revert to see the via."
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
    let close = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close, zone_s)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(
        &json!({ "net": net_name, "layer": layer, "points": pts.len() }),
    ))
}

async fn handle_delete_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let uuid = match require_str(args, "uuid") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let uuid_ipc = uuid.clone();
    ipc!(ctx, |c| c.delete_track(&uuid_ipc));
    Ok(CallToolResult::json(&json!({ "deleted_uuid": uuid })))
}

async fn handle_query_traces(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let net = args["net_name"].as_str().map(String::from);
    let layer = args["layer"].as_str().map(String::from);

    let tracks = ipc!(ctx, |c| { c.get_tracks(net.as_deref(), layer.as_deref()) });

    let items: Vec<serde_json::Value> = tracks
        .iter()
        .map(|t| {
            json!({
                // uuid is what delete_trace takes — without it a queried
                // trace could not be deleted.
                "uuid": t.uuid,
                "net": t.net_name, "layer": t.layer, "width": t.width,
                "x1": t.start.x, "y1": t.start.y,
                "x2": t.end.x,   "y2": t.end.y
            })
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "traces": items }),
    ))
}

async fn handle_get_nets_list(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let nets = ipc!(ctx, |c| c.get_nets());
    let items: Vec<serde_json::Value> = nets
        .iter()
        .map(|n| json!({ "name": n.name, "netcode": n.netcode }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "nets": items }),
    ))
}

async fn handle_modify_trace(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
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

    let uuid_ipc = uuid.clone();
    let net_ipc = net_name.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, |c| {
        c.delete_track(&uuid_ipc)?;
        c.add_track(&net_ipc, &layer_ipc, width, x1, y1, x2, y2)
    });
    Ok(CallToolResult::json(&json!({
        "modified_uuid": uuid,
        "net": net_name, "layer": layer, "width": width,
        "from": { "x": x1, "y": y1 }, "to": { "x": x2, "y": y2 }
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

    // Find the netclass block: (netclass "NAME" ...)
    let nc_pat = format!("(netclass \"{}\"", netclass);
    let nc_pos = match content.find(&nc_pat) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "Netclass '{}' not found in board file",
                netclass
            )))
        }
    };

    // Find the closing paren of the netclass block
    let mut depth = 0i32;
    let mut nc_end = nc_pos;
    for (i, ch) in content[nc_pos..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    nc_end = nc_pos + i;
                    break;
                }
            }
            _ => {}
        }
    }

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
    let new_content = apply_edits(content, vec![SexpEdit::insert(nc_end, net_entry)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "assigned": true,
        "net_name": net_name,
        "netclass": netclass
    })))
}

async fn handle_route_diff_pair(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
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

    let np_ipc = net_pos.clone();
    let nn_ipc = net_neg.clone();
    let layer_ipc = layer.clone();
    ipc!(ctx, |c| {
        c.add_track(
            &np_ipc,
            &layer_ipc,
            width,
            x1 + perp_x,
            y1 + perp_y,
            x2 + perp_x,
            y2 + perp_y,
        )?;
        c.add_track(
            &nn_ipc,
            &layer_ipc,
            width,
            x1 - perp_x,
            y1 - perp_y,
            x2 - perp_x,
            y2 - perp_y,
        )
    });

    Ok(CallToolResult::json(&json!({
        "net_pos": net_pos, "net_neg": net_neg,
        "layer": layer, "width": width, "gap": gap
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

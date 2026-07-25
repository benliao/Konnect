//! `pcb_board` toolset — board setup, layers, outlines, zones, and board-level items.
//!
//! Most operations use S-expression file manipulation so they work without a running
//! KiCAD instance. `get_board_extents` tries the IPC API first, falling back to
//! parsing the file for coordinate bounds.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_f64, require_str, ToolContext, ToolDef};
use konnect_ipc::builders;
use konnect_sexp::{
    parser::{parse_sexp, SexpNode},
    writer::{apply_edits, new_uuid, write_atomic, SexpEdit},
};
use serde_json::json;

// ─── Board layer table ───────────────────────────────────────────────────────

/// One row of the board's `(layers ...)` block: `(ID "Name" type ["Alias"])`.
struct LayerEntry {
    id: i32,
    name: String,
    kind: String,
}

/// Read the board's layer table.
///
/// A row's id is the list **head** (`(0 "F.Cu" signal)`), so rows cannot be
/// reached with `find_all(tag)` — that matches on the head and would need the
/// id as the tag. Every child after the `layers` tag is a row.
fn read_layers(tree: &SexpNode) -> Option<Vec<LayerEntry>> {
    let node = tree.find("layers")?;
    Some(
        node.children()?
            .iter()
            .skip(1)
            .filter_map(|row| {
                let c = row.children()?;
                Some(LayerEntry {
                    id: c.first()?.as_str()?.parse().ok()?,
                    name: c.get(1)?.as_str()?.to_string(),
                    kind: c
                        .get(2)
                        .and_then(|n| n.as_str())
                        .unwrap_or("user")
                        .to_string(),
                })
            })
            .collect(),
    )
}

/// The canonical id KiCAD assigns to `name`, for the layers this tool may add.
///
/// KiCAD does not let a board invent layer ids — each named layer has a fixed
/// id, and copper owns the even ids below them. Read off a KiCAD 10-written
/// board.
fn canonical_layer_id(name: &str) -> Option<i32> {
    let fixed = [
        ("F.Mask", 1),
        ("B.Mask", 3),
        ("F.SilkS", 5),
        ("B.SilkS", 7),
        ("F.Adhes", 9),
        ("B.Adhes", 11),
        ("F.Paste", 13),
        ("B.Paste", 15),
        ("Dwgs.User", 17),
        ("Cmts.User", 19),
        ("Eco1.User", 21),
        ("Eco2.User", 23),
        ("Edge.Cuts", 25),
        ("Margin", 27),
        ("B.CrtYd", 29),
        ("F.CrtYd", 31),
        ("B.Fab", 33),
        ("F.Fab", 35),
    ];
    if let Some((_, id)) = fixed.iter().find(|(n, _)| *n == name) {
        return Some(*id);
    }
    // User.1 … User.9 are the only user-definable layers: id = 37 + 2N.
    let n: i32 = name.strip_prefix("User.")?.parse().ok()?;
    (1..=9).contains(&n).then_some(37 + 2 * n)
}

/// Whether `name` is a copper layer (`F.Cu`, `B.Cu`, `In<N>.Cu`).
fn is_copper_layer(name: &str) -> bool {
    name.ends_with(".Cu")
}

/// KiCAD 10's copper id assignment, for reference and for any future
/// implementation of copper-count changes:
///
/// ```text
/// F.Cu   = 0
/// B.Cu   = 2
/// In<N>.Cu = 2 + 2N   →  In1.Cu = 4, In2.Cu = 6, In3.Cu = 8, …
/// ```
///
/// The odd ids 1..=35 belong to the technical layers (F.Mask = 1, B.Mask = 3,
/// F.SilkS = 5, …), which is why inner copper starts at 4 rather than 1. The
/// old code assigned inner layers from 1 upward and collided with F.Mask.
#[allow(dead_code)]
fn inner_copper_id(n: i32) -> i32 {
    2 + 2 * n
}

// Build the 4 Edge.Cuts segments forming a rectangle, packed as Any for create_items.
fn rect_outline_items(x1: f64, y1: f64, x2: f64, y2: f64, w: f64) -> Vec<prost_types::Any> {
    let sides = [
        (x1, y1, x2, y1),
        (x2, y1, x2, y2),
        (x2, y2, x1, y2),
        (x1, y2, x1, y1),
    ];
    sides
        .iter()
        .map(|&(a, b, c, d)| {
            builders::pack_any(
                &builders::board_segment("Edge.Cuts", w, a, b, c, d),
                "kiapi.board.types.BoardGraphicShape",
            )
        })
        .collect()
}

// ─── IPC helper ───────────────────────────────────────────────────────────────

async fn with_ipc<T, F>(addr: String, f: F) -> anyhow::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&konnect_ipc::client::KiCadIpcClient) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let client = konnect_ipc::client::KiCadIpcClient::new(&addr);
        f(&client)
    })
    .await
    {
        Ok(Ok(r)) => Ok(Ok(r)),
        Ok(Err(e)) => Ok(Err(e.to_string())),
        Err(e) => Err(anyhow::anyhow!("Thread error: {}", e)),
    }
}

// ─── S-expression format helpers ──────────────────────────────────────────────

fn format_gr_line(x1: f64, y1: f64, x2: f64, y2: f64, layer: &str, width: f64) -> String {
    let uuid = new_uuid();
    format!(
        "\n  (gr_line\n    (start {x1} {y1})\n    (end {x2} {y2})\n    \
         (stroke (width {width}) (type solid))\n    (layer \"{layer}\")\n    (uuid \"{uuid}\")\n  )"
    )
}

fn format_gr_text(text: &str, x: f64, y: f64, rot: f64, layer: &str, size: f64) -> String {
    let uuid = new_uuid();
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "\n  (gr_text \"{escaped}\"\n    (at {x} {y} {rot})\n    (layer \"{layer}\")\n    \
         (effects (font (size {size} {size}) (thickness 0.15)))\n    (uuid \"{uuid}\")\n  )"
    )
}

fn format_npth_footprint(x: f64, y: f64, drill_d: f64, reference: &str) -> String {
    let fp_uuid = new_uuid();
    let ref_uuid = new_uuid();
    let val_uuid = new_uuid();
    let pad_uuid = new_uuid();
    let pad_size = drill_d + 0.5;
    format!(
        "\n  (footprint \"MountingHole:MountingHole_{drill_d:.1}mm\"\n    \
         (layer \"F.Cu\")\n    (at {x} {y})\n    \
         (attr exclude_from_pos_files)\n    \
         (property \"Reference\" \"{reference}\"\n      (at 0 {offset} 0)\n      (layer \"F.SilkS\")\n      (uuid \"{ref_uuid}\")\n    )\n    \
         (property \"Value\" \"MountingHole\"\n      (at 0 -{offset} 0)\n      (layer \"F.Fab\")\n      (uuid \"{val_uuid}\")\n    )\n    \
         (pad \"\" np_thru_hole circle (at 0 0) (size {pad_size} {pad_size})\n      \
         (drill {drill_d})\n      (layers \"*.Cu\" \"*.Mask\")\n      (uuid \"{pad_uuid}\")\n    )\n    \
         (uuid \"{fp_uuid}\")\n  )",
        offset = drill_d + 1.5
    )
}

fn format_zone_polygon(
    net_id: i32,
    net_name: &str,
    layer: &str,
    clearance: f64,
    min_width: f64,
    points: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let pts: String = points
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone (net {net_id}) (net_name \"{net_name}\") (layer \"{layer}\") (uuid \"{uuid}\")\n    \
         (hatch edge 0.508)\n    (connect_pads (clearance {clearance}))\n    \
         (min_thickness {min_width})\n    (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5))\n    \
         (polygon (pts{pts}\n    ))\n  )"
    )
}

/// A standalone filled polygon graphic (`gr_poly`), not tied to a net or zone
/// fill — used for imported artwork rather than copper pours.
fn format_gr_poly(points: &[(f64, f64)], layer: &str) -> String {
    let uuid = new_uuid();
    let pts: String = points
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (gr_poly\n    (pts{pts}\n    )\n    \
         (stroke (width 0) (type solid))\n    (fill solid)\n    \
         (layer \"{layer}\")\n    (uuid \"{uuid}\")\n  )"
    )
}

/// Find the net ID for a given net name in the .kicad_pcb content.
fn find_net_id(content: &str, net_name: &str) -> Option<i32> {
    // Entries look like: (net 1 "GND")
    let search = format!(r#" "{net_name}")"#);
    let pos = content.find(&search)?;
    let before = &content[..pos];
    // Walk back to find the opening (net and the number
    let net_pat = before.rfind("(net ")?;
    let num_start = net_pat + "(net ".len();
    let num_end = before[num_start..].find(' ').unwrap_or(0);
    before[num_start..num_start + num_end].parse().ok()
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "set_board_size",
            "Set the PCB board outline to a rectangle of the given dimensions on the Edge.Cuts layer.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string", "description": "Path to .kicad_pcb file" },
                    "width":    { "type": "number", "description": "Board width in mm" },
                    "height":   { "type": "number", "description": "Board height in mm" },
                    "origin_x": { "type": "number", "description": "Left edge X coordinate", "default": 0 },
                    "origin_y": { "type": "number", "description": "Top edge Y coordinate", "default": 0 }
                },
                "required": ["board", "width", "height"]
            }),
            |args, ctx| async move { handle_set_board_size(args, ctx).await }
        ),
        tool!(
            "get_board_info",
            "Return metadata about the PCB: title, revision, company, layer count, paper size.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_info(args, ctx).await }
        ),
        tool!(
            "get_board_extents",
            "Return the bounding box of all objects on the board (tries KiCAD IPC, falls back to file parse).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_extents(args, ctx).await }
        ),
        tool!(
            "get_layer_list",
            "Return all layers defined in the board with their names and types.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_layer_list(args, ctx).await }
        ),
        tool!(
            "add_layer",
            "Enable a technical or user layer on the board (e.g. 'Eco1.User', 'User.1'). \
             Copper layers are NOT supported: changing the copper layer count requires \
             renumbering the copper id space and rewriting the board stackup, so it must \
             be done in KiCAD via Board Setup → Board Stackup → Physical Stackup.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "layer_name":  { "type": "string", "description": "A KiCAD-defined non-copper layer name: F.Mask, B.Mask, F.SilkS, B.SilkS, F.Adhes, B.Adhes, F.Paste, B.Paste, Dwgs.User, Cmts.User, Eco1.User, Eco2.User, Edge.Cuts, Margin, F.CrtYd, B.CrtYd, F.Fab, B.Fab, User.1 … User.9" },
                    "layer_type":  { "type": "string", "description": "Layer type recorded in the layer table", "default": "user" }
                },
                "required": ["board", "layer_name"]
            }),
            |args, ctx| async move { handle_add_layer(args, ctx).await }
        ),
        tool!(
            "set_active_layer",
            "Set the active layer recorded in the board file's setup section.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layer":  { "type": "string", "description": "KiCAD layer name (e.g. 'F.Cu')" }
                },
                "required": ["board", "layer"]
            }),
            |args, ctx| async move { handle_set_active_layer(args, ctx).await }
        ),
        tool!(
            "add_board_outline",
            "Add a rectangular board outline on the Edge.Cuts layer at specified coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "board":          { "type": "string" },
                    "x1":             { "type": "number", "description": "Top-left X in mm" },
                    "y1":             { "type": "number", "description": "Top-left Y in mm" },
                    "x2":             { "type": "number", "description": "Bottom-right X in mm" },
                    "y2":             { "type": "number", "description": "Bottom-right Y in mm" },
                    "corner_radius":  { "type": "number", "description": "Corner radius in mm (0 = sharp)", "default": 0 }
                },
                "required": ["board", "x1", "y1", "x2", "y2"]
            }),
            |args, ctx| async move { handle_add_board_outline(args, ctx).await }
        ),
        tool!(
            "add_mounting_hole",
            "Add an NPTH mounting hole footprint at the specified position.",
            json!({
                "type": "object",
                "properties": {
                    "board":          { "type": "string" },
                    "x":              { "type": "number", "description": "X position in mm" },
                    "y":              { "type": "number", "description": "Y position in mm" },
                    "drill_diameter": { "type": "number", "description": "Drill diameter in mm", "default": 3.2 },
                    "reference":      { "type": "string", "description": "Designator for the hole (e.g. 'H1')", "default": "H1" }
                },
                "required": ["board", "x", "y"]
            }),
            |args, ctx| async move { handle_add_mounting_hole(args, ctx).await }
        ),
        tool!(
            "add_board_text",
            "Add a silkscreen or fabrication text string to the board.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "text":      { "type": "string" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" },
                    "layer":     { "type": "string", "description": "Layer name", "default": "F.SilkS" },
                    "size":      { "type": "number", "description": "Font size in mm", "default": 1.0 },
                    "rotation":  { "type": "number", "description": "Rotation in degrees", "default": 0 }
                },
                "required": ["board", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_board_text(args, ctx).await }
        ),
        tool!(
            "add_zone",
            "Add a copper fill zone polygon on a specified layer and net.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "net_name":   { "type": "string", "description": "Net name (e.g. 'GND')" },
                    "layer":      { "type": "string", "description": "Copper layer (e.g. 'F.Cu')" },
                    "points": {
                        "type": "array",
                        "description": "Polygon vertices as [{x, y}]",
                        "items": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } }
                    },
                    "clearance":  { "type": "number", "default": 0.2 },
                    "min_width":  { "type": "number", "default": 0.2 }
                },
                "required": ["board", "net_name", "layer", "points"]
            }),
            |args, ctx| async move { handle_add_zone(args, ctx).await }
        ),
        tool!(
            "import_svg_logo",
            "Import an SVG file as filled silkscreen or copper artwork (a logo, icon, or other \
             graphic). Curved paths are flattened into polygon outlines since KiCAD's board \
             format doesn't support Bezier curves in filled shapes. Tries KiCAD IPC first, \
             falls back to a direct file edit if KiCAD isn't running.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string", "description": "Path to .kicad_pcb file" },
                    "svg":       { "type": "string", "description": "Path to the .svg file to import" },
                    "width_mm":  { "type": "number", "description": "Target width in mm (aspect ratio preserved)" },
                    "x":         { "type": "number", "description": "X position of the artwork's top-left corner in mm", "default": 0 },
                    "y":         { "type": "number", "description": "Y position of the artwork's top-left corner in mm", "default": 0 },
                    "layer":     { "type": "string", "description": "Target layer", "default": "F.SilkS" }
                },
                "required": ["board", "svg", "width_mm"]
            }),
            |args, ctx| async move { handle_import_svg_logo(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_set_board_size(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let width = match require_f64(args, "width") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let height = match require_f64(args, "height") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let ox = args["origin_x"].as_f64().unwrap_or(0.0);
    let oy = args["origin_y"].as_f64().unwrap_or(0.0);

    let x2 = ox + width;
    let y2 = oy + height;
    let w = 0.05_f64;

    // Try IPC first (live board in KiCAD, undo-aware); fall through to file edit.
    // ponytail: 4 segments over a single BoardRectangle keeps one builder path;
    // switch to board_rectangle if a native rect proves less flaky.
    let items = rect_outline_items(ox, oy, x2, y2, w);
    if with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.create_items(items)
    })
    .await?
    .is_ok()
    {
        return Ok(CallToolResult::json(&json!({
            "width": width, "height": height,
            "x1": ox, "y1": oy, "x2": x2, "y2": y2,
            "source": "ipc"
        })));
    }

    // Append 4 Edge.Cuts lines (top, right, bottom, left)
    let lines = format!(
        "{}{}{}{}",
        format_gr_line(ox, oy, x2, oy, "Edge.Cuts", w),
        format_gr_line(x2, oy, x2, y2, "Edge.Cuts", w),
        format_gr_line(x2, y2, ox, y2, "Edge.Cuts", w),
        format_gr_line(ox, y2, ox, oy, "Edge.Cuts", w),
    );

    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, lines)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "width": width, "height": height,
        "x1": ox, "y1": oy, "x2": x2, "y2": y2,
        "source": "file"
    })))
}

async fn handle_get_board_info(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    let tb = tree.find("title_block");
    let title = tb
        .and_then(|t| t.find("title"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let date = tb
        .and_then(|t| t.find("date"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let rev = tb
        .and_then(|t| t.find("rev"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let company = tb
        .and_then(|t| t.find("company"))
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();

    let layers = tree
        .find("layers")
        .map(|n| n.find_all("").len())
        .unwrap_or(0);
    let paper = tree
        .find("paper")
        .and_then(|n| n.get(1))
        .and_then(|n| n.as_str())
        .unwrap_or("A4")
        .to_string();

    let net_count = tree.find_all("net").len().saturating_sub(1); // exclude net 0

    Ok(CallToolResult::json(&json!({
        "file": board_path.display().to_string(),
        "title": title, "date": date, "revision": rev, "company": company,
        "paper": paper,
        "layer_count": layers,
        "net_count": net_count
    })))
}

async fn handle_get_board_extents(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;

    // Try IPC first; fall through to file-based computation on error
    if let Ok(ext) = with_ipc(ctx.config.ipc_address.clone(), |c| c.get_board_extents()).await? {
        return Ok(CallToolResult::json(&json!({
            "x_min": ext.min.x, "y_min": ext.min.y,
            "x_max": ext.max.x, "y_max": ext.max.y,
            "width": ext.max.x - ext.min.x,
            "height": ext.max.y - ext.min.y,
            "source": "ipc"
        })));
    }

    // File-based fallback: collect all coordinates from gr_lines and footprint positions
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    let mut update = |x: f64, y: f64| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    };

    for line in tree.find_all("gr_line") {
        if let (Some(s), Some(e)) = (line.find("start"), line.find("end")) {
            if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                (s.get_f64(1), s.get_f64(2), e.get_f64(1), e.get_f64(2))
            {
                update(x1, y1);
                update(x2, y2);
            }
        }
    }
    for fp in tree.find_all("footprint") {
        if let Some(at) = fp.find("at") {
            if let (Some(x), Some(y)) = (at.get_f64(1), at.get_f64(2)) {
                update(x, y);
            }
        }
    }

    if min_x == f64::MAX {
        return Ok(CallToolResult::json(
            &json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0, "width": 0, "height": 0, "source": "empty" }),
        ));
    }

    Ok(CallToolResult::json(&json!({
        "x_min": min_x, "y_min": min_y,
        "x_max": max_x, "y_max": max_y,
        "width": max_x - min_x,
        "height": max_y - min_y,
        "source": "file"
    })))
}

async fn handle_get_layer_list(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    let entries = match read_layers(&tree) {
        Some(e) => e,
        None => {
            return Ok(CallToolResult::error(
                "No (layers) section found in board file",
            ))
        }
    };

    let layers: Vec<serde_json::Value> = entries
        .iter()
        .map(|l| json!({ "id": l.id, "name": l.name, "type": l.kind }))
        .collect();

    Ok(CallToolResult::json(
        &json!({ "count": layers.len(), "layers": layers }),
    ))
}

async fn handle_add_layer(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let layer_name = match require_str(args, "layer_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let layer_type = args["layer_type"].as_str().unwrap_or("user");

    // Adding a copper layer is not an append. KiCAD owns the copper id space —
    // In1.Cu…InN.Cu sit between F.Cu (0) and B.Cu (2), so growing the stack
    // renumbers existing layers — and the copper count also lives in
    // (setup (stackup ...)), the pcbplotparams layerselection mask, and
    // (general (thickness)). Writing a bare (layers) row leaves a board KiCAD
    // refuses to open, so refuse rather than half-do it.
    if is_copper_layer(&layer_name) {
        return Ok(CallToolResult::error(format!(
            "Refusing to add copper layer '{layer_name}'. In KiCAD 10 the copper \
             ids are F.Cu=0, B.Cu=2 and In<N>.Cu=2+2N (In1.Cu=4, In2.Cu=6) — the \
             odd ids 1..35 belong to the technical layers — and the copper count \
             also lives in (setup (stackup ...)), the pcbplotparams layerselection \
             mask, and (general (thickness)). This tool updates none of that, so \
             the result would be a board KiCAD cannot load. Set the copper layer \
             count in KiCAD instead: File → Board Setup → Board Stackup → \
             Physical Stackup, set 'Copper layers', then OK."
        )));
    }

    let new_id = match canonical_layer_id(&layer_name) {
        Some(id) => id,
        None => {
            return Ok(CallToolResult::error(format!(
                "Unknown layer name '{layer_name}'. KiCAD layer ids are fixed, \
                 not allocated — this tool can only enable a layer KiCAD already \
                 defines. Valid names: F.Mask, B.Mask, F.SilkS, B.SilkS, F.Adhes, \
                 B.Adhes, F.Paste, B.Paste, Dwgs.User, Cmts.User, Eco1.User, \
                 Eco2.User, Edge.Cuts, Margin, F.CrtYd, B.CrtYd, F.Fab, B.Fab, \
                 User.1 … User.9."
            )))
        }
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = parse_sexp(&content)?;

    let existing = match read_layers(&tree) {
        Some(e) => e,
        None => return Ok(CallToolResult::error("No (layers) section found")),
    };
    if let Some(dup) = existing.iter().find(|l| l.name == layer_name) {
        return Ok(CallToolResult::error(format!(
            "Layer '{layer_name}' is already on the board (id {})",
            dup.id
        )));
    }
    if let Some(clash) = existing.iter().find(|l| l.id == new_id) {
        return Ok(CallToolResult::error(format!(
            "Cannot add '{layer_name}': its KiCAD id {new_id} is already used by \
             layer '{}'. The board's layer table disagrees with KiCAD's fixed \
             ids — fix it in KiCAD's Board Setup rather than here.",
            clash.name
        )));
    }

    // Insert before the layers block's own closing paren. Locating it needs a
    // real balanced scan: the previous indentation-guess ("\n  )") does not
    // match KiCAD 10's tab-indented output and fell back to the *first* ')' in
    // the block — the close of the F.Cu row — nesting each new layer inside it
    // and producing a board KiCAD could not load.
    let layers_pos = match find_layers_block(&content) {
        Some(p) => p,
        None => return Ok(CallToolResult::error("No (layers) section found")),
    };
    let (_, layers_end) = match konnect_sexp::writer::find_balanced_block(&content, layers_pos) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(
                "The (layers) section is not balanced — refusing to edit it",
            ))
        }
    };
    let close_paren = layers_end - 1;

    let indent = row_indent(&content, layers_pos);
    let new_layer = format!("{indent}({new_id} \"{layer_name}\" {layer_type})\n");
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_paren, new_layer)]);

    // Never hand KiCAD a board this tool has just broken: re-read the layer
    // table out of the edited text and check it is still a flat, unique set.
    if let Err(why) = validate_layer_table(&new_content) {
        return Ok(CallToolResult::error(format!(
            "Internal error: the edit would have corrupted '{}' ({why}) — \
             nothing was written.",
            board_path.display()
        )));
    }

    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "added_layer": layer_name, "id": new_id, "type": layer_type
    })))
}

/// Byte offset of the board's top-level `(layers` — skipping the `(layers ...)`
/// lists that footprints, pads, and zones carry.
fn find_layers_block(content: &str) -> Option<usize> {
    let b = content.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                // Skip quoted strings, honouring backslash escapes.
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'(' => {
                // The board's layer table is a direct child of (kicad_pcb ...).
                if depth == 1 && b[i..].starts_with(b"(layers") {
                    return Some(i);
                }
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The leading whitespace KiCAD uses for the rows inside the layers block, so
/// an inserted row lines up with the existing ones (KiCAD 10 writes tabs).
fn row_indent(content: &str, layers_pos: usize) -> String {
    content[layers_pos..]
        .lines()
        .nth(1)
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).collect())
        .unwrap_or_else(|| "\t\t".to_string())
}

/// Check the edited board still has a well-formed layer table: every row a flat
/// list of scalars, with unique ids and unique names.
fn validate_layer_table(content: &str) -> Result<(), String> {
    let tree = parse_sexp(content).map_err(|e| format!("board no longer parses: {e}"))?;
    let node = tree.find("layers").ok_or("the (layers) section vanished")?;
    let rows = node.children().ok_or("(layers) is not a list")?;

    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for row in rows.iter().skip(1) {
        let c = row.children().ok_or("a layer row is not a list")?;
        // A row is `(ID "Name" type ["Alias"])` — all scalars. A nested list
        // means an insert landed inside another row.
        if c.iter().any(|n| n.children().is_some()) {
            return Err("a layer row contains a nested list".into());
        }
        let id = c
            .first()
            .and_then(|n| n.as_str())
            .ok_or("a layer row has no id")?;
        let name = c
            .get(1)
            .and_then(|n| n.as_str())
            .ok_or("a layer row has no name")?;
        if !ids.insert(id.to_string()) {
            return Err(format!("duplicate layer id {id}"));
        }
        if !names.insert(name.to_string()) {
            return Err(format!("duplicate layer name {name}"));
        }
    }
    Ok(())
}

async fn handle_set_active_layer(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let new_content = if let Some(pos) = content.find("(active_layer ") {
        let after = pos + "(active_layer ".len();
        let close = content[after..].find(')').unwrap_or(0);
        let layer_end = after + close;
        apply_edits(
            content,
            vec![SexpEdit::replace(after, layer_end, format!("\"{layer}\""))],
        )
    } else {
        // Insert into setup block
        let setup_close = content
            .find("(setup")
            .and_then(|p| content[p..].find('\n').map(|off| p + off))
            .unwrap_or(content.rfind(')').unwrap_or(content.len()));
        apply_edits(
            content,
            vec![SexpEdit::insert(
                setup_close,
                format!("\n    (active_layer \"{layer}\")"),
            )],
        )
    };
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({ "active_layer": layer })))
}

async fn handle_add_board_outline(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
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
    let w = 0.05_f64;

    // Try IPC first; fall through to file edit if KiCAD is not reachable.
    let items = rect_outline_items(x1, y1, x2, y2, w);
    if with_ipc(ctx.config.ipc_address.clone(), move |c| {
        c.create_items(items)
    })
    .await?
    .is_ok()
    {
        return Ok(CallToolResult::json(&json!({
            "x1": x1, "y1": y1, "x2": x2, "y2": y2,
            "width": (x2-x1).abs(), "height": (y2-y1).abs(),
            "source": "ipc"
        })));
    }

    let lines = format!(
        "{}{}{}{}",
        format_gr_line(x1, y1, x2, y1, "Edge.Cuts", w),
        format_gr_line(x2, y1, x2, y2, "Edge.Cuts", w),
        format_gr_line(x2, y2, x1, y2, "Edge.Cuts", w),
        format_gr_line(x1, y2, x1, y1, "Edge.Cuts", w),
    );

    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, lines)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "x1": x1, "y1": y1, "x2": x2, "y2": y2,
        "width": (x2-x1).abs(), "height": (y2-y1).abs(),
        "source": "file"
    })))
}

async fn handle_add_mounting_hole(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let drill_d = args["drill_diameter"].as_f64().unwrap_or(3.2);
    let reference = args["reference"].as_str().unwrap_or("H1");

    let fp_sexp = format_npth_footprint(x, y, drill_d, reference);
    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, fp_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference, "x": x, "y": y, "drill_diameter": drill_d
    })))
}

async fn handle_add_board_text(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let text = match require_str(args, "text") {
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
    let layer = args["layer"].as_str().unwrap_or("F.SilkS").to_string();
    let size = args["size"].as_f64().unwrap_or(1.0);
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);

    // Try IPC first; fall through to file edit if KiCAD isn't reachable.
    let text_ipc = text.clone();
    let layer_ipc = layer.clone();
    if with_ipc(ctx.config.ipc_address.clone(), move |c| {
        let bt = builders::board_text(&layer_ipc, &text_ipc, x, y, size, rotation, false);
        let any = builders::pack_any(&bt, "kiapi.board.types.BoardText");
        c.create_items(vec![any])
    })
    .await?
    .is_ok()
    {
        return Ok(CallToolResult::json(&json!({
            "text": text, "x": x, "y": y, "layer": layer, "size": size,
            "source": "ipc"
        })));
    }

    let gr_text = format_gr_text(&text, x, y, rotation, &layer, size);
    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, gr_text)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "text": text, "x": x, "y": y, "layer": layer, "size": size,
        "source": "file"
    })))
}

async fn handle_add_zone(
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
    let min_width = args["min_width"].as_f64().unwrap_or(0.2);
    let pts_arr = match args["points"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'points' array")),
    };

    let points: Vec<(f64, f64)> = pts_arr
        .iter()
        .filter_map(|p| Some((p["x"].as_f64()?, p["y"].as_f64()?)))
        .collect();

    if points.len() < 3 {
        return Ok(CallToolResult::error("Zone requires at least 3 points"));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let net_id = find_net_id(&content, &net_name).unwrap_or(0);
    let zone_sexp = format_zone_polygon(net_id, &net_name, &layer, clearance, min_width, &points);

    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, zone_sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net_name, "layer": layer,
        "point_count": points.len(),
        "net_id": net_id
    })))
}

async fn handle_import_svg_logo(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let svg_path = get_path(args, "svg")?;
    let width_mm = match require_f64(args, "width_mm") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let x = args["x"].as_f64().unwrap_or(0.0);
    let y = args["y"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.SilkS").to_string();

    let svg_content = std::fs::read_to_string(&svg_path)?;
    let logo = crate::tools::svg_import::extract_polygons(&svg_content)?;
    if logo.polygons.is_empty() {
        return Ok(CallToolResult::error(
            "No fillable paths found in the SVG (only <path> elements are supported).",
        ));
    }

    let placed =
        crate::tools::svg_import::scale_and_place(&logo.polygons, logo.width, width_mm, x, y);

    // Try IPC first; fall through to a direct file edit if KiCAD isn't reachable.
    let layer_ipc = layer.clone();
    let placed_ipc = placed.clone();
    if with_ipc(ctx.config.ipc_address.clone(), move |c| {
        let shape = builders::board_polygon(&layer_ipc, true, &placed_ipc);
        let any = builders::pack_any(&shape, "kiapi.board.types.BoardGraphicShape");
        c.create_items(vec![any])
    })
    .await?
    .is_ok()
    {
        return Ok(CallToolResult::json(&json!({
            "polygon_count": placed.len(),
            "layer": layer,
            "width_mm": width_mm,
            "source": "ipc"
        })));
    }

    let mut sexp = String::new();
    for polygon in &placed {
        sexp.push_str(&format_gr_poly(polygon, &layer));
    }
    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, sexp)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "polygon_count": placed.len(),
        "layer": layer,
        "width_mm": width_mm,
        "source": "file"
    })))
}

#[cfg(test)]
mod svg_logo_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            // Deliberately empty ipc_address: with_ipc fails fast against it,
            // exercising the file-fallback path without needing live KiCAD.
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

    fn blank_board() -> &'static str {
        "(kicad_pcb\n  (version 20250610)\n  (generator \"konnect\")\n  (paper \"A4\")\n  (net 0 \"\")\n)\n"
    }

    fn rect_svg() -> &'static str {
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100">
            <path d="M0 0 L100 0 L100 100 L0 100 Z" fill="black"/>
        </svg>"##
    }

    #[test]
    fn format_gr_poly_contains_layer_fill_and_points() {
        let sexp = format_gr_poly(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)], "F.SilkS");
        assert!(sexp.contains("(gr_poly"));
        assert!(sexp.contains("(fill solid)"));
        assert!(sexp.contains("(layer \"F.SilkS\")"));
        assert!(sexp.contains("(xy 1 0)") || sexp.contains("(xy 1.0 0)"));
    }

    #[tokio::test]
    async fn import_svg_logo_file_fallback_places_polygon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board_path = dir.path().join("board.kicad_pcb");
        let svg_path = dir.path().join("logo.svg");
        std::fs::write(&board_path, blank_board()).unwrap();
        std::fs::write(&svg_path, rect_svg()).unwrap();

        let ctx = test_ctx();
        let args = json!({
            "board": board_path.to_str().unwrap(),
            "svg": svg_path.to_str().unwrap(),
            "width_mm": 10.0
        });

        let result = handle_import_svg_logo(&args, &ctx)
            .await
            .expect("handler should succeed");
        assert!(!result.is_error);

        let body = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["polygon_count"], json!(1));
        assert_eq!(parsed["source"], json!("file"));
        assert_eq!(parsed["layer"], json!("F.SilkS"));

        let updated = std::fs::read_to_string(&board_path).unwrap();
        assert!(updated.contains("(gr_poly"));
    }

    #[tokio::test]
    async fn import_svg_logo_rejects_svg_with_no_fillable_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board_path = dir.path().join("board.kicad_pcb");
        let svg_path = dir.path().join("empty.svg");
        std::fs::write(&board_path, blank_board()).unwrap();
        std::fs::write(
            &svg_path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"##,
        )
        .unwrap();

        let ctx = test_ctx();
        let args = json!({
            "board": board_path.to_str().unwrap(),
            "svg": svg_path.to_str().unwrap(),
            "width_mm": 10.0
        });

        let result = handle_import_svg_logo(&args, &ctx).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn import_svg_logo_missing_width_mm_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let board_path = dir.path().join("board.kicad_pcb");
        let svg_path = dir.path().join("logo.svg");
        std::fs::write(&board_path, blank_board()).unwrap();
        std::fs::write(&svg_path, rect_svg()).unwrap();

        let ctx = test_ctx();
        let args = json!({
            "board": board_path.to_str().unwrap(),
            "svg": svg_path.to_str().unwrap()
        });

        let result = handle_import_svg_logo(&args, &ctx).await.unwrap();
        assert!(result.is_error);
    }
}

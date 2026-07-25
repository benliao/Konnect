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
    writer::{
        apply_edits, check_document, find_balanced_block, find_block_with_leading_whitespace,
        new_uuid, write_atomic, SexpEdit,
    },
};
use serde_json::json;
use std::path::Path;

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

/// Whether the board KiCAD currently has open is the file the caller named.
///
/// **Every tool that takes a `board` path and has an IPC branch must gate that
/// branch on this.** KiCAD's IPC API has no per-file addressing — commands land
/// on whatever document KiCAD has open — so the `board` argument used to be read
/// and then ignored: a call naming a sandbox copy returned
/// `{"source":"ipc","success":true}`, left the named file untouched, and applied
/// the edit to the user's real project instead.
///
/// `false` (KiCAD unreachable, no board open, or a *different* board open) means
/// "take the file path", which edits exactly the file that was named.
pub(crate) async fn ipc_targets_board(addr: String, board: &Path) -> bool {
    if addr.is_empty() {
        return false;
    }
    let board = board.to_path_buf();
    tokio::task::spawn_blocking(move || {
        konnect_ipc::client::KiCadIpcClient::new(&addr).open_board_is(&board)
    })
    .await
    .unwrap_or(false)
}

// ─── Top-level board graphics ────────────────────────────────────────────────

/// Graphic primitive tags that appear as direct children of `(kicad_pcb …)`.
///
/// Footprint-owned graphics use the `fp_*` tags and are deliberately absent:
/// deleting those would silently reshape a component's silkscreen or courtyard.
const GRAPHIC_TAGS: [&str; 7] = [
    "gr_line", "gr_rect", "gr_circle", "gr_arc", "gr_poly", "gr_curve", "gr_text",
];

/// A top-level graphic primitive, located in the raw file text.
#[derive(Debug, Clone)]
struct BoardGraphic {
    tag: String,
    /// Byte range of the `(…)` block itself, excluding leading whitespace.
    start: usize,
    end: usize,
    layer: Option<String>,
    uuid: Option<String>,
    /// `(min_x, min_y, max_x, max_y)` over every coordinate the block carries.
    bbox: Option<(f64, f64, f64, f64)>,
}

/// The tag of the block opening at `open` — `"gr_line"` for `(gr_line …`.
fn tag_at(content: &str, open: usize) -> Option<&str> {
    let rest = content.get(open + 1..)?;
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    (end > 0).then(|| &rest[..end])
}

/// Every top-level graphic primitive in a `.kicad_pcb`, in file order.
///
/// Walks the raw text tracking paren depth and skipping quoted strings, so it
/// is indentation-agnostic (KiCAD 10 writes tabs, this crate's writer writes two
/// spaces) and immune to the unbalanced parens that appear inside quoted text
/// values. Only depth-1 blocks — direct children of the root — are considered,
/// which is what keeps footprint innards out of range.
fn collect_board_graphics(content: &str) -> Vec<BoardGraphic> {
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
                    // depth 0 opens the root; depth 1 opens a top-level item.
                    if depth == 1 {
                        if let Some(tag) = tag_at(content, i) {
                            if GRAPHIC_TAGS.contains(&tag) {
                                if let Some((s, e)) = find_balanced_block(content, i) {
                                    out.push(describe_graphic(content, tag, s, e));
                                    i = e; // the whole block is accounted for
                                    continue;
                                }
                            }
                        }
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

fn describe_graphic(content: &str, tag: &str, start: usize, end: usize) -> BoardGraphic {
    let node = parse_sexp(&content[start..end]).ok();
    BoardGraphic {
        tag: tag.to_string(),
        start,
        end,
        layer: node
            .as_ref()
            .and_then(|n| n.find_str("layer"))
            .map(str::to_string),
        uuid: node
            .as_ref()
            .and_then(|n| n.find_str("uuid"))
            .map(str::to_string),
        bbox: node.as_ref().and_then(graphic_bbox),
    }
}

/// Bounding box over every coordinate pair a graphic block carries.
fn graphic_bbox(node: &SexpNode) -> Option<(f64, f64, f64, f64)> {
    fn walk(n: &SexpNode, acc: &mut Option<(f64, f64, f64, f64)>) {
        let Some(children) = n.children() else { return };
        if matches!(
            n.head(),
            Some("xy" | "start" | "end" | "center" | "mid" | "at")
        ) {
            if let (Some(x), Some(y)) = (n.get_f64(1), n.get_f64(2)) {
                *acc = Some(match *acc {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
        for c in children {
            walk(c, acc);
        }
    }
    let mut acc = None;
    walk(node, &mut acc);
    acc
}

/// The byte range to remove for `g`, including the whitespace that indents it.
///
/// `None` when the range cannot be established. Callers must abort on `None`
/// rather than substitute a default: a delete helper that fell back to offset 0
/// is what erased an entire schematic file previously.
fn deletion_range(content: &str, g: &BoardGraphic) -> Option<(usize, usize)> {
    let (ws_start, end) = find_block_with_leading_whitespace(content, g.start)?;
    // The block must be the same one we located, must be non-empty, and must
    // not reach back over the root's opening paren.
    if end != g.end || ws_start >= end || ws_start == 0 {
        return None;
    }
    let span = content.get(ws_start..end)?;
    span.trim_start().starts_with('(').then_some((ws_start, end))
}

/// Offset of the root block's closing paren — where a new top-level item goes.
///
/// String-aware, unlike `rfind(')')`, which lands on whatever the last `)` byte
/// in the file happens to be.
fn root_close_offset(content: &str) -> Option<usize> {
    find_balanced_block(content, 0).map(|(_, end)| end - 1)
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
    net: &crate::tools::NetRef,
    layer: &str,
    clearance: f64,
    min_width: f64,
    points: &[(f64, f64)],
) -> String {
    let uuid = new_uuid();
    let net_fields = net.zone_fields();
    let pts: String = points
        .iter()
        .map(|(x, y)| format!("\n      (xy {x} {y})"))
        .collect();
    format!(
        "\n  (zone {net_fields} (layer \"{layer}\") (uuid \"{uuid}\")\n    \
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


// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "set_board_size",
            "SET the PCB board outline to a rectangle of the given dimensions on the Edge.Cuts \
             layer. This REPLACES the existing outline: every Edge.Cuts outline shape already on \
             the board is removed first (text on Edge.Cuts is left alone), so calling it twice \
             leaves one rectangle (4 segments), not two overlapping ones. To add an extra outline \
             shape without removing what is there — cutouts, a second board region — use \
             `add_board_outline` instead.",
            json!({
                "type": "object",
                "properties": {
                    "board":    { "type": "string", "description": "Path to .kicad_pcb file" },
                    "width":    { "type": "number", "description": "Board width in mm" },
                    "height":   { "type": "number", "description": "Board height in mm" },
                    "origin_x": { "type": "number", "description": "Left edge X coordinate", "default": 0 },
                    "origin_y": { "type": "number", "description": "Top edge Y coordinate", "default": 0 },
                    "replace":  { "type": "boolean", "description": "Remove existing Edge.Cuts graphics before writing the new rectangle. Leave true for set semantics; false appends and can produce a self-intersecting outline that fails DRC.", "default": true }
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
            "ADD (append) a rectangular outline on the Edge.Cuts layer at the specified \
             coordinates. Additive by design — existing Edge.Cuts graphics are left alone, so \
             this can build cutouts or multi-region outlines. To set the board's single overall \
             outline (replacing whatever is there), use `set_board_size`; calling this one twice \
             with the same rectangle leaves 8 overlapping segments that fail DRC.",
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
        tool!(
            "delete_board_graphic",
            "Delete board graphic primitives (gr_line, gr_rect, gr_circle, gr_arc, gr_poly, \
             gr_curve, gr_text) selected by layer, uuid, type, and/or bounding box. This is the \
             counterpart to add_board_outline / add_board_text / import_svg_logo — `delete_trace` \
             only removes tracks, so without this there is no way to undo a graphic. \
             At least one selector is required; a call with none is refused rather than \
             interpreted as 'delete everything'. Only top-level graphics are touched: a \
             footprint's own silkscreen and courtyard shapes (fp_*) and copper tracks are never \
             deleted. Edits the file directly, so save or close the board in KiCAD first.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "layer": { "type": "string", "description": "Only delete graphics on this layer (e.g. 'Edge.Cuts', 'F.SilkS')" },
                    "uuid":  { "type": "string", "description": "Only delete the graphic with this uuid" },
                    "types": {
                        "type": "array",
                        "description": "Only delete these primitive types, e.g. ['gr_line', 'gr_rect']. Default: all graphic types.",
                        "items": { "type": "string" }
                    },
                    "bbox": {
                        "type": "object",
                        "description": "Only delete graphics lying entirely within this rectangle (mm)",
                        "properties": {
                            "x1": { "type": "number" },
                            "y1": { "type": "number" },
                            "x2": { "type": "number" },
                            "y2": { "type": "number" }
                        },
                        "required": ["x1", "y1", "x2", "y2"]
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_delete_board_graphic(args, ctx).await }
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

    // Set semantics: the outline is replaced, not appended to. Appending was
    // the bug — two calls left 8 overlapping Edge.Cuts segments forming a
    // self-intersecting, non-closed outline that fails DRC.
    let replace = args["replace"].as_bool().unwrap_or(true);

    let x2 = ox + width;
    let y2 = oy + height;
    let w = 0.05_f64;

    // IPC only when the board KiCAD has open *is* the file we were handed;
    // otherwise this would edit somebody else's board and report success.
    // ponytail: 4 segments over a single BoardRectangle keeps one builder path;
    // switch to board_rectangle if a native rect proves less flaky.
    if ipc_targets_board(ctx.config.ipc_address.clone(), &board_path).await {
        let items = rect_outline_items(ox, oy, x2, y2, w);
        let outcome = with_ipc(ctx.config.ipc_address.clone(), move |c| {
            // create_items appends, so clearing first is what makes this a set.
            let removed = if replace {
                c.delete_shapes_on_layer("Edge.Cuts")?
            } else {
                0
            };
            c.create_items(items)?;
            Ok(removed)
        })
        .await?;
        if let Ok(removed) = outcome {
            return Ok(CallToolResult::json(&json!({
                "width": width, "height": height,
                "x1": ox, "y1": oy, "x2": x2, "y2": y2,
                "replaced": replace, "removed_graphics": removed,
                "target": board_path.display().to_string(),
                "source": "ipc"
            })));
        }
    }

    // The 4 Edge.Cuts lines (top, right, bottom, left) of the new rectangle.
    let lines = format!(
        "{}{}{}{}",
        format_gr_line(ox, oy, x2, oy, "Edge.Cuts", w),
        format_gr_line(x2, oy, x2, y2, "Edge.Cuts", w),
        format_gr_line(x2, y2, ox, y2, "Edge.Cuts", w),
        format_gr_line(ox, y2, ox, oy, "Edge.Cuts", w),
    );

    let content = std::fs::read_to_string(&board_path)?;

    let mut edits = Vec::new();
    let mut removed = 0usize;
    if replace {
        // Outline *shapes* only. A gr_text on Edge.Cuts is a fab note, not part
        // of the outline, and the IPC path (delete_shapes_on_layer, which acts
        // on KOT_PCB_SHAPE) leaves it alone too — the two must agree.
        for g in collect_board_graphics(&content)
            .iter()
            .filter(|g| g.layer.as_deref() == Some("Edge.Cuts") && g.tag != "gr_text")
        {
            match deletion_range(&content, g) {
                Some((s, e)) => {
                    edits.push(SexpEdit::delete(s, e));
                    removed += 1;
                }
                None => {
                    return Ok(CallToolResult::error(format!(
                        "Refusing to replace the board outline: could not establish the byte \
                         range of an existing ({}) on Edge.Cuts in '{}'. Nothing was written.",
                        g.tag,
                        board_path.display()
                    )))
                }
            }
        }
    }

    let close_pos = match root_close_offset(&content) {
        Some(p) => p,
        None => {
            return Ok(CallToolResult::error(format!(
                "'{}' has no balanced (kicad_pcb …) root block — refusing to edit it.",
                board_path.display()
            )))
        }
    };
    edits.push(SexpEdit::insert(close_pos, lines));

    let new_content = apply_edits(content, edits);
    if let Err(why) = check_document(&new_content, "kicad_pcb") {
        return Ok(CallToolResult::error(format!(
            "Internal error: the edit would have corrupted '{}' ({why}) — nothing was written.",
            board_path.display()
        )));
    }
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "width": width, "height": height,
        "x1": ox, "y1": oy, "x2": x2, "y2": y2,
        "replaced": replace, "removed_graphics": removed,
        "target": board_path.display().to_string(),
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

    // Only ask KiCAD when the board it has open is the one that was requested —
    // otherwise this reports another board's extents under the caller's path.
    if ipc_targets_board(ctx.config.ipc_address.clone(), &board_path).await {
        if let Ok(ext) = with_ipc(ctx.config.ipc_address.clone(), |c| c.get_board_extents()).await? {
            return Ok(CallToolResult::json(&json!({
                "x_min": ext.min.x, "y_min": ext.min.y,
                "x_max": ext.max.x, "y_max": ext.max.y,
                "width": ext.max.x - ext.min.x,
                "height": ext.max.y - ext.min.y,
                "target": board_path.display().to_string(),
                "source": "ipc"
            })));
        }
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
        return Ok(CallToolResult::json(&json!({
            "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0, "width": 0, "height": 0,
            "target": board_path.display().to_string(),
            "source": "empty"
        })));
    }

    Ok(CallToolResult::json(&json!({
        "x_min": min_x, "y_min": min_y,
        "x_max": max_x, "y_max": max_y,
        "width": max_x - min_x,
        "height": max_y - min_y,
        "target": board_path.display().to_string(),
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

    // IPC only when KiCAD's open board is the file we were handed; otherwise
    // fall through so the caller's own file is what changes.
    if ipc_targets_board(ctx.config.ipc_address.clone(), &board_path).await {
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
                "target": board_path.display().to_string(),
                "source": "ipc"
            })));
        }
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
        "target": board_path.display().to_string(),
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

    // IPC only when KiCAD's open board is the file we were handed; otherwise
    // fall through so the caller's own file is what changes.
    if ipc_targets_board(ctx.config.ipc_address.clone(), &board_path).await {
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
                "target": board_path.display().to_string(),
                "source": "ipc"
            })));
        }
    }

    let gr_text = format_gr_text(&text, x, y, rotation, &layer, size);
    let content = std::fs::read_to_string(&board_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, gr_text)]);
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "text": text, "x": x, "y": y, "layer": layer, "size": size,
        "target": board_path.display().to_string(),
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
    // An unresolved net used to become net 0: a pour joined to nothing, which
    // DRC does not flag, so a "GND plane" that isn't connected to GND survives
    // all the way to fabrication.
    let Some(net) = crate::tools::resolve_net(&content, &net_name) else {
        return Ok(crate::tools::net_not_found_error(&content, &net_name));
    };
    let zone_sexp = format_zone_polygon(&net, &layer, clearance, min_width, &points);

    let close_pos = root_close_offset(&content).unwrap_or(content.len());
    let new_content = apply_edits(content, vec![SexpEdit::insert(close_pos, zone_sexp)]);
    if let Err(why) = konnect_sexp::writer::check_document(&new_content, "kicad_pcb") {
        return Ok(CallToolResult::error(format!(
            "Internal error: adding the zone would have corrupted '{}' ({why}) — \
             nothing was written.",
            board_path.display()
        )));
    }
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "net": net.name(), "layer": layer,
        "point_count": points.len(),
        "net_id": net.code()
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

    // IPC only when KiCAD's open board is the file we were handed; otherwise
    // fall through so the caller's own file is what changes.
    if ipc_targets_board(ctx.config.ipc_address.clone(), &board_path).await {
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
                "target": board_path.display().to_string(),
                "source": "ipc"
            })));
        }
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
        "target": board_path.display().to_string(),
        "source": "file"
    })))
}

async fn handle_delete_board_graphic(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let layer = args["layer"].as_str();
    let uuid = args["uuid"].as_str();
    let types: Vec<String> = args["types"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let bbox = args.get("bbox").and_then(|b| {
        let (x1, y1, x2, y2) = (
            b["x1"].as_f64()?,
            b["y1"].as_f64()?,
            b["x2"].as_f64()?,
            b["y2"].as_f64()?,
        );
        Some((x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)))
    });

    // A selector-less call would mean "delete every graphic on the board".
    // That is never what an automation step intends, so refuse it.
    if layer.is_none() && uuid.is_none() && bbox.is_none() && types.is_empty() {
        return Ok(CallToolResult::error(
            "delete_board_graphic needs at least one selector (layer, uuid, types, or bbox). \
             Refusing to delete every graphic on the board.",
        ));
    }
    if let Some(bad) = types.iter().find(|t| !GRAPHIC_TAGS.contains(&t.as_str())) {
        return Ok(CallToolResult::error(format!(
            "Unknown graphic type '{bad}'. Valid types: {}.",
            GRAPHIC_TAGS.join(", ")
        )));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let matched: Vec<BoardGraphic> = collect_board_graphics(&content)
        .into_iter()
        .filter(|g| uuid.is_none_or(|u| g.uuid.as_deref() == Some(u)))
        .filter(|g| layer.is_none_or(|l| g.layer.as_deref() == Some(l)))
        .filter(|g| types.is_empty() || types.iter().any(|t| t == &g.tag))
        .filter(|g| match bbox {
            None => true,
            // Only fully-contained graphics: a partial overlap is ambiguous and
            // deleting on it would remove more than the caller pointed at.
            Some((bx1, by1, bx2, by2)) => g
                .bbox
                .is_some_and(|(x1, y1, x2, y2)| x1 >= bx1 && y1 >= by1 && x2 <= bx2 && y2 <= by2),
        })
        .collect();

    let deleted: Vec<serde_json::Value> = matched
        .iter()
        .map(|g| json!({ "type": g.tag, "layer": g.layer, "uuid": g.uuid }))
        .collect();

    if matched.is_empty() {
        return Ok(CallToolResult::json(&json!({
            "deleted": 0,
            "items": deleted,
            "note": "No top-level graphic matched the given selectors; nothing was written.",
            "target": board_path.display().to_string()
        })));
    }

    let mut edits = Vec::new();
    for g in &matched {
        match deletion_range(&content, g) {
            Some((s, e)) => edits.push(SexpEdit::delete(s, e)),
            None => {
                return Ok(CallToolResult::error(format!(
                    "Refusing to delete: could not establish the byte range of a ({}) in '{}'. \
                     Nothing was written.",
                    g.tag,
                    board_path.display()
                )))
            }
        }
    }

    let new_content = apply_edits(content, edits);
    if let Err(why) = check_document(&new_content, "kicad_pcb") {
        return Ok(CallToolResult::error(format!(
            "Internal error: the delete would have corrupted '{}' ({why}) — nothing was written.",
            board_path.display()
        )));
    }
    write_atomic(&board_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "deleted": matched.len(),
        "items": deleted,
        "target": board_path.display().to_string()
    })))
}

#[cfg(test)]
mod outline_and_delete_tests {
    //! Fixtures are **tab-indented**, like every file KiCAD 10 writes. The
    //! codebase's own writer emits two spaces, so any matcher with hardcoded
    //! leading whitespace passes against self-written files and silently finds
    //! nothing in the user's real board — the dominant bug class here.

    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        ToolContext::new(
            // Empty ipc_address: ipc_targets_board short-circuits to false, so
            // the file path is exercised without a live KiCAD.
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

    /// A tab-indented board with: a footprint carrying its own `fp_line` on
    /// Edge.Cuts (must never be deleted), a `gr_text` whose *quoted* value holds
    /// an unbalanced `(` (must not throw off paren counting), and one existing
    /// top-level Edge.Cuts outline.
    fn tab_board() -> String {
        [
            "(kicad_pcb",
            "\t(version 20250610)",
            "\t(generator \"pcbnew\")",
            "\t(paper \"A4\")",
            "\t(net 0 \"\")",
            "\t(footprint \"Resistor_SMD:R_0603\"",
            "\t\t(layer \"F.Cu\")",
            "\t\t(uuid \"fp-1\")",
            "\t\t(at 50 50)",
            "\t\t(fp_line",
            "\t\t\t(start -1 -1)",
            "\t\t\t(end 1 -1)",
            "\t\t\t(stroke (width 0.1) (type solid))",
            "\t\t\t(layer \"Edge.Cuts\")",
            "\t\t\t(uuid \"fp-line-1\")",
            "\t\t)",
            "\t)",
            "\t(gr_text \"BOARD (rev A\"",
            "\t\t(at 10 10 0)",
            "\t\t(layer \"F.SilkS\")",
            "\t\t(uuid \"keep-me\")",
            "\t)",
            "\t(gr_rect",
            "\t\t(start 5 5)",
            "\t\t(end 20 20)",
            "\t\t(stroke (width 0.1) (type solid))",
            "\t\t(layer \"Edge.Cuts\")",
            "\t\t(uuid \"old-outline\")",
            "\t)",
            ")",
            "",
        ]
        .join("\n")
    }

    fn board_file(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("test.kicad_pcb");
        std::fs::write(&p, tab_board()).unwrap();
        p
    }

    fn body(result: &CallToolResult) -> serde_json::Value {
        match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => {
                serde_json::from_str(text).expect("result must be JSON")
            }
            _ => panic!("expected text content"),
        }
    }

    /// Every top-level graphic on `layer`, read back out of the written file.
    fn graphics_on(content: &str, layer: &str) -> Vec<BoardGraphic> {
        collect_board_graphics(content)
            .into_iter()
            .filter(|g| g.layer.as_deref() == Some(layer))
            .collect()
    }

    #[test]
    fn collect_board_graphics_sees_only_top_level_blocks() {
        let content = tab_board();
        let graphics = collect_board_graphics(&content);
        let tags: Vec<&str> = graphics.iter().map(|g| g.tag.as_str()).collect();
        // The footprint's fp_line is not a board graphic, and the unbalanced
        // '(' inside the gr_text string must not shift any offset.
        assert_eq!(tags, vec!["gr_text", "gr_rect"]);
        assert_eq!(graphics[0].uuid.as_deref(), Some("keep-me"));
        assert_eq!(graphics[1].layer.as_deref(), Some("Edge.Cuts"));
        assert_eq!(graphics[1].bbox, Some((5.0, 5.0, 20.0, 20.0)));
        // Each located range really is the block it claims to be.
        for g in &graphics {
            assert!(content[g.start..g.end].starts_with(&format!("({}", g.tag)));
            assert!(content[g.start..g.end].ends_with(')'));
        }
    }

    #[tokio::test]
    async fn set_board_size_twice_leaves_exactly_four_edge_cuts_segments() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();
        let args = json!({
            "board": board.to_str().unwrap(), "width": 50.0, "height": 30.0
        });

        for _ in 0..2 {
            let result = handle_set_board_size(&args, &ctx).await.unwrap();
            assert!(!result.is_error, "{:?}", body(&result));
            assert_eq!(body(&result)["source"], json!("file"));
        }

        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").expect("board must still parse");

        let edge = graphics_on(&content, "Edge.Cuts");
        assert_eq!(
            edge.len(),
            4,
            "two set_board_size calls must leave one rectangle, got {} segments",
            edge.len()
        );
        assert!(edge.iter().all(|g| g.tag == "gr_line"));
        // The pre-existing gr_rect outline was replaced, not stacked on.
        assert!(!content.contains("old-outline"));
        // Unrelated items survive untouched.
        assert!(content.contains("keep-me"));
        assert!(content.contains("fp-line-1"));
        assert!(content.contains("(footprint \"Resistor_SMD:R_0603\""));
    }

    #[tokio::test]
    async fn set_board_size_reports_the_file_it_actually_edited() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();
        let result = handle_set_board_size(
            &json!({ "board": board.to_str().unwrap(), "width": 10.0, "height": 10.0 }),
            &ctx,
        )
        .await
        .unwrap();
        let b = body(&result);
        assert_eq!(b["source"], json!("file"));
        assert_eq!(b["target"], json!(board.display().to_string()));
        assert_eq!(b["replaced"], json!(true));
        assert_eq!(b["removed_graphics"], json!(1));
    }

    #[tokio::test]
    async fn set_board_size_with_replace_false_appends() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();
        let args = json!({
            "board": board.to_str().unwrap(),
            "width": 50.0, "height": 30.0, "replace": false
        });
        for _ in 0..2 {
            assert!(!handle_set_board_size(&args, &ctx).await.unwrap().is_error);
        }
        let content = std::fs::read_to_string(&board).unwrap();
        // 1 pre-existing gr_rect + 2 × 4 appended segments.
        assert_eq!(graphics_on(&content, "Edge.Cuts").len(), 9);
    }

    #[tokio::test]
    async fn add_board_outline_stays_additive() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();
        let args = json!({
            "board": board.to_str().unwrap(),
            "x1": 0.0, "y1": 0.0, "x2": 40.0, "y2": 25.0
        });
        for _ in 0..2 {
            assert!(!handle_add_board_outline(&args, &ctx).await.unwrap().is_error);
        }
        let content = std::fs::read_to_string(&board).unwrap();
        check_document(&content, "kicad_pcb").unwrap();
        assert_eq!(graphics_on(&content, "Edge.Cuts").len(), 9);
        assert!(content.contains("old-outline"));
    }

    #[tokio::test]
    async fn delete_board_graphic_by_uuid_removes_exactly_that_item() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();
        let ctx = test_ctx();

        let result = handle_delete_board_graphic(
            &json!({ "board": board.to_str().unwrap(), "uuid": "old-outline" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert_eq!(body(&result)["deleted"], json!(1));

        let after = std::fs::read_to_string(&board).unwrap();
        check_document(&after, "kicad_pcb").expect("board must still parse");
        assert!(!after.contains("old-outline"));
        // Everything else is byte-for-byte intact.
        assert!(after.contains("keep-me"));
        assert!(after.contains("fp-line-1"));
        assert!(after.contains("\t(gr_text \"BOARD (rev A\""));
        assert!(after.contains("\t(net 0 \"\")"));
        assert_eq!(collect_board_graphics(&after).len(), 1);
        assert!(after.len() < before.len());
    }

    #[tokio::test]
    async fn delete_board_graphic_by_layer_spares_footprint_graphics() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();

        let result = handle_delete_board_graphic(
            &json!({ "board": board.to_str().unwrap(), "layer": "Edge.Cuts" }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(body(&result)["deleted"], json!(1));

        let after = std::fs::read_to_string(&board).unwrap();
        check_document(&after, "kicad_pcb").unwrap();
        assert!(graphics_on(&after, "Edge.Cuts").is_empty());
        // The footprint's own Edge.Cuts fp_line is not a board graphic.
        assert!(after.contains("fp-line-1"));
    }

    #[tokio::test]
    async fn delete_board_graphic_bbox_only_takes_fully_contained_items() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();

        // The gr_text sits at (10,10); the gr_rect spans (5,5)-(20,20) and so
        // pokes outside this window.
        let result = handle_delete_board_graphic(
            &json!({
                "board": board.to_str().unwrap(),
                "bbox": { "x1": 0.0, "y1": 0.0, "x2": 15.0, "y2": 15.0 }
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(body(&result)["deleted"], json!(1));

        let after = std::fs::read_to_string(&board).unwrap();
        check_document(&after, "kicad_pcb").unwrap();
        assert!(!after.contains("keep-me"));
        assert!(after.contains("old-outline"));
    }

    #[tokio::test]
    async fn delete_board_graphic_without_selectors_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();
        let ctx = test_ctx();

        let result =
            handle_delete_board_graphic(&json!({ "board": board.to_str().unwrap() }), &ctx)
                .await
                .unwrap();
        assert!(result.is_error);
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn delete_board_graphic_rejects_unknown_types() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();
        let result = handle_delete_board_graphic(
            &json!({ "board": board.to_str().unwrap(), "types": ["segment"] }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn delete_board_graphic_with_no_match_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let before = std::fs::read_to_string(&board).unwrap();
        let ctx = test_ctx();
        let result = handle_delete_board_graphic(
            &json!({ "board": board.to_str().unwrap(), "uuid": "not-on-this-board" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert_eq!(body(&result)["deleted"], json!(0));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), before);
    }

    #[tokio::test]
    async fn empty_ipc_address_never_claims_an_ipc_source() {
        // The regression guard for the retargeting bug: with no reachable
        // KiCAD, every handler must edit the named file and say so.
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path());
        let ctx = test_ctx();
        assert!(!ipc_targets_board(String::new(), &board).await);

        for result in [
            handle_get_board_extents(&json!({ "board": board.to_str().unwrap() }), &ctx)
                .await
                .unwrap(),
            handle_add_board_text(
                &json!({ "board": board.to_str().unwrap(), "text": "T", "x": 1.0, "y": 1.0 }),
                &ctx,
            )
            .await
            .unwrap(),
        ] {
            assert_ne!(body(&result)["source"], json!("ipc"));
            assert_eq!(body(&result)["target"], json!(board.display().to_string()));
        }
    }
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

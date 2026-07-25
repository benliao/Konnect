//! `sch_batch` toolset — bulk/batch operations on schematic elements.
//!
//! **Critical invariant**: every write handler performs a single file read,
//! collects ALL mutations as `SexpEdit` values against the original content,
//! then writes *at most* once — skipping the write entirely when nothing
//! matched, so a batch of typos cannot pass itself off as a successful edit.
//! This fixes the Python bug where `batch_connect_to_net` did N separate
//! read/write cycles.
//!
//! That write goes through `write_atomic_checked`, not `write_atomic`: these
//! handlers splice raw text at computed byte offsets, so the result is
//! re-parsed and the call fails rather than leaving behind a file KiCAD
//! cannot open.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_symbol_instance_block, get_path, opt_str, rename_symbol_edits, require_f64, require_str,
    ToolDef,
};
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident, snap_point},
    schematic::{
        extract_labels, extract_lib_pins_resolved, extract_symbol_instances, extract_wires,
        format_net_label, format_wire, pin_endpoint, read_schematic,
    },
    writer::{
        apply_edits, find_block_with_leading_whitespace, find_top_level_item, new_uuid,
        write_atomic_checked, SexpEdit,
    },
};
use serde_json::json;

// Re-use the crate-internal net-graph primitives from sch_analysis.
use super::sch_analysis::build_net_graph;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "batch_connect_to_net",
            "Connect multiple component pins to a named net by adding net labels at each pin \
             endpoint. Single file read → all labels inserted → single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Name of the net to connect pins to" },
                    "pins": {
                        "type": "array",
                        "description": "List of {reference, pin_number} objects to connect",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "pin_number": { "type": "string" }
                            },
                            "required": ["reference", "pin_number"]
                        }
                    }
                },
                "required": ["schematic", "net_name", "pins"]
            }),
            |args, ctx| async move { handle_batch_connect_to_net(args, ctx).await }
        ),
        tool!(
            "batch_delete",
            "Delete multiple schematic items (wires, labels, junctions, components) by UUID \
             or component reference designator — single file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "uuids": {
                        "type": "array",
                        "description": "UUIDs of items to delete",
                        "items": { "type": "string" }
                    },
                    "references": {
                        "type": "array",
                        "description": "Component reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_batch_delete(args, ctx).await }
        ),
        tool!(
            "bulk_move_schematic_components",
            "Move multiple components by a uniform dx/dy offset in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to move",
                        "items": { "type": "string" }
                    },
                    "dx": { "type": "number", "description": "X offset in mm" },
                    "dy": { "type": "number", "description": "Y offset in mm" }
                },
                "required": ["schematic", "references", "dx", "dy"]
            }),
            |args, ctx| async move { handle_bulk_move(args, ctx).await }
        ),
        tool!(
            "batch_edit_schematic_components",
            "Apply field updates (Value, Footprint, custom properties) and reference-designator \
             renames to multiple components in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "edits": {
                        "type": "array",
                        "description": "List of {reference, new_reference?, value?, footprint?, fields?} edit objects",
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "new_reference": {
                                    "type": "string",
                                    "description": "New reference designator (updates both the \
                                                    Reference property and the instances entry \
                                                    that PCB sync reads)"
                                },
                                "value": { "type": "string" },
                                "footprint": { "type": "string" },
                                "fields": {
                                    "type": "object",
                                    "description": "Additional property fields as key:value pairs"
                                }
                            },
                            "required": ["reference"]
                        }
                    }
                },
                "required": ["schematic", "edits"]
            }),
            |args, ctx| async move { handle_batch_edit(args, ctx).await }
        ),
        tool!(
            "batch_delete_schematic_components",
            "Delete multiple components by reference designator in a single atomic file write.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "description": "Reference designators to delete",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_delete_components(args, ctx).await }
        ),
        tool!(
            "connect_passthrough",
            "Add a wire stub and matching net label at a point to route a signal through \
             a region without drawing a full wire path. Direction controls stub orientation.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "net_name": { "type": "string", "description": "Net name for the passthrough label" },
                    "x": { "type": "number", "description": "X position of the stub root in mm" },
                    "y": { "type": "number", "description": "Y position of the stub root in mm" },
                    "direction": {
                        "type": "string",
                        "description": "Stub direction: 'left', 'right', 'up', 'down'",
                        "default": "right"
                    }
                },
                "required": ["schematic", "net_name", "x", "y"]
            }),
            |args, ctx| async move { handle_connect_passthrough(args, ctx).await }
        ),
        tool!(
            "add_schematic_text",
            "Add a text annotation (non-net label) to the schematic at a given position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "text": { "type": "string", "description": "Text content to add" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "size": { "type": "number", "description": "Font size in mm", "default": 1.27 },
                    "rotation": { "type": "number", "description": "Rotation in degrees", "default": 0 }
                },
                "required": ["schematic", "text", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_text(args, ctx).await }
        ),
        tool!(
            "get_schematic_layout",
            "Return a compact spatial summary of the schematic: component positions, \
             bounding box, and optionally wire segments and label locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "include_wires": { "type": "boolean", "description": "Include wire data", "default": true },
                    "include_labels": { "type": "boolean", "description": "Include label data", "default": true }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_layout(args, ctx).await }
        ),
        tool!(
            "validate_wire_connections",
            "Check all wire endpoints for floating ends (not connected to a pin, label, \
             or another wire). Reports each floating endpoint with its coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "tolerance": { "type": "number", "description": "Snap tolerance in mm", "default": 0.01 }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_wire_connections(args, ctx).await }
        ),
        tool!(
            "validate_component_connections",
            "Check that every non-passive pin on every component has at least one wire \
             or label connected. Reports unconnected pins with reference, pin number, \
             and schematic position.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "ignore_power_pins": {
                        "type": "boolean",
                        "description": "Skip power-type pins in the check",
                        "default": false
                    },
                    "references": {
                        "type": "array",
                        "description": "Limit check to these reference designators (empty = all)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_validate_component_connections(args, ctx).await }
        ),
    ]
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Find the `(symbol ...)` block for a reference designator, plus its leading
/// whitespace so deletion leaves clean formatting.
/// Returns `(block_start, block_end)` byte offsets in `content`.
fn find_symbol_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    let (sym_start, _) = find_symbol_instance_block(content, reference)?;
    find_block_with_leading_whitespace(content, sym_start)
}

/// Return `(val_start, val_end)` byte offsets in `content` for the *value* portion
/// of a `(property "FieldName" "VALUE" ...)` node within the symbol identified by
/// `reference`. Only the bytes inside the opening quote are included (i.e. the
/// replacement does NOT need to include surrounding quotes).
fn field_value_range(content: &str, reference: &str, field: &str) -> Option<(usize, usize)> {
    let (sym_start, sym_end) = find_symbol_instance_block(content, reference)?;
    let sym_block = &content[sym_start..sym_end];

    let field_search = format!(r#"(property "{field}" ""#);
    let field_rel = sym_block.find(&field_search)?;
    let val_start = sym_start + field_rel + field_search.len();
    // find the closing quote of the current value
    let val_end = val_start + content[val_start..].find('"')?;
    Some((val_start, val_end))
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_batch_connect_to_net(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net_name = match require_str(args, "net_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pins = match args["pins"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'pins' array")),
    };

    let (content, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut inserts = String::new();
    let mut added: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for pin_spec in &pins {
        let reference = match pin_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in pin spec".into());
                continue;
            }
        };
        let pin_number = match pin_spec["pin_number"].as_str() {
            Some(p) => p,
            None => {
                errors.push("Missing 'pin_number' in pin spec".into());
                continue;
            }
        };

        let inst = match instances.iter().find(|i| i.reference == reference) {
            Some(i) => i,
            None => {
                errors.push(format!("Component '{}' not found", reference));
                continue;
            }
        };

        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));

        let pin_ep = lib_sym.and_then(|sym| {
            // `_resolved` walks `(extends "Parent")`: derived symbols carry
            // no pins themselves, so the label landed nowhere for them.
            extract_lib_pins_resolved(sym, &lib_syms)
                .into_iter()
                .find(|p| p.number == pin_number)
                .map(|p| pin_endpoint(&p, inst.pin_transform()))
        });

        match pin_ep {
            Some((px, py)) => {
                inserts.push_str(&format_net_label(&net_name, px, py, 0.0));
                added.push(json!({
                    "reference": reference,
                    "pin": pin_number,
                    "x": px,
                    "y": py
                }));
            }
            None => errors.push(format!("Pin '{}' not found on '{}'", pin_number, reference)),
        }
    }

    if !inserts.is_empty() {
        let close_pos = content.rfind(')').unwrap_or(content.len());
        let edits = vec![SexpEdit::insert(close_pos, inserts)];
        let new_content = apply_edits(content, edits);
        // Checked: the label text is spliced in at a computed byte offset, so a
        // bad offset must fail the call rather than leave behind a file KiCAD
        // cannot open.
        write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;
    }

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "added": added,
        "added_count": added.len(),
        "errors": errors
    })))
}

async fn handle_batch_delete(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let content = std::fs::read_to_string(&sch_path)?;

    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Delete by UUID — walk back from uuid node to enclosing top-level block
    if let Some(uuids) = args["uuids"].as_array() {
        for uuid_val in uuids {
            let uuid = match uuid_val.as_str() {
                Some(u) => u,
                None => continue,
            };
            let pattern = format!(r#"(uuid "{}")"#, uuid);
            match content.find(&pattern) {
                Some(uuid_pos) => {
                    // Indentation-agnostic: this used to walk back to a literal
                    // "\n  (", which finds nothing in the tab-indented files
                    // eeschema writes, so deleting by UUID failed outright on
                    // every real KiCAD schematic.
                    match find_top_level_item(&content, uuid_pos)
                        .and_then(|(start, _)| find_block_with_leading_whitespace(&content, start))
                    {
                        Some((del_start, del_end)) => {
                            edits.push(SexpEdit::delete(del_start, del_end));
                            deleted.push(uuid.to_string());
                        }
                        None => errors.push(format!("Cannot locate block for UUID '{}'", uuid)),
                    }
                }
                None => errors.push(format!("UUID '{}' not found", uuid)),
            }
        }
    }

    // Delete by reference designator
    if let Some(refs) = args["references"].as_array() {
        for ref_val in refs {
            let reference = match ref_val.as_str() {
                Some(r) => r,
                None => continue,
            };
            match find_symbol_block(&content, reference) {
                Some((del_start, del_end)) => {
                    edits.push(SexpEdit::delete(del_start, del_end));
                    deleted.push(reference.to_string());
                }
                None => errors.push(format!("Component '{}' not found", reference)),
            }
        }
    }

    // Nothing matched: rewriting the file with identical content would bump its
    // mtime and present a no-op as a successful delete.
    if !edits.is_empty() {
        let new_content = apply_edits(content, edits);
        // Checked: whole blocks are cut out at computed byte offsets, so an
        // off-by-one that unbalances the parens must fail the call rather than
        // replace the user's file with one KiCAD cannot open.
        write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;
    }

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_bulk_move(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"].as_array().cloned().unwrap_or_default();
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut moved: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };

        // Locate symbol block for this reference
        let (sym_start, sym_end) = match find_symbol_instance_block(&content, reference) {
            Some(r) => r,
            None => {
                errors.push(format!("'{}' not found", reference));
                continue;
            }
        };

        // Find first (at X Y [ROT]) inside this symbol block
        let sym_block = &content[sym_start..sym_end];
        let at_pat = "(at ";
        let at_rel = match sym_block.find(at_pat) {
            Some(r) => r,
            None => {
                errors.push(format!("No (at) in symbol '{}'", reference));
                continue;
            }
        };
        let at_abs = sym_start + at_rel + at_pat.len();
        let close_rel = sym_block[at_rel..].find(')').unwrap_or(0);
        let at_end = sym_start + at_rel + close_rel;

        let at_str = &content[at_abs..at_end];
        let parts: Vec<&str> = at_str.split_whitespace().collect();
        let x = parts
            .first()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let y = parts
            .get(1)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let rot = parts
            .get(2)
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let (new_x, new_y) = snap_point(x + dx, y + dy, 1.27);
        edits.push(SexpEdit::replace(
            at_abs,
            at_end,
            format!("{new_x} {new_y} {rot}"),
        ));
        moved.push(json!({
            "reference": reference,
            "old_x": x, "old_y": y,
            "new_x": new_x, "new_y": new_y
        }));
    }

    // Nothing matched: rewriting the file with identical content would bump its
    // mtime and present a no-op as a successful move.
    if !edits.is_empty() {
        let new_content = apply_edits(content, edits);
        // Checked: these are raw string splices, so a mis-computed offset must
        // fail the call rather than replace the user's file with one KiCAD
        // cannot open.
        write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;
    }

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved,
        "dx": dx, "dy": dy,
        "errors": errors
    })))
}

async fn handle_batch_edit(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let edits_arr = match args["edits"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'edits' array")),
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let mut file_edits: Vec<SexpEdit> = Vec::new();
    let mut changed: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for edit_spec in &edits_arr {
        let reference = match edit_spec["reference"].as_str() {
            Some(r) => r,
            None => {
                errors.push("Missing 'reference' in edit spec".into());
                continue;
            }
        };

        let mut component_changes: Vec<String> = Vec::new();

        // A designator is stored twice — in the `Reference` property and in the
        // `(instances … (reference "…"))` entry that "Update PCB from
        // Schematic" reads — so it can never go through the single-property
        // field path, however the caller spells the request.
        let new_ref = edit_spec["new_reference"]
            .as_str()
            .or_else(|| edit_spec["fields"]["Reference"].as_str());
        if let Some(new_ref) = new_ref {
            match rename_symbol_edits(&content, reference, new_ref) {
                Ok((rename_edits, outcome)) => {
                    file_edits.extend(rename_edits);
                    component_changes.push(format!("Reference → {}", new_ref));
                    if outcome.instances > 0 {
                        component_changes.push(format!(
                            "instances reference → {} ({})",
                            new_ref, outcome.instances
                        ));
                    } else {
                        errors.push(format!(
                            "instances: '{}' has no (instances …) entry, so \
                             'Update PCB from Schematic' will not see the new name",
                            reference
                        ));
                    }
                }
                Err(why) => errors.push(format!("Reference on '{}': {}", reference, why)),
            }
        }

        // Standard fields
        for (field, key) in &[("Value", "value"), ("Footprint", "footprint")] {
            if let Some(new_val) = edit_spec[key].as_str() {
                match field_value_range(&content, reference, field) {
                    Some((start, end)) => {
                        file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
                        component_changes.push(format!("{} → {}", field, new_val));
                    }
                    None => errors.push(format!("Field '{}' not found on '{}'", field, reference)),
                }
            }
        }

        // Arbitrary extra fields from "fields" object
        if let Some(fields_obj) = edit_spec["fields"].as_object() {
            for (field_name, field_val) in fields_obj {
                // Already renamed above, in both of its homes.
                if field_name == "Reference" {
                    continue;
                }
                if let Some(new_val) = field_val.as_str() {
                    match field_value_range(&content, reference, field_name) {
                        Some((start, end)) => {
                            file_edits.push(SexpEdit::replace(start, end, new_val.to_string()));
                            component_changes.push(format!("{} → {}", field_name, new_val));
                        }
                        None => errors.push(format!(
                            "Field '{}' not found on '{}'",
                            field_name, reference
                        )),
                    }
                }
            }
        }

        if !component_changes.is_empty() {
            changed.push(json!({
                "reference": reference,
                "changes": component_changes
            }));
        }
    }

    // Nothing matched: rewriting the file with identical content would bump its
    // mtime and, worse, present a no-op as a successful edit.
    if !file_edits.is_empty() {
        let new_content = apply_edits(content, file_edits);
        // Checked: these are raw string splices, so a mis-computed offset must
        // fail the call rather than replace the user's file with one KiCAD
        // cannot open.
        write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;
    }

    Ok(CallToolResult::json(&json!({
        "updated_count": changed.len(),
        "updated": changed,
        "errors": errors
    })))
}

async fn handle_batch_delete_components(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = match args["references"].as_array() {
        Some(a) => a.clone(),
        None => return Ok(CallToolResult::error("Missing 'references' array")),
    };

    let content = std::fs::read_to_string(&sch_path)?;
    let mut edits: Vec<SexpEdit> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for ref_val in &refs {
        let reference = match ref_val.as_str() {
            Some(r) => r,
            None => continue,
        };
        match find_symbol_block(&content, reference) {
            Some((del_start, del_end)) => {
                edits.push(SexpEdit::delete(del_start, del_end));
                deleted.push(reference.to_string());
            }
            None => errors.push(format!("Component '{}' not found", reference)),
        }
    }

    // Nothing matched: rewriting the file with identical content would bump its
    // mtime and present a no-op as a successful delete.
    if !edits.is_empty() {
        let new_content = apply_edits(content, edits);
        // Checked: whole blocks are cut out at computed byte offsets, so an
        // off-by-one that unbalances the parens must fail the call rather than
        // replace the user's file with one KiCAD cannot open.
        write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;
    }

    Ok(CallToolResult::json(&json!({
        "deleted_count": deleted.len(),
        "deleted": deleted,
        "errors": errors
    })))
}

async fn handle_connect_passthrough(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
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
    let direction = opt_str(args, "direction").unwrap_or("right");

    // Stub is 2.54mm (2×1.27 grid units)
    let stub = 2.54_f64;
    let (wire_end_x, wire_end_y, label_rot) = match direction {
        "left" => (x - stub, y, 180.0),
        "up" => (x, y - stub, 90.0),
        "down" => (x, y + stub, 270.0),
        _ => (x + stub, y, 0.0), // "right" default
    };

    let wire_sexp = format_wire(x, y, wire_end_x, wire_end_y);
    let label_sexp = format_net_label(&net_name, wire_end_x, wire_end_y, label_rot);

    let content = std::fs::read_to_string(&sch_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(
        close_pos,
        format!("{wire_sexp}{label_sexp}"),
    )];
    let new_content = apply_edits(content, edits);
    // Always exactly one insert, so there is no empty-batch case to guard —
    // but the splice lands at a computed offset, so it still gets re-parsed.
    write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;

    Ok(CallToolResult::json(&json!({
        "net": net_name,
        "stub_root": { "x": x, "y": y },
        "label_position": { "x": wire_end_x, "y": wire_end_y },
        "direction": direction,
        "label_rotation": label_rot
    })))
}

async fn handle_add_schematic_text(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
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
    let size = args["size"].as_f64().unwrap_or(1.27);
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let uuid = new_uuid();

    // Escape quotes in text content
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");

    let text_sexp = format!(
        "\n  (text \"{escaped}\"\n    (at {x} {y} {rotation})\n    \
         (effects (font (size {size} {size})))\n    (uuid \"{uuid}\")\n  )"
    );

    let content = std::fs::read_to_string(&sch_path)?;
    let close_pos = content.rfind(')').unwrap_or(content.len());
    let edits = vec![SexpEdit::insert(close_pos, text_sexp)];
    let new_content = apply_edits(content, edits);
    // Always exactly one insert, so there is no empty-batch case to guard —
    // but the text is user-supplied and spliced at a computed offset, so it
    // still gets re-parsed.
    write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;

    Ok(CallToolResult::json(&json!({
        "added": text,
        "x": x, "y": y,
        "size": size,
        "rotation": rotation,
        "uuid": uuid
    })))
}

async fn handle_get_layout(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let include_wires = args["include_wires"].as_bool().unwrap_or(true);
    let include_labels = args["include_labels"].as_bool().unwrap_or(true);

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);

    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|i| {
            json!({
                "reference": i.reference,
                "value": i.value,
                "lib_id": i.lib_id,
                "x": i.x, "y": i.y,
                "rotation": i.rotation,
                "mirror_x": i.mirror_x,
                "mirror_y": i.mirror_y
            })
        })
        .collect();

    // Bounding box over component origins
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    for i in &instances {
        min_x = min_x.min(i.x);
        min_y = min_y.min(i.y);
        max_x = max_x.max(i.x);
        max_y = max_y.max(i.y);
    }
    let bbox = if instances.is_empty() {
        json!({ "x_min": 0, "y_min": 0, "x_max": 0, "y_max": 0 })
    } else {
        json!({ "x_min": min_x, "y_min": min_y, "x_max": max_x, "y_max": max_y })
    };

    let mut result = json!({
        "component_count": components.len(),
        "components": components,
        "bounding_box": bbox
    });

    if include_wires {
        let wires = extract_wires(&tree);
        let wire_data: Vec<serde_json::Value> = wires
            .iter()
            .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2, "uuid": w.uuid }))
            .collect();
        result["wire_count"] = json!(wire_data.len());
        result["wires"] = json!(wire_data);
    }

    if include_labels {
        let labels = extract_labels(&tree);
        let label_data: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
            .collect();
        result["label_count"] = json!(label_data.len());
        result["labels"] = json!(label_data);
    }

    Ok(CallToolResult::json(&result))
}

async fn handle_validate_wire_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tol = args["tolerance"].as_f64().unwrap_or(0.01);

    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid pin endpoints
    let mut pin_points: Vec<(f64, f64)> = Vec::new();
    for inst in &instances {
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins_resolved(sym, &lib_syms) {
                pin_points.push(pin_endpoint(&pin, t));
            }
        }
    }

    let label_points: Vec<(f64, f64)> = labels.iter().map(|l| (l.x, l.y)).collect();
    // All wire endpoints as a flat list (for quick counting)
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    let is_connected = |px: f64, py: f64| -> bool {
        // Another wire endpoint at the same position (count >= 2 because px/py itself is in the list)
        let same_ep_count = all_wire_eps
            .iter()
            .filter(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
            .count();
        if same_ep_count >= 2 {
            return true;
        }

        // T-junction: lies on the INTERIOR of another wire
        if wires.iter().any(|w| {
            point_on_segment(px, py, w.x1, w.y1, w.x2, w.y2, tol)
                && !points_coincident(px, py, w.x1, w.y1, tol)
                && !points_coincident(px, py, w.x2, w.y2, tol)
        }) {
            return true;
        }

        // Label at this point
        if label_points
            .iter()
            .any(|(lx, ly)| points_coincident(px, py, *lx, *ly, tol))
        {
            return true;
        }

        // Pin endpoint at this point
        if pin_points
            .iter()
            .any(|(ppx, ppy)| points_coincident(px, py, *ppx, *ppy, tol))
        {
            return true;
        }

        false
    };

    let mut floating: Vec<serde_json::Value> = Vec::new();
    for w in &wires {
        for (px, py) in [(w.x1, w.y1), (w.x2, w.y2)] {
            if !is_connected(px, py) {
                floating.push(json!({ "x": px, "y": py, "wire_uuid": w.uuid }));
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": floating.is_empty(),
        "floating_count": floating.len(),
        "floating_endpoints": floating
    })))
}

async fn handle_validate_component_connections(
    args: &serde_json::Value,
    _ctx: &crate::tools::ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let filter_refs: Vec<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let tol = 0.01_f64;

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // No-connect positions (pins with intentional no-connect markers are exempt)
    let no_connect_pts: Vec<(f64, f64)> = tree
        .find_all("no_connect")
        .iter()
        .filter_map(|n| {
            let at = n.find("at")?;
            Some((at.get_f64(1)?, at.get_f64(2)?))
        })
        .collect();

    // Build net graph so we can check connectivity
    let mut g = build_net_graph(&wires, &labels);
    // Also build flat wire-endpoint list for direct presence checks
    let all_wire_eps: Vec<(f64, f64)> = wires
        .iter()
        .flat_map(|w| [(w.x1, w.y1), (w.x2, w.y2)])
        .collect();

    // `g.net_at` requires &mut self, so we need a `mut` closure.
    let mut has_connection = |px: f64, py: f64| -> bool {
        // Connected to a wire endpoint
        if all_wire_eps
            .iter()
            .any(|(wx, wy)| points_coincident(px, py, *wx, *wy, tol))
        {
            return true;
        }
        // Or has a named net (label at or reachable from pin via wires)
        g.net_at(px, py).is_some()
    };

    let mut unconnected: Vec<serde_json::Value> = Vec::new();

    for inst in &instances {
        if !filter_refs.is_empty() && !filter_refs.contains(&inst.reference) {
            continue;
        }
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            for pin in extract_lib_pins_resolved(sym, &lib_syms) {
                let (px, py) = pin_endpoint(&pin, t);

                // Skip intentional no-connects
                if no_connect_pts
                    .iter()
                    .any(|(nx, ny)| points_coincident(px, py, *nx, *ny, tol))
                {
                    continue;
                }

                if !has_connection(px, py) {
                    unconnected.push(json!({
                        "reference": inst.reference,
                        "value": inst.value,
                        "pin": pin.number,
                        "pin_name": pin.name,
                        "x": px,
                        "y": py
                    }));
                }
            }
        }
    }

    Ok(CallToolResult::json(&json!({
        "valid": unconnected.is_empty(),
        "unconnected_count": unconnected.len(),
        "unconnected_pins": unconnected
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::{ServerConfig, ToolContext};
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
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

    /// TAB-indented, the way eeschema/KiCAD 10 writes files — this crate's own
    /// writer uses two spaces, and every matcher has to cope with both.
    fn tab_indented_sch(path: &std::path::Path) {
        std::fs::write(
            path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\"\n\t\t\t\t(at 0 0 0)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(exclude_from_sim no)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 102 82 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"tabs\"\n\t\t\t\t(path \"/11111111-2222-3333-4444-555555555555\"\n\t\t\t\t\t(reference \"R1\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
    }

    /// A designator lives in the `Reference` property *and* in the instances
    /// entry, and it is the instance entry that "Update PCB from Schematic"
    /// reads. Routing `fields: {"Reference": …}` through the plain property
    /// writer left the file self-inconsistent and PCB sync failing on the old
    /// designator.
    #[tokio::test]
    async fn batch_rename_via_fields_updates_property_and_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let ctx = test_ctx();

        let result = handle_batch_edit(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{
                    "reference": "R1",
                    "fields": { "Reference": "R42" },
                    "value": "22k"
                }]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "batch edit failed: {:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#"(property "Reference" "R42""#),
            "Reference property not renamed:\n{text}"
        );
        assert!(
            text.contains(r#"(reference "R42")"#),
            "instances (reference …) not renamed — PCB sync reads this one:\n{text}"
        );
        assert!(!text.contains("\"R1\""), "old designator remains:\n{text}");
        // Other fields in the same spec still land, and the lib_symbols
        // definition's own Reference is untouched.
        assert!(text.contains(r#"(property "Value" "22k""#), "{text}");
        assert!(text.contains(r#"(property "Reference" "R""#), "{text}");
        // Raw-text editing must not drop nodes the typed model doesn't know.
        assert!(
            text.contains("(exclude_from_sim no)"),
            "editing dropped an unmodelled node:\n{text}"
        );
    }

    /// The explicit key does the same thing as `fields: {"Reference": …}`.
    #[tokio::test]
    async fn batch_rename_via_new_reference_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let ctx = test_ctx();

        let result = handle_batch_edit(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{ "reference": "R1", "new_reference": "R7" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"(property "Reference" "R7""#), "{text}");
        assert!(text.contains(r#"(reference "R7")"#), "{text}");
    }

    /// Multi-unit parts repeat the designator across one `(symbol …)` block per
    /// unit. Every unit's *both* copies must move, and since the batch handler
    /// collects offsets against the original content, the second unit's edits
    /// must not be computed against text the first unit's edits shifted.
    #[tokio::test]
    async fn batch_rename_covers_every_unit_of_a_multi_unit_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.kicad_sch");
        let unit = |n: u32, y: u32| {
            format!("\t(symbol\n\t\t(lib_id \"Amp:LM358\")\n\t\t(at 100 {y} 0)\n\t\t(unit {n})\n\t\t(uuid \"unit-{n}\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(property \"Value\" \"LM358\"\n\t\t\t(at 102 82 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"multi\"\n\t\t\t\t(path \"/root-uuid\"\n\t\t\t\t\t(reference \"U1\")\n\t\t\t\t\t(unit {n})\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n")
        };
        std::fs::write(
            &path,
            format!(
                "(kicad_sch\n\t(version 20250610)\n\t(uuid \"root-uuid\")\n{}{})\n",
                unit(1, 80),
                unit(2, 100)
            ),
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_batch_edit(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{ "reference": "U1", "fields": { "Reference": "U9" } }]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"U1\""), "a unit was left behind:\n{text}");
        assert_eq!(
            text.matches(r#"(property "Reference" "U9""#).count(),
            2,
            "both units' properties must be renamed:\n{text}"
        );
        assert_eq!(
            text.matches(r#"(reference "U9")"#).count(),
            2,
            "both units' instance entries must be renamed:\n{text}"
        );
    }

    /// An edit that matched nothing must not rewrite the file: the old handler
    /// wrote unconditionally, so a batch of typos still bumped the file's mtime
    /// while reporting `updated_count: 0`.
    #[tokio::test]
    async fn batch_edit_that_matches_nothing_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let ctx = test_ctx();

        let result = handle_batch_edit(
            &json!({
                "schematic": path.display().to_string(),
                "edits": [{ "reference": "R99", "value": "1k" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op batch must not touch the file"
        );
    }

    /// Same contract for the sibling handlers: a batch that matched nothing
    /// reports its errors and leaves the file untouched.
    #[tokio::test]
    async fn bulk_move_that_matches_nothing_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let ctx = test_ctx();

        let result = handle_bulk_move(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["R99"],
                "dx": 2.54,
                "dy": 0.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op bulk move must not touch the file"
        );
    }

    #[tokio::test]
    async fn batch_delete_components_that_matches_nothing_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let ctx = test_ctx();

        let result = handle_batch_delete_components(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["R99"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op delete must not touch the file"
        );
    }

    #[tokio::test]
    async fn batch_connect_to_net_that_matches_nothing_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let ctx = test_ctx();

        let result = handle_batch_connect_to_net(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": "VCC",
                "pins": [{ "reference": "R99", "pin_number": "1" }]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op net connect must not touch the file"
        );
    }

    #[tokio::test]
    async fn batch_delete_that_matches_nothing_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let ctx = test_ctx();

        let result = handle_batch_delete(
            &json!({
                "schematic": path.display().to_string(),
                "references": ["R99"],
                "uuids": ["00000000-0000-0000-0000-000000000000"]
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a no-op delete must not touch the file"
        );
    }

    /// Deleting by UUID used to walk back to a literal `"\n  ("`, so it found
    /// nothing in the tab-indented files eeschema actually writes — the whole
    /// by-UUID branch failed on every real KiCAD schematic. Both indentation
    /// styles must work.
    #[tokio::test]
    async fn batch_delete_by_uuid_works_on_either_indentation() {
        for style in ["tabs", "spaces"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("d.kicad_sch");
            tab_indented_sch(&path);
            if style == "spaces" {
                let tabbed = std::fs::read_to_string(&path).unwrap();
                std::fs::write(&path, tabbed.replace('\t', "  ")).unwrap();
            }
            let ctx = test_ctx();

            let result = handle_batch_delete(
                &json!({
                    "schematic": path.display().to_string(),
                    "uuids": ["aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"]
                }),
                &ctx,
            )
            .await
            .unwrap();
            assert!(!result.is_error, "{style}: {:?}", result.content);

            let text = std::fs::read_to_string(&path).unwrap();
            read_schematic(&path)
                .unwrap_or_else(|e| panic!("{style}: output does not parse: {e}\n{text}"));
            // The whole symbol block went, not just the (uuid …) line.
            assert!(
                !text.contains(r#"(property "Reference" "R1""#),
                "{style}: symbol block survived:\n{text}"
            );
            assert!(
                !text.contains("aaaaaaaa-bbbb"),
                "{style}: uuid survived:\n{text}"
            );
            // Neighbouring top-level items must be untouched.
            assert!(
                text.contains("(lib_symbols"),
                "{style}: took out a sibling block:\n{text}"
            );
            assert!(
                text.contains(r#"(property "Reference" "R""#),
                "{style}: lib_symbols contents damaged:\n{text}"
            );
        }
    }

    /// `connect_passthrough` and `add_schematic_text` always insert exactly one
    /// block, so they have no empty-batch case — what the checked write buys
    /// them is that the splice cannot leave behind an unparseable file.
    #[tokio::test]
    async fn connect_passthrough_output_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let ctx = test_ctx();

        let result = handle_connect_passthrough(
            &json!({
                "schematic": path.display().to_string(),
                "net_name": "VCC",
                "x": 100.0, "y": 80.0,
                "direction": "right"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        read_schematic(&path).unwrap_or_else(|e| panic!("output does not parse: {e}\n{text}"));
        assert!(text.contains(r#""VCC""#), "{text}");
        // The pre-existing symbol must survive the splice.
        assert!(text.contains(r#"(property "Reference" "R1""#), "{text}");
    }

    #[tokio::test]
    async fn add_schematic_text_output_still_parses_with_quotes_in_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        tab_indented_sch(&path);
        let ctx = test_ctx();

        // Quotes and backslashes in user text are the way this splice would
        // most plausibly produce an unbalanced document.
        let result = handle_add_schematic_text(
            &json!({
                "schematic": path.display().to_string(),
                "text": r#"say "hi" \ (not a block)"#,
                "x": 50.0, "y": 50.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        read_schematic(&path).unwrap_or_else(|e| panic!("output does not parse: {e}\n{text}"));
        assert!(text.contains(r#"(property "Reference" "R1""#), "{text}");
    }
}

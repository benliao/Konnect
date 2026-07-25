//! `sch_components` toolset — add, edit, move, rotate, delete schematic symbols.
//!
//! Simple CRUD operations use `konnect_schematic_editor` (cse) for structured
//! round-trip parsing.  Pin coordinate math still delegates to
//! `konnect_sexp::geometry::transform_pin`.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    find_symbol_instance_block, get_path, opt_f64, opt_str, project_name_for, rename_symbol_edits,
    require_f64, require_str, RenameOutcome, ToolContext, ToolDef,
};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::snap_point,
    schematic::{
        extract_lib_pins_resolved, extract_symbol_instances, pin_endpoint, read_schematic,
    },
    writer::{apply_edits, new_uuid, write_atomic, write_atomic_checked, SexpEdit},
};
use serde_json::json;

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "create_schematic",
            "Create a new blank .kicad_sch schematic file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full path for the new .kicad_sch file" }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_create_schematic(args, ctx).await }
        ),
        tool!(
            "add_schematic_component",
            "Add a symbol from a KiCAD library to the schematic. The symbol is snapped \
             to the 1.27mm schematic grid. Specify position in schematic mm coordinates.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "lib_id": { "type": "string", "description": "Library:Symbol (e.g. 'Device:R')" },
                    "x": { "type": "number", "description": "X position in mm" },
                    "y": { "type": "number", "description": "Y position in mm" },
                    "rotation": { "type": "number", "description": "Rotation in degrees (0/90/180/270)", "default": 0 },
                    "reference": { "type": "string", "description": "Optional override for reference designator" },
                    "value": { "type": "string", "description": "Optional override for value field" },
                    "unit": { "type": "integer", "description": "Unit number for multi-unit symbols (gate/part selection). Default 1.", "default": 1 }
                },
                "required": ["schematic", "lib_id", "x", "y"]
            }),
            |args, ctx| async move { handle_add_schematic_component(args, ctx).await }
        ),
        tool!(
            "delete_schematic_component",
            "Remove a symbol instance from the schematic by its reference designator. \
             Reports any net labels left stranded on the removed component's pins (these \
             otherwise cause 'Label not connected' ERC errors) and prunes its lib_symbols \
             definition when nothing else uses it.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator (e.g. 'R1')" },
                    "cleanup_connections": {
                        "type": "boolean",
                        "description": "Also delete net labels left stranded on the removed component's pins. Default false — they are only reported.",
                        "default": false
                    }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_delete_schematic_component(args, ctx).await }
        ),
        tool!(
            "edit_schematic_component",
            "Update fields (Reference, Value, Footprint, custom properties) of a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string", "description": "Current reference designator" },
                    "new_reference": { "type": "string", "description": "New reference designator (optional)" },
                    "value": { "type": "string", "description": "New value (optional)" },
                    "footprint": { "type": "string", "description": "New footprint (optional)" },
                    "datasheet": { "type": "string", "description": "New datasheet URL (optional)" },
                    "fields": {
                        "type": "object",
                        "description": "Additional property fields to set as key:value pairs"
                    }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_edit_schematic_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_component",
            "Get all properties, position, and pin locations for a symbol instance.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_component(args, ctx).await }
        ),
        tool!(
            "list_schematic_components",
            "List all symbol instances in a schematic with their positions, values, \
             footprints, and pin locations.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_list_schematic_components(args, ctx).await }
        ),
        tool!(
            "move_schematic_component",
            "Move a symbol to a new position. Does NOT adjust connected wires.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number", "description": "New X position in mm" },
                    "y": { "type": "number", "description": "New Y position in mm" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_schematic_component(args, ctx).await }
        ),
        tool!(
            "rotate_schematic_component",
            "Rotate a symbol by setting its absolute rotation angle (0/90/180/270).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "rotation": { "type": "number", "description": "Absolute rotation in degrees" }
                },
                "required": ["schematic", "reference", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_schematic_component(args, ctx).await }
        ),
        tool!(
            "move_connected",
            "Move a symbol and stretch/shrink connected wire stubs to preserve connections.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "x": { "type": "number" },
                    "y": { "type": "number" }
                },
                "required": ["schematic", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_move_connected(args, ctx).await }
        ),
        tool!(
            "move_region",
            "Move all symbols within a bounding box by a given offset.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x1": { "type": "number", "description": "Region bounding box min X" },
                    "y1": { "type": "number", "description": "Region bounding box min Y" },
                    "x2": { "type": "number", "description": "Region bounding box max X" },
                    "y2": { "type": "number", "description": "Region bounding box max Y" },
                    "dx": { "type": "number", "description": "X offset to move by" },
                    "dy": { "type": "number", "description": "Y offset to move by" }
                },
                "required": ["schematic", "x1", "y1", "x2", "y2", "dx", "dy"]
            }),
            |args, ctx| async move { handle_move_region(args, ctx).await }
        ),
        tool!(
            "annotate_schematic",
            "Run kicad-cli to auto-assign reference designators (R? → R1, U? → U1, etc.).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_annotate_schematic(args, ctx).await }
        ),
        tool!(
            "get_schematic_pin_locations",
            "Get the exact schematic-space (X,Y) coordinates of every pin on a symbol, \
             accounting for rotation and mirroring. Uses the canonical pin transform.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_schematic_pin_locations(args, ctx).await }
        ),
        tool!(
            "batch_get_schematic_pin_locations",
            "Get pin locations for multiple components in a single file read.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators"
                    }
                },
                "required": ["schematic", "references"]
            }),
            |args, ctx| async move { handle_batch_get_pin_locations(args, ctx).await }
        ),
        tool!(
            "add_component_annotation",
            "Add a custom property (annotation) to a symbol instance in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" },
                    "key": { "type": "string", "description": "Property name" },
                    "value": { "type": "string", "description": "Property value" }
                },
                "required": ["schematic", "reference", "key", "value"]
            }),
            |args, ctx| async move { handle_add_component_annotation(args, ctx).await }
        ),
        tool!(
            "group_components",
            "Add a group property to multiple components in the schematic.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "references": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of reference designators to group"
                    },
                    "group_name": { "type": "string", "description": "Group name to assign" }
                },
                "required": ["schematic", "references", "group_name"]
            }),
            |args, ctx| async move { handle_group_components(args, ctx).await }
        ),
        tool!(
            "replace_component",
            "Replace a component's lib_id with a new library symbol (swap the component type).",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'U1')" },
                    "new_lib_id": { "type": "string", "description": "New Library:Symbol identifier (e.g. 'Device:C')" }
                },
                "required": ["schematic", "reference", "new_lib_id"]
            }),
            |args, ctx| async move { handle_replace_component(args, ctx).await }
        ),
        tool!(
            "get_schematic_view",
            "Render the schematic to a PNG image (base64-encoded) via kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_get_schematic_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_create_schematic(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = get_path(args, "path")?;
    // Build a minimal valid schematic and save via cse's atomic writer.
    let template = crate::tools::blank_schematic_template();
    // Write the template then immediately load/save through cse so the file
    // is normalised to cse's writer output format.
    write_atomic(&path, &template)?;
    let sch = cse::Schematic::load(&path)?;
    sch.overwrite()?;
    Ok(CallToolResult::json(
        &json!({ "created": path.display().to_string() }),
    ))
}

async fn handle_add_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let lib_id = match require_str(args, "lib_id") {
        Ok(s) => s.to_string(),
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
    let rotation = opt_f64(args, "rotation").unwrap_or(0.0);
    let reference = opt_str(args, "reference");
    let value = opt_str(args, "value");
    let unit = opt_f64(args, "unit").unwrap_or(1.0) as u32;

    // Snap to 1.27mm grid
    let (x, y) = snap_point(x, y, 1.27);

    let ref_str = reference.unwrap_or("?");
    let val_str = value.unwrap_or(lib_id.split(':').next_back().unwrap_or("?"));

    // Load via konnect-schematic-editor
    let mut sch = cse::Schematic::load(&sch_path)?;

    // The instance path below must be "/<root-uuid>" — KiCAD's netlister
    // resolves instances against the root sheet UUID and silently forms no
    // wire-only nets for symbols whose path doesn't resolve.
    let root_uuid = crate::tools::ensure_root_uuid(&mut sch);
    let project_name = project_name_for(&sch_path);

    // Embed the library symbol definition
    if !cse::library::ensure_lib_symbol(&mut sch, &lib_id) {
        return Ok(crate::tools::lib_symbol_not_found_error(&lib_id));
    }

    // Build the Symbol struct
    let mut sym = cse::Symbol::new(&lib_id, x, y);
    sym.at.rotation = Some(rotation);
    sym.unit = unit;

    // Helper: build an effects sub-node  (font (size 1.27 1.27))  with optional (hide yes)
    let effects_node = |hide: bool| -> cse::sexp::SexpNode {
        let font = cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("font"),
            cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("size"),
                cse::sexp::atom("1.27"),
                cse::sexp::atom("1.27"),
            ]),
        ]);
        let mut children = vec![cse::sexp::atom("effects"), font];
        if hide {
            children.push(cse::sexp::SexpNode::List(vec![
                cse::sexp::atom("hide"),
                cse::sexp::atom("yes"),
            ]));
        }
        cse::sexp::SexpNode::List(children)
    };

    // Helper: build an (at X Y ROT) sub-node
    let at_node = |px: f64, py: f64, rot: f64| -> cse::sexp::SexpNode {
        cse::sexp::SexpNode::List(vec![
            cse::sexp::atom("at"),
            cse::sexp::atom(cse::types::fmt_f64(px)),
            cse::sexp::atom(cse::types::fmt_f64(py)),
            cse::sexp::atom(cse::types::fmt_f64(rot)),
        ])
    };

    // Offset Reference above component, Value below
    let ref_y = y - 3.81;
    let val_y = y + 3.81;

    // Reference property
    let mut ref_prop = cse::Property::new("Reference", ref_str);
    ref_prop.sub_nodes.push(at_node(x, ref_y, 0.0));
    ref_prop.sub_nodes.push(effects_node(false));
    sym.properties.push(ref_prop);

    // Value property
    let mut val_prop = cse::Property::new("Value", val_str);
    val_prop.sub_nodes.push(at_node(x, val_y, 0.0));
    val_prop.sub_nodes.push(effects_node(false));
    sym.properties.push(val_prop);

    // Footprint property (hidden)
    let mut fp_prop = cse::Property::new("Footprint", "");
    fp_prop.sub_nodes.push(at_node(x, y, 0.0));
    fp_prop.sub_nodes.push(effects_node(true));
    sym.properties.push(fp_prop);

    // Datasheet property (hidden)
    let mut ds_prop = cse::Property::new("Datasheet", "");
    ds_prop.sub_nodes.push(at_node(x, y, 0.0));
    ds_prop.sub_nodes.push(effects_node(true));
    sym.properties.push(ds_prop);

    // Instance entry, keyed to the root sheet UUID like eeschema writes it:
    // (instances (project "<name>" (path "/<root-uuid>" (reference ...) (unit 1))))
    sym.set_instance_path(&project_name, &format!("/{}", root_uuid), ref_str, unit);

    let uuid = sym.uuid.clone();
    sch.add_symbol(sym);
    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "added": lib_id,
        "reference": ref_str,
        "value": val_str,
        "x": x, "y": y,
        "unit": unit,
        "uuid": uuid
    })))
}

async fn handle_delete_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let cleanup = args["cleanup_connections"].as_bool().unwrap_or(false);

    // Pin coordinates of the component about to go, so labels sitting on them
    // can be reported (or removed). batch_connect_to_net drops a net label on
    // each pin; deleting only the symbol strands them and ERC then reports
    // "Label not connected" for every one (S4-1).
    let doomed_pins = pin_world_positions(&sch_path, &reference).unwrap_or_default();

    let mut sch = cse::Schematic::load(&sch_path)?;

    let Some(removed) = sch.symbols.remove_by_reference(&reference) else {
        return Ok(CallToolResult::error(format!(
            "Component '{}' not found in schematic",
            reference
        )));
    };

    // A label is orphaned only if no *surviving* pin still sits on it.
    let surviving: Vec<(f64, f64)> = sch
        .symbols
        .iter()
        .filter_map(|s| s.reference())
        .flat_map(|r| pin_world_positions(&sch_path, r).unwrap_or_default())
        .collect();
    let is_stranded = |x: f64, y: f64| {
        doomed_pins
            .iter()
            .any(|&(px, py)| near(px, x) && near(py, y))
            && !surviving.iter().any(|&(px, py)| near(px, x) && near(py, y))
    };

    let mut orphaned = Vec::new();
    for l in sch.labels.iter() {
        let (x, y) = l.position();
        if is_stranded(x, y) {
            orphaned.push(json!({ "net": l.text, "x": x, "y": y, "kind": "label" }));
        }
    }
    for l in sch.global_labels.iter() {
        let (x, y) = l.position();
        if is_stranded(x, y) {
            orphaned.push(json!({ "net": l.text, "x": x, "y": y, "kind": "global_label" }));
        }
    }

    if cleanup {
        sch.labels.retain(|l| {
            let (x, y) = l.position();
            !is_stranded(x, y)
        });
        sch.global_labels.retain(|l| {
            let (x, y) = l.position();
            !is_stranded(x, y)
        });
    }

    // Drop the lib_symbols definition if nothing references it any more —
    // KiCAD prunes unused definitions on save, and leaving them made the
    // section grow without bound across edits.
    let pruned = prune_unused_lib_symbol(&mut sch, &removed.lib_id.clone());

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "deleted": reference,
        "orphaned_labels": orphaned,
        "orphaned_labels_removed": cleanup,
        "pruned_lib_symbol": pruned,
    })))
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

/// World-space pin coordinates of `reference` in the schematic at `path`.
fn pin_world_positions(path: &std::path::Path, reference: &str) -> Option<Vec<(f64, f64)>> {
    let (_, tree) = read_schematic(path).ok()?;
    let instances = extract_symbol_instances(&tree);
    let inst = instances.iter().find(|i| i.reference == reference)?;
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let sym = lib_syms
        .iter()
        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id))?;
    let t = inst.pin_transform();
    Some(
        extract_lib_pins_resolved(sym, &lib_syms)
            .iter()
            .map(|p| pin_endpoint(p, t))
            .collect(),
    )
}

/// Remove `lib_id`'s definition from `lib_symbols` when no symbol instance
/// uses it any more. Returns whether anything was removed.
fn prune_unused_lib_symbol(sch: &mut cse::Schematic, lib_id: &str) -> bool {
    if lib_id.is_empty() {
        return false;
    }
    if sch.symbols.iter().any(|s| s.lib_id == lib_id) {
        return false;
    }
    // Keep a definition that a surviving symbol still extends.
    let mut removed = false;
    for node in sch.raw_other.iter_mut() {
        if node.tag() != Some("lib_symbols") {
            continue;
        }
        if let cse::sexp::SexpNode::List(children) = node {
            let before = children.len();
            children.retain(|c| {
                c.tag() != Some("symbol")
                    || c.children().get(1).and_then(|n| n.text()) != Some(lib_id)
            });
            removed = children.len() != before;
        }
    }
    removed
}

/// Rewrite the value of `(property "<field>" "<value>" …)` inside the placed
/// symbol block whose Reference is `ref_`. Returns the reason on failure so the
/// caller can report it instead of silently claiming success.
///
/// Edits the raw text rather than going through `cse::Schematic`: that model
/// keeps only `pin` and `instances` sub-nodes verbatim and re-emits everything
/// else from typed fields, so a load→mutate→save of an eeschema-written symbol
/// drops nodes it does not model (`exclude_from_sim`, `lib_name`, …). Editing a
/// user's file must not silently delete parts of it.
fn update_symbol_field(
    content: &str,
    ref_: &str,
    field: &str,
    new_val: &str,
) -> Result<String, String> {
    let (sym_start, sym_end) = find_symbol_instance_block(content, ref_)
        .ok_or_else(|| format!("symbol '{ref_}' not found in this schematic"))?;
    let sym_block = &content[sym_start..sym_end];
    let field_search = format!(r#"(property "{field}" ""#);
    let field_offset = sym_block
        .find(&field_search)
        .map(|o| sym_start + o + field_search.len())
        .ok_or_else(|| format!("'{ref_}' has no '{field}' property"))?;
    // Find the closing quote of the current value
    let val_end = content[field_offset..]
        .find('"')
        .map(|o| field_offset + o)
        .ok_or_else(|| format!("'{field}' property on '{ref_}' is malformed"))?;
    Ok(format!(
        "{}{}{}",
        &content[..field_offset],
        new_val,
        &content[val_end..]
    ))
}

/// Rename a component in both places KiCAD 6+ stores a designator, returning
/// the rewritten content. The edit computation lives in
/// [`rename_symbol_edits`], shared with the batch handler.
fn rename_symbol(content: &str, old: &str, new: &str) -> Result<(String, RenameOutcome), String> {
    let (edits, outcome) = rename_symbol_edits(content, old, new)?;
    Ok((apply_edits(content.to_string(), edits), outcome))
}

async fn handle_edit_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = std::fs::read_to_string(&sch_path)?;
    let mut changed = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // Tracks the designator the symbol is currently findable by: after a
    // rename, later fields must be looked up under the *new* name.
    let mut lookup_ref = reference.clone();

    macro_rules! apply {
        ($field:expr, $new_val:expr) => {
            match update_symbol_field(&content, &lookup_ref, $field, $new_val) {
                Ok(updated) => {
                    content = updated;
                    changed.push(format!("{} → {}", $field, $new_val));
                    true
                }
                Err(why) => {
                    errors.push(format!("{}: {}", $field, why));
                    false
                }
            }
        };
    }

    if let Some(new_ref) = opt_str(args, "new_reference") {
        match rename_symbol(&content, &reference, new_ref) {
            Ok((updated, outcome)) => {
                content = updated;
                // The property now reads `new_ref`, so every later field edit
                // must look the symbol up under the new name.
                lookup_ref = new_ref.to_string();
                changed.push(format!("Reference → {}", new_ref));
                if outcome.instances > 0 {
                    changed.push(format!(
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
            Err(why) => errors.push(format!("Reference: {why}")),
        }
    }
    if let Some(val) = opt_str(args, "value") {
        apply!("Value", val);
    }
    if let Some(fp) = opt_str(args, "footprint") {
        apply!("Footprint", fp);
    }
    if let Some(ds) = opt_str(args, "datasheet") {
        apply!("Datasheet", ds);
    }

    // A request that changed nothing is a failure, not a success — silently
    // reporting `"changes": []` is what let the tab-indentation bug hide.
    if changed.is_empty() && !errors.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No fields were updated on '{}': {}",
            reference,
            errors.join("; ")
        )));
    }

    if !changed.is_empty() {
        // Checked: these are raw string splices, so a mis-computed offset must
        // fail the call rather than replace the user's file with one KiCAD
        // cannot open.
        write_atomic_checked(&sch_path, &content, "kicad_sch")?;
    }

    let mut result = json!({
        "reference": reference,
        "changes": changed
    });
    if !errors.is_empty() {
        result["errors"] = json!(errors);
    }
    Ok(CallToolResult::json(&result))
}

async fn handle_get_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference(&reference) {
        Some(sym) => {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            Ok(CallToolResult::json(&json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y'),
                "uuid": sym.uuid
            })))
        }
        None => Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        ))),
    }
}

async fn handle_list_schematic_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;

    let items: Vec<serde_json::Value> = sch
        .symbols
        .iter()
        .map(|sym| {
            let (x, y) = sym.position();
            let rotation = sym.at.rotation.unwrap_or(0.0);
            let mirror = sym.mirror.as_deref().unwrap_or("");
            json!({
                "reference": sym.reference().unwrap_or("?"),
                "value": sym.value_str().unwrap_or(""),
                "footprint": sym.footprint().unwrap_or(""),
                "lib_id": sym.lib_id,
                "x": x,
                "y": y,
                "rotation": rotation,
                "mirror_x": mirror.contains('x'),
                "mirror_y": mirror.contains('y')
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "components": items
    })))
}

async fn handle_move_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let new_y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (new_x, new_y) = snap_point(new_x, new_y, 1.27);

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.move_to(new_x, new_y);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "moved": reference, "x": new_x, "y": new_y }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_rotate_schematic_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    match sch.symbols.by_reference_mut(&reference) {
        Some(sym) => {
            sym.set_rotation(rotation);
            sch.overwrite()?;
            Ok(CallToolResult::json(
                &json!({ "rotated": reference, "rotation": rotation }),
            ))
        }
        None => Err(anyhow::anyhow!("Component '{}' not found", reference)),
    }
}

async fn handle_move_connected(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    // For now: delegate to simple move. Wire adjustment is a Phase 2 enhancement.
    handle_move_schematic_component(args, ctx).await
}

async fn handle_move_region(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
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
    let dx = match require_f64(args, "dx") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let dy = match require_f64(args, "dy") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let mut sch = cse::Schematic::load(&sch_path)?;

    // Collect references of symbols within the bounding box
    let refs_to_move: Vec<String> = sch
        .symbols
        .within_rectangle(x1, y1, x2, y2)
        .iter()
        .filter_map(|s| s.reference().map(String::from))
        .collect();

    let mut moved = Vec::new();
    for reference in &refs_to_move {
        if let Some(sym) = sch.symbols.by_reference_mut(reference) {
            let (ox, oy) = sym.position();
            let (nx, ny) = snap_point(ox + dx, oy + dy, 1.27);
            sym.move_to(nx, ny);
            moved.push(reference.clone());
        }
    }

    sch.overwrite()?;

    Ok(CallToolResult::json(&json!({
        "moved_count": moved.len(),
        "moved": moved
    })))
}

async fn handle_annotate_schematic(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    crate::tools::cli::annotate_schematic(&ctx.config.kicad_cli, &sch_path).await?;
    Ok(CallToolResult::text("Annotation complete."))
}

async fn handle_get_schematic_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let inst = match instances.iter().find(|i| i.reference == reference) {
        Some(i) => i,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the library symbol definition within the schematic's lib_symbols section
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let lib_sym = lib_syms
        .iter()
        .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));

    // A missing embedded definition is an error, not an empty pin list —
    // silently returning [] hid every bad-lib_id component until wiring or
    // netlisting failed much later (#34).
    let Some(sym) = lib_sym else {
        return Ok(CallToolResult::error(format!(
            "Component '{}' has no embedded definition for '{}' in this \
             schematic's lib_symbols — it was likely added with a lib_id that \
             doesn't exist in the installed libraries, so it is invisible to \
             KiCAD's netlister. Re-add it with a valid lib_id \
             (delete_schematic_component + add_schematic_component).",
            reference, inst.lib_id
        )));
    };
    // `_resolved` follows `(extends "Parent")`: standard KiCAD parts like
    // Transistor_FET:2N7002 carry no pins of their own and inherit them all
    // from the parent that `ensure_lib_symbol` embedded alongside them.
    // Scanning only the symbol's own units returned `{"pins": []}` for every
    // derived part (S3-2).
    let lib_pins = extract_lib_pins_resolved(sym, &lib_syms);
    let t = inst.pin_transform();
    let pins: Vec<serde_json::Value> = lib_pins
        .iter()
        .map(|p| {
            let (sx, sy) = pin_endpoint(p, t);
            json!({
                "number": p.number,
                "name": p.name,
                "x": sx,
                "y": sy
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "component_x": inst.x,
        "component_y": inst.y,
        "rotation": inst.rotation,
        "pins": pins
    })))
}

async fn handle_batch_get_pin_locations(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let (_, tree) = read_schematic(&sch_path)?; // single read
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let results: Vec<serde_json::Value> = refs
        .iter()
        .map(|reference| {
            let inst = match instances.iter().find(|i| &i.reference == reference) {
                Some(i) => i,
                None => return json!({ "reference": reference, "error": "not found" }),
            };
            let lib_sym = lib_syms
                .iter()
                .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
            // Per-entry error rather than a silent empty pin list (#34).
            let Some(sym) = lib_sym else {
                return json!({
                    "reference": reference,
                    "error": format!(
                        "no embedded definition for '{}' in lib_symbols — \
                         likely added with a nonexistent lib_id",
                        inst.lib_id
                    )
                });
            };
            let t = inst.pin_transform();
            // Resolve through `(extends …)` — see handle_get_schematic_pin_locations.
            let pins: Vec<serde_json::Value> = extract_lib_pins_resolved(sym, &lib_syms)
                .iter()
                .map(|p| {
                    let (sx, sy) = pin_endpoint(p, t);
                    json!({ "number": p.number, "name": p.name, "x": sx, "y": sy })
                })
                .collect();
            json!({ "reference": reference, "x": inst.x, "y": inst.y, "pins": pins })
        })
        .collect();

    Ok(CallToolResult::json(&json!({ "components": results })))
}

async fn handle_get_schematic_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tmp_dir = std::env::temp_dir().join(format!("konnect_{}", new_uuid()));
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // KiCAD 10 CLI only supports SVG export for schematics (no bitmap)
    let svg_path =
        crate::tools::cli::render_schematic_svg(&ctx.config.kicad_cli, &sch_path, &tmp_dir).await?;

    let svg_content = tokio::fs::read_to_string(&svg_path).await?;
    tokio::fs::remove_dir_all(&tmp_dir).await.ok();

    // Return as text content (SVG is XML text, not a raster image)
    Ok(crate::mcp::protocol::CallToolResult {
        content: vec![crate::mcp::protocol::ToolContent::Text {
            text: format!("SVG schematic rendered. {} bytes.\n\nNote: KiCAD 10 CLI exports schematics as SVG only (no bitmap). \
                          The SVG file has been generated. Use export_schematic_pdf for a PDF version.", svg_content.len()),
        }],
        is_error: false,
    })
}

async fn handle_add_component_annotation(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let key = match require_str(args, "key") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let value = match require_str(args, "value") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&sch_path)?;

    // Find the symbol block for this reference
    let (sym_start, sym_end) = match find_symbol_instance_block(&content, &reference) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the position just before (instances in the symbol block, or before closing paren
    let sym_block = &content[sym_start..sym_end];
    let insert_rel = sym_block
        .find("(instances")
        .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
    let insert_abs = sym_start + insert_rel;

    // Build the property S-expression
    let prop_sexp = format!(
        "    (property \"{key}\" \"{value}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
    );

    let new_content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
    write_atomic(&sch_path, &new_content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "added_property": key,
        "value": value
    })))
}

async fn handle_group_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let group_name = match require_str(args, "group_name") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let refs = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if refs.is_empty() {
        return Ok(CallToolResult::error("No references provided"));
    }

    let mut content = std::fs::read_to_string(&sch_path)?;
    let mut grouped = Vec::new();

    for reference in &refs {
        let (sym_start, sym_end) = match find_symbol_instance_block(&content, reference) {
            Some(r) => r,
            None => continue,
        };

        let sym_block = &content[sym_start..sym_end];
        let insert_rel = sym_block
            .find("(instances")
            .unwrap_or(sym_block.rfind(')').unwrap_or(sym_block.len() - 1));
        let insert_abs = sym_start + insert_rel;

        let prop_sexp = format!(
            "    (property \"Group\" \"{group_name}\"\n      (at 0 0 0)\n      (effects (font (size 1.27 1.27)) (hide yes))\n    )\n    "
        );

        content = apply_edits(content, vec![SexpEdit::insert(insert_abs, prop_sexp)]);
        grouped.push(reference.clone());
    }

    write_atomic(&sch_path, &content)?;

    Ok(CallToolResult::json(&json!({
        "group_name": group_name,
        "grouped_count": grouped.len(),
        "grouped": grouped
    })))
}

async fn handle_replace_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(r) => r.to_string(),
        Err(e) => return Ok(e),
    };
    let new_lib_id = match require_str(args, "new_lib_id") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let mut content = std::fs::read_to_string(&sch_path)?;

    // Find the symbol block for this reference
    let (sym_start, sym_end) = match find_symbol_instance_block(&content, &reference) {
        Some(r) => r,
        None => {
            return Ok(CallToolResult::error(format!(
                "Component '{}' not found",
                reference
            )))
        }
    };

    // Find the (lib_id "OLD") and replace it — searching only within this
    // symbol's block, so a malformed instance can't reach into the next one.
    let sym_block = &content[sym_start..sym_end];
    let lib_id_pat = "(lib_id \"";
    let lib_id_rel = match sym_block.find(lib_id_pat) {
        Some(o) => o,
        None => {
            return Ok(CallToolResult::error(
                "Could not find lib_id in symbol block",
            ))
        }
    };
    let lib_id_abs = sym_start + lib_id_rel + lib_id_pat.len();
    let lib_id_end = match content[lib_id_abs..].find('"') {
        Some(o) => lib_id_abs + o,
        None => return Ok(CallToolResult::error("Malformed lib_id")),
    };

    let old_lib_id = content[lib_id_abs..lib_id_end].to_string();

    let new_content = apply_edits(
        content,
        vec![SexpEdit::replace(
            lib_id_abs,
            lib_id_end,
            new_lib_id.clone(),
        )],
    );
    content = new_content;

    // Ensure the new library symbol definition is present. Bail BEFORE writing:
    // a replace that can't embed its definition would leave the component
    // netlist-invisible (#34).
    if !super::ensure_lib_symbol_in_schematic(&mut content, &new_lib_id) {
        return Ok(crate::tools::lib_symbol_not_found_error(&new_lib_id));
    }
    write_atomic(&sch_path, &content)?;

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "old_lib_id": old_lib_id,
        "new_lib_id": new_lib_id
    })))
}

// Library symbol resolution moved to tools/mod.rs (shared with sch_wiring.rs)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
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

    /// Serializes tests that set KICAD10_SYMBOL_DIR (process-wide env).
    static SYMBOL_DIR_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A stub symbol library so component adds resolve without an installed
    /// KiCAD (CI has none): Device:R and Device:C_Polarized in the KiCAD 10
    /// symdir layout. Returns (tempdir guard, env lock).
    fn stub_symbol_dir() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = SYMBOL_DIR_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let symdir = dir.path().join("Device.kicad_symdir");
        std::fs::create_dir_all(&symdir).unwrap();
        let symbol = |name: &str| {
            format!(
                "(kicad_symbol_lib\n\t(version 20241209)\n\t(generator \"test\")\n\t(symbol \"{name}\"\n\t\t(property \"Reference\" \"R\" (at 0 0 0))\n\t\t(property \"Value\" \"{name}\" (at 0 0 0))\n\t\t(symbol \"{name}_0_1\"\n\t\t\t(pin passive line (at 0 3.81 270) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"1\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t\t(pin passive line (at 0 -3.81 90) (length 1.27)\n\t\t\t\t(name \"~\" (effects (font (size 1.27 1.27))))\n\t\t\t\t(number \"2\" (effects (font (size 1.27 1.27))))\n\t\t\t)\n\t\t)\n\t)\n)\n"
            )
        };
        std::fs::write(symdir.join("R.kicad_sym"), symbol("R")).unwrap();
        std::fs::write(symdir.join("C_Polarized.kicad_sym"), symbol("C_Polarized")).unwrap();
        std::env::set_var("KICAD10_SYMBOL_DIR", dir.path());
        (dir, guard)
    }

    #[tokio::test]
    async fn create_schematic_writes_root_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.kicad_sch");
        let ctx = test_ctx();

        let result = handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        assert!(
            sch.uuid.is_some(),
            "root (uuid ...) is required for KiCAD's netlister to resolve instance paths"
        );
    }

    /// Deleting a component used to strand the net labels that
    /// batch_connect_to_net drops on its pins, and ERC then reported
    /// "Label not connected" for each (S4-1). The labels must at minimum be
    /// reported, and removed on request.
    #[tokio::test]
    async fn delete_reports_and_can_clean_up_stranded_labels() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.kicad_sch");
        let ctx = test_ctx();
        let p = path.display().to_string();

        handle_create_schematic(&json!({ "path": p }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({ "schematic": p, "lib_id": "Device:R", "x": 100.0, "y": 80.0,
                     "reference": "R1" }),
            &ctx,
        )
        .await
        .unwrap();

        // A label sitting exactly on R1's pin 1, plus one that is unrelated.
        let pins = pin_world_positions(&path, "R1").expect("R1 has pins");
        let (px, py) = pins[0];
        let mut sch = cse::Schematic::load(&path).unwrap();
        sch.add_label("+3V3", px, py);
        sch.add_label("ELSEWHERE", px + 50.0, py + 50.0);
        sch.overwrite().unwrap();

        let result =
            handle_delete_schematic_component(&json!({ "schematic": p, "reference": "R1" }), &ctx)
                .await
                .unwrap();
        assert!(!result.is_error);
        let raw = match &result.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        };
        let body: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let orphans = body["orphaned_labels"].as_array().unwrap();
        assert_eq!(
            orphans.len(),
            1,
            "expected exactly one stranded label: {body}"
        );
        assert_eq!(orphans[0]["net"], "+3V3");
        // Default is non-destructive: reported, still on disk.
        assert_eq!(body["orphaned_labels_removed"], json!(false));
        let sch = cse::Schematic::load(&path).unwrap();
        assert_eq!(sch.labels.iter().count(), 2, "nothing removed by default");
        // The definition is gone now that nothing uses it.
        assert_eq!(body["pruned_lib_symbol"], json!(true));

        // With cleanup requested, only the stranded one goes.
        handle_add_schematic_component(
            &json!({ "schematic": p, "lib_id": "Device:R", "x": 100.0, "y": 80.0,
                     "reference": "R1" }),
            &ctx,
        )
        .await
        .unwrap();
        handle_delete_schematic_component(
            &json!({ "schematic": p, "reference": "R1", "cleanup_connections": true }),
            &ctx,
        )
        .await
        .unwrap();
        let sch = cse::Schematic::load(&path).unwrap();
        let names: Vec<&str> = sch.labels.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(names, vec!["ELSEWHERE"], "only the stranded label removed");
    }

    #[tokio::test]
    async fn add_component_writes_eeschema_style_instance_path() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("amp.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("root uuid present");
        let sym = sch.symbols.by_reference("R1").unwrap();
        // KiCAD only forms wire-only nets when the instance path is exactly
        // "/<root-uuid>"; the project key mirrors eeschema (file stem).
        assert!(
            sym.has_instance_path("amp", &format!("/{}", root_uuid)),
            "instance path must be /<root-uuid> under the file-stem project name"
        );
    }

    #[tokio::test]
    async fn add_component_writes_requested_unit() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "U1",
                "unit": 3
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let sym = sch.symbols.by_reference("U1").unwrap();
        assert_eq!(sym.unit, 3, "symbol (unit N) must match the requested unit");
        let root_uuid = sch.uuid.clone().unwrap();
        // Instance entry must carry the same unit, not a hardcoded 1.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&format!("/{}", root_uuid)));
        assert!(raw.contains("(unit 3)"), "instance unit must be 3");
    }

    #[tokio::test]
    async fn add_component_repairs_legacy_file_without_root_uuid() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.kicad_sch");
        // File shape produced by Konnect before root UUIDs were written.
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 50.0, "y": 50.0,
                "reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error);

        let sch = cse::Schematic::load(&path).unwrap();
        let root_uuid = sch.uuid.clone().expect("legacy file gains a root uuid");
        let sym = sch.symbols.by_reference("R1").unwrap();
        assert!(sym.has_instance_path("legacy", &format!("/{}", root_uuid)));
    }

    #[tokio::test]
    async fn add_component_with_nonexistent_lib_id_errors_with_suggestion() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Device:CP is the KiCAD ≤9 name; 10 renamed it to C_Polarized (#34).
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:CP",
                "x": 100.0, "y": 80.0,
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error, "nonexistent lib_id must be an error");
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"), "names the bad lib_id: {msg}");
        assert!(
            msg.contains("C_Polarized"),
            "did-you-mean should surface the rename: {msg}"
        );

        // And nothing was written: no ghost instance in the file.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn add_component_with_unknown_library_says_so() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nolib.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let result = handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Transistor_FET_xyzzy:IRF830",
                "x": 100.0, "y": 80.0
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        let msg = format!("{:?}", result.content);
        assert!(
            msg.contains("Library 'Transistor_FET_xyzzy' not found"),
            "distinguishes missing library from missing symbol: {msg}"
        );
    }

    /// The reference designator lives in two places; a rename must move both.
    /// Only the property is drawn in eeschema, but "Update PCB from Schematic"
    /// reads the instances entry — updating one and not the other is what made
    /// PCB sync keep failing on the old designator.
    #[tokio::test]
    async fn rename_updates_property_and_instances_block() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        handle_add_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "lib_id": "Device:R",
                "x": 100.0, "y": 80.0,
                "reference": "FLG1"
            }),
            &ctx,
        )
        .await
        .unwrap();

        let result = handle_edit_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "FLG1",
                "new_reference": "#FLG01"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "rename failed: {:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r##"(property "Reference" "#FLG01""##),
            "Reference property not renamed:\n{text}"
        );
        assert!(
            text.contains(r##"(reference "#FLG01")"##),
            "instances (reference …) not renamed — PCB sync reads this one:\n{text}"
        );
        assert!(
            !text.contains("\"FLG1\""),
            "old designator still present somewhere:\n{text}"
        );

        // And the typed model agrees, i.e. the file still parses.
        let sch = cse::Schematic::load(&path).unwrap();
        assert!(sch.symbols.by_reference("#FLG01").is_some());
    }

    /// eeschema/KiCAD 10 write tabs; this crate's writer writes two spaces.
    /// Neither indentation may be assumed by the rename matchers.
    #[tokio::test]
    async fn rename_works_on_tab_indented_eeschema_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\"\n\t\t\t\t(at 0 0 0)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(exclude_from_sim no)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 102 82 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"tabs\"\n\t\t\t\t(path \"/11111111-2222-3333-4444-555555555555\"\n\t\t\t\t\t(reference \"R1\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        // Rename and change another field in the same call: the later field
        // must be looked up under the *new* designator.
        let result = handle_edit_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "R1",
                "new_reference": "R42",
                "value": "22k"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "rename failed: {:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"(property "Reference" "R42""#), "{text}");
        assert!(text.contains(r#"(reference "R42")"#), "{text}");
        assert!(text.contains(r#"(property "Value" "22k""#), "{text}");
        assert!(!text.contains("\"R1\""), "old designator remains:\n{text}");
        // The lib_symbols definition's own Reference must be untouched.
        assert!(text.contains(r#"(property "Reference" "R""#), "{text}");
        // Raw-text editing must not drop nodes the typed model doesn't know.
        assert!(
            text.contains("(exclude_from_sim no)"),
            "editing dropped an unmodelled node:\n{text}"
        );
    }

    /// Multi-unit parts repeat the designator across one `(symbol …)` block per
    /// unit. Renaming only the first leaves the file half-renamed.
    #[tokio::test]
    async fn rename_covers_every_unit_of_a_multi_unit_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.kicad_sch");
        let unit = |n: u32, y: u32| {
            format!("\t(symbol\n\t\t(lib_id \"Amp:LM358\")\n\t\t(at 100 {y} 0)\n\t\t(unit {n})\n\t\t(uuid \"unit-{n}\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(instances\n\t\t\t(project \"multi\"\n\t\t\t\t(path \"/root-uuid\"\n\t\t\t\t\t(reference \"U1\")\n\t\t\t\t\t(unit {n})\n\t\t\t\t)\n\t\t\t)\n\t\t)\n\t)\n")
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

        let result = handle_edit_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "U1",
                "new_reference": "U7"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"U1\""), "a unit was left behind:\n{text}");
        assert_eq!(
            text.matches(r#"(property "Reference" "U7""#).count(),
            2,
            "both units' properties must be renamed:\n{text}"
        );
        assert_eq!(
            text.matches(r#"(reference "U7")"#).count(),
            2,
            "both units' instance entries must be renamed:\n{text}"
        );
    }

    #[tokio::test]
    async fn renaming_an_unknown_reference_is_an_error() {
        let (_symdir, _env) = stub_symbol_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.kicad_sch");
        let ctx = test_ctx();

        handle_create_schematic(&json!({ "path": path.display().to_string() }), &ctx)
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let result = handle_edit_schematic_component(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "R99",
                "new_reference": "R1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn pin_locations_error_when_definition_not_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noembed.kicad_sch");
        // A symbol instance whose lib_id has NO lib_symbols entry — the file
        // shape a ghost lib_id used to leave behind (#34).
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\t(lib_symbols\n\t)\n\t(symbol\n\t\t(lib_id \"Device:CP\")\n\t\t(at 100 80 0)\n\t\t(unit 1)\n\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\t\t(property \"Reference\" \"C1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t)\n)\n",
        )
        .unwrap();
        let ctx = test_ctx();

        let result = handle_get_schematic_pin_locations(
            &json!({
                "schematic": path.display().to_string(),
                "reference": "C1"
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(
            result.is_error,
            "missing embedded definition must be an error, not pins: []"
        );
        let msg = format!("{:?}", result.content);
        assert!(msg.contains("Device:CP"));
        assert!(msg.contains("no embedded definition"));
    }

    /// S3-2: a real KiCAD 10.0.4 session got `{"pins": []}` for
    /// `Transistor_FET:2N7002` and `Regulator_Linear:XC6206PxxxMR` while
    /// R/C/Y/D/L in the same batch call came back fine. Both are *derived*
    /// symbols — `(extends "Parent")` with no pins of their own — and
    /// `ensure_lib_symbol` embeds the parent alongside them, so the pins are
    /// right there to be resolved. TAB-indented, as KiCAD 10 writes.
    fn derived_symbol_schematic() -> String {
        "(kicad_sch\n\
\t(version 20250610)\n\
\t(generator \"eeschema\")\n\
\t(uuid \"11111111-2222-3333-4444-555555555555\")\n\
\t(lib_symbols\n\
\t\t(symbol \"Transistor_FET:Q_NMOS_GSD\"\n\
\t\t\t(property \"Description\" \"scheme (pin number consists of\"\n\
\t\t\t\t(at 0 0 0)\n\
\t\t\t)\n\
\t\t\t(symbol \"Q_NMOS_GSD_1_1\"\n\
\t\t\t\t(pin input line\n\
\t\t\t\t\t(at -5.08 -2.54 0)\n\
\t\t\t\t\t(length 2.54)\n\
\t\t\t\t\t(name \"G\")\n\
\t\t\t\t\t(number \"1\")\n\
\t\t\t\t)\n\
\t\t\t\t(pin passive line\n\
\t\t\t\t\t(at 0 -5.08 90)\n\
\t\t\t\t\t(length 2.54)\n\
\t\t\t\t\t(name \"S\")\n\
\t\t\t\t\t(number \"2\")\n\
\t\t\t\t)\n\
\t\t\t\t(pin passive line\n\
\t\t\t\t\t(at 0 5.08 270)\n\
\t\t\t\t\t(length 2.54)\n\
\t\t\t\t\t(name \"D\")\n\
\t\t\t\t\t(number \"3\")\n\
\t\t\t\t)\n\
\t\t\t)\n\
\t\t\t(embedded_fonts no)\n\
\t\t)\n\
\t\t(symbol \"Transistor_FET:2N7002\"\n\
\t\t\t(extends \"Transistor_FET:Q_NMOS_GSD\")\n\
\t\t\t(property \"Reference\" \"Q\"\n\
\t\t\t\t(at 5.08 1.905 0)\n\
\t\t\t)\n\
\t\t\t(embedded_fonts no)\n\
\t\t)\n\
\t)\n\
\t(symbol\n\
\t\t(lib_id \"Transistor_FET:2N7002\")\n\
\t\t(at 100 80 0)\n\
\t\t(unit 1)\n\
\t\t(uuid \"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\")\n\
\t\t(property \"Reference\" \"Q1\"\n\
\t\t\t(at 105 78 0)\n\
\t\t)\n\
\t\t(property \"Value\" \"2N7002\"\n\
\t\t\t(at 105 80 0)\n\
\t\t)\n\
\t)\n\
)\n"
        .to_string()
    }

    #[tokio::test]
    async fn pin_locations_resolve_through_extends_for_derived_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.kicad_sch");
        std::fs::write(&path, derived_symbol_schematic()).unwrap();
        let ctx = test_ctx();

        let result = handle_get_schematic_pin_locations(
            &json!({ "schematic": path.display().to_string(), "reference": "Q1" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let v = result_json(&result);
        let pins = v["pins"].as_array().unwrap();
        assert_eq!(pins.len(), 3, "derived 2N7002 must inherit 3 pins, got {v}");

        // Drain pin (3) sits at local (0, +5.08) in Y-up symbol space, which is
        // 5.08 mm ABOVE the component in Y-down schematic space.
        let d = pins.iter().find(|p| p["number"] == "3").unwrap();
        assert_eq!(d["name"], "D");
        assert!((d["x"].as_f64().unwrap() - 100.0).abs() < 1e-9);
        assert!((d["y"].as_f64().unwrap() - 74.92).abs() < 1e-9, "got {d}");
    }

    #[tokio::test]
    async fn batch_pin_locations_resolve_through_extends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived_batch.kicad_sch");
        std::fs::write(&path, derived_symbol_schematic()).unwrap();
        let ctx = test_ctx();

        let result = handle_batch_get_pin_locations(
            &json!({ "schematic": path.display().to_string(), "references": ["Q1"] }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", result.content);

        let v = result_json(&result);
        let comp = &v["components"][0];
        assert_eq!(
            comp["pins"].as_array().unwrap().len(),
            3,
            "S3-2: batch call returned an empty pin list for a derived symbol: {v}"
        );
    }

    /// The JSON payload of a CallToolResult, for tests that inspect it.
    fn result_json(result: &CallToolResult) -> serde_json::Value {
        let text = result
            .content
            .iter()
            .find_map(|c| match c {
                crate::mcp::protocol::ToolContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .expect("a text content block");
        serde_json::from_str(&text).expect("payload must be JSON")
    }
}

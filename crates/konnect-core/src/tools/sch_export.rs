//! `sch_export` toolset — export, netlist, ERC, connectivity fix, board sync.
//!
//! All export operations delegate to `kicad-cli` via the `cli` module.
//! `export_netlist_summary` and `fix_connectivity` operate directly on
//! S-expression file content so they work without a running KiCAD instance.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    schematic::{
        extract_labels, extract_lib_pins_resolved, extract_symbol_instances, extract_wires,
        pin_endpoint, read_schematic,
    },
    writer::{
        apply_edits, find_balanced_block, find_block_starts, find_enclosing_block,
        write_atomic_checked, SexpEdit,
    },
};
use serde_json::json;

use super::cli;
use super::sch_analysis::build_net_graph;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "export_schematic_svg",
            "Export a schematic sheet to an SVG file using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output SVG file path (directory used as output dir)" },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false },
                    "theme": { "type": "string", "description": "KiCAD colour theme name (optional)" }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_export_svg(args, ctx).await }
        ),
        tool!(
            "export_schematic_pdf",
            "Export a schematic sheet to a PDF file using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output PDF file path" },
                    "black_and_white": { "type": "boolean", "description": "Render in black and white", "default": false },
                    "all_sheets": { "type": "boolean", "description": "Include all hierarchical sheets", "default": true }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_export_pdf(args, ctx).await }
        ),
        tool!(
            "generate_netlist",
            "Generate a KiCAD netlist file from the schematic using kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Output .net file path" },
                    "format": {
                        "type": "string",
                        "description": "Netlist format: 'kicad', 'orcadpcb2', 'cadstar', 'spice'",
                        "default": "kicad"
                    }
                },
                "required": ["schematic", "output"]
            }),
            |args, ctx| async move { handle_generate_netlist(args, ctx).await }
        ),
        tool!(
            "export_netlist_summary",
            "Return a human-readable JSON summary of the schematic netlist: all \
             components, their nets, pin counts. Does not require kicad-cli.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_export_netlist_summary(args, ctx).await }
        ),
        tool!(
            "run_erc",
            "Run the Electrical Rules Check (ERC) on the schematic via kicad-cli \
             and return a list of violations filtered by severity.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "output":    { "type": "string", "description": "Optional path to write ERC report JSON" },
                    "severity":  {
                        "type": "string",
                        "description": "Minimum severity to report: 'error', 'warning', 'info'",
                        "default": "warning"
                    }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_run_erc(args, ctx).await }
        ),
        tool!(
            "fix_connectivity",
            "Scan the schematic for near-miss wire endpoints (within snap_tolerance of a \
             pin or label but not exactly on it) and snap them into place. Use dry_run \
             to preview fixes without writing.",
            json!({
                "type": "object",
                "properties": {
                    "schematic":       { "type": "string", "description": "Path to .kicad_sch file" },
                    "snap_tolerance":  { "type": "number", "description": "Snap distance in mm", "default": 0.05 },
                    "dry_run":         { "type": "boolean", "description": "Report fixes without applying them", "default": false }
                },
                "required": ["schematic"]
            }),
            |args, ctx| async move { handle_fix_connectivity(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_export_svg(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;

    // kicad-cli writes to an output directory and names the file <stem>.svg
    let output_dir = output_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&output_dir)?;

    let svg_path = cli::export_schematic_svg(&ctx.config.kicad_cli, &sch_path, &output_dir).await?;

    Ok(CallToolResult::json(&json!({
        "exported": svg_path.display().to_string()
    })))
}

async fn handle_export_pdf(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    cli::export_schematic_pdf(&ctx.config.kicad_cli, &sch_path, &output_path).await?;

    Ok(CallToolResult::json(&json!({
        "exported": output_path.display().to_string()
    })))
}

async fn handle_generate_netlist(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let output_path = get_path(args, "output")?;
    let format = args["format"].as_str().unwrap_or("kicad");

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    cli::export_netlist(&ctx.config.kicad_cli, &sch_path, &output_path, format).await?;

    Ok(CallToolResult::json(&json!({
        "exported": output_path.display().to_string(),
        "format": format
    })))
}

async fn handle_export_netlist_summary(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let (_, tree) = read_schematic(&sch_path)?;

    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let mut g = build_net_graph(&wires, &labels);

    // Collect distinct net names
    let mut net_names: Vec<String> = labels.iter().map(|l| l.net.clone()).collect();
    net_names.sort();
    net_names.dedup();

    // Build per-component net map
    let components: Vec<serde_json::Value> = instances
        .iter()
        .map(|inst| {
            let lib_sym = lib_syms
                .iter()
                .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));

            let pins: Vec<serde_json::Value> = if let Some(sym) = lib_sym {
                let t = inst.pin_transform();
                // `_resolved` follows `(extends "Parent")`: derived parts such
                // as Transistor_FET:2N7002 carry no pins of their own, so the
                // netlist listed them with zero pins (S3-2).
                extract_lib_pins_resolved(sym, &lib_syms)
                    .iter()
                    .map(|p| {
                        let (px, py) = pin_endpoint(p, t);
                        let net = g.net_at(px, py).unwrap_or_else(|| "~".to_string());
                        json!({
                            "number": p.number,
                            "name": p.name,
                            "net": net,
                            "x": px, "y": py
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };

            json!({
                "reference": inst.reference,
                "value": inst.value,
                "footprint": inst.footprint,
                "lib_id": inst.lib_id,
                "pin_count": pins.len(),
                "pins": pins
            })
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "component_count": components.len(),
        "net_count": net_names.len(),
        "nets": net_names,
        "components": components
    })))
}

async fn handle_run_erc(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let min_severity = args["severity"].as_str().unwrap_or("warning");

    let violations = cli::run_erc(&ctx.config.kicad_cli, &sch_path).await?;

    let severity_rank = |s: &str| match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    };
    let min_rank = severity_rank(min_severity);

    let filtered: Vec<serde_json::Value> = violations
        .iter()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .map(|v| {
            let mut entry = json!({
                "severity": v.severity,
                "description": v.description,
            });
            if let Some(sheet) = &v.sheet {
                entry["sheet"] = json!(sheet);
            }
            if let Some(pos) = &v.pos {
                entry["x"] = json!(pos.x);
                entry["y"] = json!(pos.y);
            }
            entry
        })
        .collect();

    // Optionally write the report to a file
    if let Some(out_path) = args["output"].as_str() {
        let report = serde_json::to_string_pretty(&filtered)?;
        std::fs::write(out_path, report)?;
    }

    let error_count = filtered.iter().filter(|v| v["severity"] == "error").count();
    let warning_count = filtered
        .iter()
        .filter(|v| v["severity"] == "warning")
        .count();

    Ok(CallToolResult::json(&json!({
        "total": filtered.len(),
        "errors": error_count,
        "warnings": warning_count,
        "violations": filtered
    })))
}

async fn handle_fix_connectivity(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let snap_tol = args["snap_tolerance"].as_f64().unwrap_or(0.05);
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let exact_tol = 0.01_f64;

    let (content, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let instances = extract_symbol_instances(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    // Collect all valid snap targets: pin endpoints + label positions + wire endpoints
    let mut snap_targets: Vec<(f64, f64)> = Vec::new();

    for inst in &instances {
        let lib_sym = lib_syms
            .iter()
            .find(|n| n.get(1).and_then(|c| c.as_str()) == Some(&inst.lib_id));
        if let Some(sym) = lib_sym {
            let t = inst.pin_transform();
            // Derived symbols inherit their pins — without resolving, their
            // pins were missing from the snap targets entirely.
            for pin in extract_lib_pins_resolved(sym, &lib_syms) {
                snap_targets.push(pin_endpoint(&pin, t));
            }
        }
    }
    for l in &labels {
        snap_targets.push((l.x, l.y));
    }
    for w in &wires {
        snap_targets.push((w.x1, w.y1));
        snap_targets.push((w.x2, w.y2));
    }

    let mut fixes: Vec<serde_json::Value> = Vec::new();
    let mut file_edits: Vec<SexpEdit> = Vec::new();

    for w in &wires {
        for (is_start, (px, py)) in &[(true, (w.x1, w.y1)), (false, (w.x2, w.y2))] {
            let px = *px;
            let py = *py;
            // Count how many targets are exactly at this point
            // (count >= 2 → there is at least one other connected thing)
            let exact_count = snap_targets
                .iter()
                .filter(|(tx, ty)| points_coincident(px, py, *tx, *ty, exact_tol))
                .count();

            if exact_count >= 2 {
                continue; // already connected
            }
            // Also consider T-junctions (endpoint in middle of another wire)
            if wires.iter().any(|w2| {
                point_on_segment(px, py, w2.x1, w2.y1, w2.x2, w2.y2, exact_tol)
                    && !points_coincident(px, py, w2.x1, w2.y1, exact_tol)
                    && !points_coincident(px, py, w2.x2, w2.y2, exact_tol)
            }) {
                continue; // T-junction — already connected
            }

            // Look for a near-miss snap target within snap_tol
            let near = snap_targets.iter().find(|(tx, ty)| {
                let dist = ((px - tx).powi(2) + (py - ty).powi(2)).sqrt();
                dist > exact_tol && dist <= snap_tol
            });

            if let Some(&(tx, ty)) = near {
                // Only claim a fix was applied once the edit has actually been
                // located: `fixes` is pushed unconditionally, so deriving
                // `applied` from it reported success on files where not one
                // byte was written.
                let mut edit_made = false;
                if !dry_run {
                    if let Some(edit) = w
                        .uuid
                        .as_deref()
                        .and_then(|u| wire_endpoint_edit(&content, u, *is_start, tx, ty))
                    {
                        file_edits.push(edit);
                        edit_made = true;
                    }
                }

                fixes.push(json!({
                    "wire_uuid": w.uuid,
                    "endpoint": if *is_start { "start" } else { "end" },
                    "from": { "x": px, "y": py },
                    "to":   { "x": tx, "y": ty },
                    "applied": edit_made
                }));
            }
        }
    }

    let fixes_applied = file_edits.len();
    let mut applied = false;
    if !dry_run && !file_edits.is_empty() {
        let new_content = apply_edits(content, file_edits);
        // Validate before replacing the user's schematic: a mis-computed
        // offset must fail the call, not produce an unopenable file.
        write_atomic_checked(&sch_path, &new_content, "kicad_sch")?;
        applied = true;
    }

    let mut result = json!({
        "fixes_found": fixes.len(),
        "fixes_applied": fixes_applied,
        "applied": applied,
        "dry_run": dry_run,
        "fixes": fixes
    });
    if !dry_run && fixes_applied < result["fixes_found"].as_u64().unwrap_or(0) as usize {
        result["note"] = json!(
            "Some near-miss endpoints could not be located in the file and were left unchanged."
        );
    }

    Ok(CallToolResult::json(&result))
}

/// Byte-range edit that moves one endpoint of the wire carrying `uuid` to
/// (`tx`, `ty`). Returns `None` when the wire or its coordinate cannot be
/// located, so the caller never reports an edit it did not make.
///
/// Indentation-agnostic (KiCAD writes tabs, this crate wrote two spaces) and
/// handles both the KiCAD 10 `(pts (xy …) (xy …))` form and the legacy
/// `(start …)` / `(end …)` form.
fn wire_endpoint_edit(
    content: &str,
    uuid: &str,
    is_start: bool,
    tx: f64,
    ty: f64,
) -> Option<SexpEdit> {
    let uuid_pos = content.find(&format!(r#"(uuid "{uuid}")"#))?;
    let (wbs, wbe) = find_enclosing_block(content, "wire", uuid_pos)?;
    let wire_block = &content[wbs..wbe];

    // KiCAD 10: (wire (pts (xy X Y) (xy X Y)) …)
    if let Some(&pts_rel) = find_block_starts(wire_block, "pts").first() {
        let (pbs, pbe) = find_balanced_block(wire_block, pts_rel)?;
        let pts_block = &wire_block[pbs..pbe];
        let xy_starts = find_block_starts(pts_block, "xy");
        let &xy_rel = xy_starts.get(if is_start { 0 } else { 1 })?;
        let (xs, xe) = find_balanced_block(pts_block, xy_rel)?;
        return Some(SexpEdit::replace(
            wbs + pbs + xs,
            wbs + pbs + xe,
            format!("(xy {tx} {ty})"),
        ));
    }

    // KiCAD 8/9: (wire (start X Y) (end X Y) …)
    let tag = if is_start { "start" } else { "end" };
    let &tag_rel = find_block_starts(wire_block, tag).first()?;
    let (ts, te) = find_balanced_block(wire_block, tag_rel)?;
    Some(SexpEdit::replace(
        wbs + ts,
        wbs + te,
        format!("({tag} {tx} {ty})"),
    ))
}

#[cfg(test)]
mod fix_connectivity_tests {
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

    /// TAB-indented, as eeschema writes it. The old `rfind("\n  (wire")`
    /// matched nothing here, so every "fix" was a silent no-op.
    fn tab_schematic() -> String {
        [
            "(kicad_sch",
            "\t(version 20250610)",
            "\t(generator \"eeschema\")",
            "\t(uuid \"22222222-2222-2222-2222-222222222222\")",
            "\t(paper \"A4\")",
            "\t(lib_symbols",
            "\t)",
            "\t(wire",
            "\t\t(pts",
            "\t\t\t(xy 100 100) (xy 110 100)",
            "\t\t)",
            "\t\t(stroke",
            "\t\t\t(width 0)",
            "\t\t\t(type default)",
            "\t\t)",
            "\t\t(uuid \"aaaaaaaa-0000-0000-0000-000000000001\")",
            "\t)",
            "\t(label \"NET1\"",
            "\t\t(at 110.02 100 0)",
            "\t\t(uuid \"bbbbbbbb-0000-0000-0000-000000000002\")",
            "\t)",
            ")",
            "",
        ]
        .join("\n")
    }

    fn result_json(res: &CallToolResult) -> serde_json::Value {
        match &res.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => serde_json::from_str(text).unwrap(),
            _ => panic!("expected text content"),
        }
    }

    fn write_temp(content: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "konnect-fixconn-{}",
            konnect_sexp::writer::new_uuid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.kicad_sch");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn applies_edit_to_tab_indented_schematic() {
        let (dir, path) = write_temp(&tab_schematic());
        let args = json!({ "schematic": path.to_str().unwrap(), "snap_tolerance": 0.05 });

        let res = handle_fix_connectivity(&args, &test_ctx()).await.unwrap();
        let out = result_json(&res);

        assert_eq!(out["fixes_found"], 1, "{out}");
        assert_eq!(out["fixes_applied"], 1, "{out}");
        assert_eq!(out["applied"], true, "{out}");

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("(xy 110.02 100)"),
            "endpoint was not moved: {after}"
        );
        assert!(after.contains("(xy 100 100)"), "other endpoint disturbed");
        konnect_sexp::writer::check_document(&after, "kicad_sch").unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dry_run_reports_not_applied_and_leaves_file_alone() {
        let original = tab_schematic();
        let (dir, path) = write_temp(&original);
        let args = json!({
            "schematic": path.to_str().unwrap(),
            "snap_tolerance": 0.05,
            "dry_run": true
        });

        let res = handle_fix_connectivity(&args, &test_ctx()).await.unwrap();
        let out = result_json(&res);

        assert_eq!(out["fixes_found"], 1, "{out}");
        assert_eq!(out["applied"], false, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn applied_is_false_when_no_edit_can_be_located() {
        // Wire without a uuid: a fix is identifiable but not addressable.
        let sch =
            tab_schematic().replace("\t\t(uuid \"aaaaaaaa-0000-0000-0000-000000000001\")\n", "");
        let (dir, path) = write_temp(&sch);
        let args = json!({ "schematic": path.to_str().unwrap(), "snap_tolerance": 0.05 });

        let res = handle_fix_connectivity(&args, &test_ctx()).await.unwrap();
        let out = result_json(&res);

        assert_eq!(out["fixes_found"], 1, "{out}");
        assert_eq!(out["fixes_applied"], 0, "{out}");
        assert_eq!(
            out["applied"], false,
            "reported success without writing anything: {out}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), sch);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wire_endpoint_edit_handles_tabs_and_legacy_format() {
        let sch = tab_schematic();
        let edit = wire_endpoint_edit(
            &sch,
            "aaaaaaaa-0000-0000-0000-000000000001",
            false,
            110.02,
            100.0,
        )
        .expect("tab-indented wire must be found");
        assert_eq!(&sch[edit.start..edit.end], "(xy 110 100)");

        // KiCAD 8/9 form, two-space indented.
        let legacy = "(kicad_sch\n  (wire (start 1 2) (end 3 4) (uuid \"w9\"))\n)\n";
        let edit = wire_endpoint_edit(legacy, "w9", true, 5.0, 6.0).unwrap();
        assert_eq!(&legacy[edit.start..edit.end], "(start 1 2)");
        assert_eq!(edit.replacement, "(start 5 6)");

        assert!(wire_endpoint_edit(&sch, "no-such-uuid", true, 0.0, 0.0).is_none());
        // A uuid that belongs to a label, not a wire.
        assert!(
            wire_endpoint_edit(&sch, "bbbbbbbb-0000-0000-0000-000000000002", true, 0.0, 0.0)
                .is_none()
        );
    }
}

//! `verification` toolset — DRC, design rules, KiCAD UI management, routing utilities.
//!
//! DRC delegates to `kicad-cli`. Design rule constraints live in the project's
//! `.kicad_pro` (JSON) and custom rules in `.kicad_dru` — NOT in `.kicad_pcb`.
//! KiCAD UI management uses process inspection + subprocess spawning.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, require_str, ToolContext, ToolDef};
use konnect_sexp::writer::write_atomic;
use serde_json::json;
use tokio::task;

use super::cli;

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "run_drc",
            "Run the Design Rule Check on the PCB and return structured violation results, \
             with separate error and warning counts in the summary. Prefer this over \
             `get_drc_violations` (pcb_export toolset) — they run the same underlying \
             kicad-cli check, but `run_drc` returns a cleaner breakdown.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "output": { "type": "string", "description": "Optional path to write DRC report JSON" },
                    "severity": {
                        "type": "string",
                        "description": "Minimum violation severity to include: 'error', 'warning' (default), 'info'",
                        "default": "warning"
                    },
                    "tests": {
                        "type": "array",
                        "description": "Specific DRC test IDs to run (empty = all tests)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_run_drc(args, ctx).await }
        ),
        tool!(
            "set_design_rules",
            "Set board-level design rule constraints. These are stored in the project's \
             .kicad_pro file (board.design_settings.rules), which is what KiCAD reads — \
             not in .kicad_pcb. Reload the project in KiCAD for changes to take effect.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_pro is updated" },
                    "min_clearance": { "type": "number", "description": "Minimum copper clearance in mm" },
                    "min_trace_width": { "type": "number", "description": "Minimum track width in mm" },
                    "min_via_drill": { "type": "number", "description": "Minimum through-hole (drill) diameter in mm" },
                    "min_via_size": { "type": "number", "description": "Minimum via diameter in mm" },
                    "min_hole_to_hole": { "type": "number", "description": "Minimum hole-to-hole clearance in mm" },
                    "min_hole_clearance": { "type": "number", "description": "Minimum hole-to-copper clearance in mm" },
                    "min_via_annular_width": { "type": "number", "description": "Minimum via annular ring width in mm" },
                    "min_copper_edge_clearance": { "type": "number", "description": "Minimum copper-to-board-edge clearance in mm" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_set_design_rules(args, ctx).await }
        ),
        tool!(
            "get_design_rules",
            "Return the board's design rule constraints, read from the project's \
             .kicad_pro file (board.design_settings.rules).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_design_rules(args, ctx).await }
        ),
        tool!(
            "check_kicad_ui",
            "Check whether the KiCAD GUI application is running and responsive.",
            json!({
                "type": "object",
                "properties": {
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout for the health check in seconds",
                        "default": 5
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_check_kicad_ui(args, ctx).await }
        ),
        tool!(
            "launch_kicad_ui",
            "Launch the KiCAD GUI application and optionally open a project file.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Path to .kicad_pro file to open (optional)" },
                    "wait_ready": {
                        "type": "boolean",
                        "description": "Wait until KiCAD IPC is responsive before returning",
                        "default": true
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Maximum wait time in seconds",
                        "default": 30
                    }
                },
                "required": []
            }),
            |args, ctx| async move { handle_launch_kicad_ui(args, ctx).await }
        ),
        tool!(
            "copy_routing_pattern",
            "Copy a routing pattern (traces and vias) from one region of the board to another.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "src_x1": { "type": "number", "description": "Source region bounding box min X" },
                    "src_y1": { "type": "number", "description": "Source region bounding box min Y" },
                    "src_x2": { "type": "number", "description": "Source region bounding box max X" },
                    "src_y2": { "type": "number", "description": "Source region bounding box max Y" },
                    "dest_x": { "type": "number", "description": "Destination anchor X (maps to src_x1)" },
                    "dest_y": { "type": "number", "description": "Destination anchor Y (maps to src_y1)" },
                    "net_map": {
                        "type": "object",
                        "description": "Optional mapping from source net names to destination net names"
                    }
                },
                "required": ["board", "src_x1", "src_y1", "src_x2", "src_y2", "dest_x", "dest_y"]
            }),
            |args, ctx| async move { handle_copy_routing_pattern(args, ctx).await }
        ),
        tool!(
            "set_layer_constraints",
            "Set per-layer design constraints (min trace width, clearance) as custom DRC rules. \
             Written to the project's .kicad_dru file, which is where KiCAD keeps custom rules — \
             not in .kicad_pcb. Reload the project in KiCAD to apply them.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file; the sibling .kicad_dru is updated" },
                    "layer": { "type": "string", "description": "Layer name (e.g. 'F.Cu', 'B.Cu')" },
                    "min_clearance": { "type": "number", "description": "Minimum clearance for this layer in mm" },
                    "min_trace_width": { "type": "number", "description": "Minimum trace width for this layer in mm" }
                },
                "required": ["board", "layer"]
            }),
            |args, ctx| async move { handle_set_layer_constraints(args, ctx).await }
        ),
        tool!(
            "check_clearance",
            "Check the physical clearance (distance) between two components on the PCB.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                    "ref1":  { "type": "string", "description": "First component reference (e.g. 'U1')" },
                    "ref2":  { "type": "string", "description": "Second component reference (e.g. 'C1')" }
                },
                "required": ["board", "ref1", "ref2"]
            }),
            |args, ctx| async move { handle_check_clearance(args, ctx).await }
        ),
    ]
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

fn severity_rank(s: &str) -> u8 {
    match s {
        "error" => 2,
        "warning" => 1,
        _ => 0,
    }
}

async fn handle_run_drc(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let severity_filter = args["severity"].as_str().unwrap_or("warning");
    let min_rank = severity_rank(severity_filter);

    let refill = args["refill_zones"].as_bool().unwrap_or(false);
    let violations = cli::run_drc(&ctx.config.kicad_cli, &board, refill).await?;

    // Optionally write report
    if let Some(out_path) = args["output"].as_str() {
        let report = serde_json::to_string_pretty(&violations)?;
        tokio::fs::write(out_path, report).await?;
    }

    let filtered: Vec<_> = violations
        .iter()
        .filter(|v| severity_rank(&v.severity) >= min_rank)
        .collect();

    let errors = filtered.iter().filter(|v| v.severity == "error").count();
    let warnings = filtered.iter().filter(|v| v.severity == "warning").count();

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "total_violations": violations.len(),
            "filtered_count": filtered.len(),
            "errors": errors,
            "warnings": warnings,
            "severity_filter": severity_filter,
            "violations": filtered.iter().map(|v| json!({
                "severity": v.severity,
                "description": v.description,
                "pos": v.pos.as_ref().map(|p| json!({ "x": p.x, "y": p.y }))
            })).collect::<Vec<_>>()
        }))
        .unwrap(),
    ))
}

// ─── Design rule constraints (.kicad_pro) ────────────────────────────────────

/// Tool argument name -> the key KiCAD uses in
/// `.kicad_pro` → `board.design_settings.rules`.
///
/// These constraints do NOT live in `.kicad_pcb`. Writing `(min_clearance …)`
/// and friends into the board's `(setup …)` block produces a file KiCAD refuses
/// to open outright — the parens balance, so nothing catches it until the board
/// will not load.
const DESIGN_RULE_KEYS: &[(&str, &str)] = &[
    ("min_clearance", "min_clearance"),
    ("min_trace_width", "min_track_width"),
    ("min_via_size", "min_via_diameter"),
    ("min_via_drill", "min_through_hole_diameter"),
    ("min_hole_to_hole", "min_hole_to_hole"),
    ("min_hole_clearance", "min_hole_clearance"),
    ("min_via_annular_width", "min_via_annular_width"),
    ("min_copper_edge_clearance", "min_copper_edge_clearance"),
];

/// The `.kicad_pro` beside a `.kicad_pcb`.
fn project_file_for(board: &std::path::Path) -> std::path::PathBuf {
    board.with_extension("kicad_pro")
}

/// `board.design_settings.rules` as a mutable object, creating the path if the
/// project file has not recorded any rules yet.
fn rules_object(root: &mut serde_json::Value) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    let mut cur = root;
    for key in ["board", "design_settings", "rules"] {
        cur = cur.as_object_mut()?.entry(key).or_insert_with(|| json!({}));
    }
    cur.as_object_mut()
}

async fn handle_set_design_rules(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let project = project_file_for(&board);

    if !project.exists() {
        return Ok(CallToolResult::error(format!(
            "No project file at '{}'. Design rule constraints are stored in \
             .kicad_pro (board.design_settings.rules), not in .kicad_pcb, so the \
             project file must exist.",
            project.display()
        )));
    }

    let raw = tokio::fs::read_to_string(&project).await?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("'{}' is not valid JSON: {e}", project.display()))?;

    let mut changed = Vec::new();
    {
        let Some(rules) = rules_object(&mut root) else {
            return Ok(CallToolResult::error(format!(
                "'{}' has a 'board.design_settings' entry that is not an object",
                project.display()
            )));
        };
        for (arg_key, pro_key) in DESIGN_RULE_KEYS {
            if let Some(val) = args[*arg_key].as_f64() {
                rules.insert((*pro_key).to_string(), json!(val));
                changed.push(format!("{} = {}", pro_key, val));
            }
        }
    }

    if changed.is_empty() {
        return Ok(CallToolResult::error(format!(
            "No design rule given. Accepted: {}.",
            DESIGN_RULE_KEYS
                .iter()
                .map(|(a, _)| *a)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let out = serde_json::to_string_pretty(&root)? + "\n";
    // Re-read our own output before it replaces the project file.
    if serde_json::from_str::<serde_json::Value>(&out).is_err() {
        return Ok(CallToolResult::error(
            "Internal error: the edited project file is not valid JSON — nothing was written.",
        ));
    }
    write_atomic_json(&project, &out)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "file": project.display().to_string(),
            "changed": changed,
            "note": "KiCAD reads these from .kicad_pro; reload the project for them to take effect."
        }))
        .unwrap(),
    ))
}

/// Atomic write for the project JSON: temp file -> fsync -> rename.
fn write_atomic_json(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("kicad_pro.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn handle_get_design_rules(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let project = project_file_for(&board);

    // Read from .kicad_pro, where KiCAD actually keeps these. Reading the
    // .kicad_pcb returned null for every constraint, since none are stored there.
    if !project.exists() {
        return Ok(CallToolResult::error(format!(
            "No project file at '{}'. Design rule constraints live in \
             .kicad_pro (board.design_settings.rules).",
            project.display()
        )));
    }
    let raw = tokio::fs::read_to_string(&project).await?;
    let root: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("'{}' is not valid JSON: {e}", project.display()))?;

    let stored = root
        .get("board")
        .and_then(|b| b.get("design_settings"))
        .and_then(|d| d.get("rules"));

    let mut rules = serde_json::Map::new();
    for (arg_key, pro_key) in DESIGN_RULE_KEYS {
        let val = stored
            .and_then(|r| r.get(*pro_key))
            .and_then(|v| v.as_f64());
        rules.insert((*arg_key).to_string(), json!(val));
    }

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "board": board.to_str().unwrap_or(""),
            "file": project.display().to_string(),
            "rules": rules
        }))
        .unwrap(),
    ))
}

// ─── KiCAD UI management ──────────────────────────────────────────────────────

/// Check if the KiCAD GUI is running by scanning the process list.
fn is_kicad_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        // On Windows, use `tasklist` to check
        std::process::Command::new("tasklist")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("kicad.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("pgrep")
            .arg("-x")
            .arg("kicad")
            .output()
            .ok()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// Resolve the KiCAD binary path from config or well-known locations.
fn find_kicad_binary(config_binary: &str) -> String {
    if !config_binary.is_empty() && std::path::Path::new(config_binary).exists() {
        return config_binary.to_string();
    }
    #[cfg(target_os = "windows")]
    {
        // Scan common install roots and KiCAD version directories
        let roots = [
            r"C:\Program Files\KiCad",
            r"C:\KiCad",
            r"D:\KiCad",
            r"D:\Program Files\KiCad",
        ];
        let versions = ["10.0", "9.0", "8.0"];
        for root in &roots {
            for ver in &versions {
                let path = format!(r"{}\{}\bin\kicad.exe", root, ver);
                if std::path::Path::new(&path).exists() {
                    return path;
                }
            }
            // Also check without version subdir
            let path = format!(r"{}\bin\kicad.exe", root);
            if std::path::Path::new(&path).exists() {
                return path;
            }
        }
        "kicad".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad".to_string()
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        "kicad".to_string()
    }
}

async fn handle_check_kicad_ui(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let running = task::spawn_blocking(is_kicad_running).await?;

    if !running {
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "running": false,
                "ipc_responsive": false
            }))
            .unwrap(),
        ));
    }

    // Try IPC ping
    let addr = ctx.config.ipc_address.clone();
    let ipc_ok = task::spawn_blocking(move || {
        konnect_ipc::client::KiCadIpcClient::new(&addr)
            .ping()
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "running": true,
            "ipc_responsive": ipc_ok
        }))
        .unwrap(),
    ))
}

async fn handle_launch_kicad_ui(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let wait_ready = args["wait_ready"].as_bool().unwrap_or(true);
    let timeout_secs = args["timeout_seconds"].as_u64().unwrap_or(30);
    let binary = find_kicad_binary(&ctx.config.kicad_binary);

    let mut cmd = tokio::process::Command::new(&binary);
    if let Some(project) = args["project"].as_str() {
        cmd.arg(project);
    }

    // Spawn detached — we don't wait for the process to exit
    match cmd.spawn() {
        Ok(_child) => {
            if wait_ready {
                // Poll IPC until responsive or timeout
                let addr = ctx.config.ipc_address.clone();
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let addr2 = addr.clone();
                    let ok = task::spawn_blocking(move || {
                        konnect_ipc::client::KiCadIpcClient::new(&addr2)
                            .ping()
                            .unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false);

                    if ok {
                        return Ok(CallToolResult::text(
                            serde_json::to_string_pretty(&json!({
                                "launched": true,
                                "ipc_ready": true
                            }))
                            .unwrap(),
                        ));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Ok(CallToolResult::text(
                            serde_json::to_string_pretty(&json!({
                                "launched": true,
                                "ipc_ready": false,
                                "note": "KiCAD launched but IPC not yet responsive within timeout"
                            }))
                            .unwrap(),
                        ));
                    }
                }
            }

            Ok(CallToolResult::text(
                serde_json::to_string_pretty(&json!({
                    "launched": true,
                    "ipc_ready": null
                }))
                .unwrap(),
            ))
        }
        Err(e) => Ok(CallToolResult::error(format!(
            "Failed to launch KiCAD ({}): {}",
            binary, e
        ))),
    }
}

// ─── Copy routing pattern ─────────────────────────────────────────────────────

async fn handle_copy_routing_pattern(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let src_x1 = args["src_x1"].as_f64().unwrap_or(0.0);
    let src_y1 = args["src_y1"].as_f64().unwrap_or(0.0);
    let src_x2 = args["src_x2"].as_f64().unwrap_or(0.0);
    let src_y2 = args["src_y2"].as_f64().unwrap_or(0.0);
    let dest_x = args["dest_x"].as_f64().unwrap_or(0.0);
    let dest_y = args["dest_y"].as_f64().unwrap_or(0.0);

    let dx = dest_x - src_x1;
    let dy = dest_y - src_y1;

    let net_map: std::collections::HashMap<String, String> =
        if let Some(obj) = args["net_map"].as_object() {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    let content = tokio::fs::read_to_string(&board).await?;
    let mut new_tracks = Vec::new();

    // Find all (segment ...) and (via ...) blocks within the bounding box
    // and collect translated copies.
    for (block_start, block_end, _block_type) in find_routing_blocks(&content) {
        let block = &content[block_start..block_end];
        if let Some((bx, by)) = extract_start_xy(block) {
            if bx >= src_x1 && bx <= src_x2 && by >= src_y1 && by <= src_y2 {
                let translated = translate_block(block, dx, dy, &net_map);
                new_tracks.push(translated);
            }
        }
    }

    if new_tracks.is_empty() {
        return Ok(CallToolResult::text(
            serde_json::to_string_pretty(&json!({
                "copied": 0,
                "note": "No routing elements found in the specified source region"
            }))
            .unwrap(),
        ));
    }

    // Insert all new blocks before the final `)` of the file
    let insert_pos = content.rfind(')').unwrap_or(content.len());
    let insertion = new_tracks.join("\n");
    let new_content = format!(
        "{}\n{}\n{}",
        &content[..insert_pos],
        insertion,
        &content[insert_pos..]
    );

    // Assign new UUIDs to inserted blocks (replace uuid "ORIGINAL" with new ones)
    let new_content = reassign_uuids(&new_content, insert_pos);

    write_atomic(&board, &new_content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "copied": new_tracks.len(),
            "dx": dx,
            "dy": dy
        }))
        .unwrap(),
    ))
}

/// Find all `(segment ...)` and `(via ...)` blocks in the PCB content.
/// Returns (start, end, type) tuples.
fn find_routing_blocks(content: &str) -> Vec<(usize, usize, &'static str)> {
    let mut results = Vec::new();
    for (prefix, kind) in &[("\n  (segment ", "segment"), ("\n  (via ", "via")] {
        let mut pos = 0;
        while let Some(found) = content[pos..].find(prefix) {
            let start = pos + found + 3; // skip \n
            let mut depth = 0i32;
            let mut end = start;
            for (i, ch) in content[start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            results.push((start, end, *kind));
            pos = start + 1;
        }
    }
    results
}

/// Extract the `(start X Y)` coordinates from a routing block.
fn extract_start_xy(block: &str) -> Option<(f64, f64)> {
    let pat = "(start ";
    let pos = block.find(pat)?;
    let after = &block[pos + pat.len()..];
    let end = after.find(')')?;
    let parts: Vec<&str> = after[..end].split_whitespace().collect();
    let x = parts.first()?.parse::<f64>().ok()?;
    let y = parts.get(1)?.parse::<f64>().ok()?;
    Some((x, y))
}

/// Translate all coordinate pairs in a routing block by (dx, dy).
fn translate_block(
    block: &str,
    dx: f64,
    dy: f64,
    net_map: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = block.to_string();

    // Translate (start X Y), (end X Y), (at X Y) coordinate pairs
    for coord_key in &["start", "end", "at"] {
        let pat = format!("({} ", coord_key);
        let mut new_result = String::new();
        let mut remaining = result.as_str();
        while let Some(pos) = remaining.find(&pat) {
            new_result.push_str(&remaining[..pos]);
            new_result.push_str(&pat);
            let after = &remaining[pos + pat.len()..];
            if let Some(close) = after.find(')') {
                let coords_str = &after[..close];
                let parts: Vec<&str> = coords_str.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let (Ok(x), Ok(y)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                        new_result.push_str(&format!("{} {}", x + dx, y + dy));
                        if parts.len() > 2 {
                            new_result.push(' ');
                            new_result.push_str(&parts[2..].join(" "));
                        }
                        new_result.push(')');
                        remaining = &remaining[pos + pat.len() + close + 1..];
                        continue;
                    }
                }
                // Fall through if parsing failed
                new_result.push_str(coords_str);
                new_result.push(')');
                remaining = &remaining[pos + pat.len() + close + 1..];
            } else {
                break;
            }
        }
        new_result.push_str(remaining);
        result = new_result;
    }

    // Remap net names
    for (old_net, new_net) in net_map {
        let old_pat = format!("(net \"{}\")", old_net);
        let new_pat = format!("(net \"{}\")", new_net);
        result = result.replace(&old_pat, &new_pat);
        // Also handle numeric net references if needed (not replaced here)
    }

    result
}

/// Reassign UUIDs in all newly inserted blocks (those after `insert_boundary`).
fn reassign_uuids(content: &str, insert_boundary: usize) -> String {
    let mut result = String::with_capacity(content.len() + 64);
    result.push_str(&content[..insert_boundary]);
    let tail = &content[insert_boundary..];
    let mut remaining = tail;
    while let Some(pos) = remaining.find("(uuid \"") {
        result.push_str(&remaining[..pos]);
        result.push_str("(uuid \"");
        // Find end of UUID string
        let after = &remaining[pos + 7..];
        if let Some(end) = after.find('"') {
            let new_uuid = uuid::Uuid::new_v4().to_string();
            result.push_str(&new_uuid);
            result.push('"');
            remaining = &remaining[pos + 7 + end + 1..];
        } else {
            break;
        }
    }
    result.push_str(remaining);
    result
}

// ─── Symbol info ──────────────────────────────────────────────────────────────

// ─── Layer constraints ───────────────────────────────────────────────────────

async fn handle_set_layer_constraints(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let layer = match require_str(args, "layer") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    // Custom DRC rules live in the project's `.kicad_dru` file, not in the
    // board. Inserting `(rule …)` into `.kicad_pcb`'s `(setup …)` leaves a file
    // KiCAD refuses to open — verified against kicad-cli 10.0, which reports
    // "Failed to load board". The parens balance, so nothing catches it until
    // the board will not load.
    let dru = board.with_extension("kicad_dru");

    let existing = match tokio::fs::read_to_string(&dru).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "(version 1)\n".to_string(),
        Err(e) => return Err(e.into()),
    };

    let rule_base = format!("{}_constraints", layer.replace('.', "_"));
    let wanted: Vec<(String, String)> = [
        ("clearance", "clearance", args["min_clearance"].as_f64()),
        ("trace_width", "track_width", args["min_trace_width"].as_f64()),
    ]
    .iter()
    .filter_map(|(suffix, constraint, val)| {
        let v = (*val)?;
        Some((
            format!("{rule_base}_{suffix}"),
            format!(
                "(rule \"{rule_base}_{suffix}\"\n\t(constraint {constraint} (min {v}mm))\n\t(condition \"A.Layer == '{layer}'\"))",
            ),
        ))
    })
    .collect();

    if wanted.is_empty() {
        return Ok(CallToolResult::error(
            "Give at least one of min_clearance or min_trace_width.",
        ));
    }

    // Replace a rule of the same name rather than appending a second one.
    let mut content = existing;
    let mut changed = Vec::new();
    for (name, rule_text) in &wanted {
        content = upsert_dru_rule(&content, name, rule_text);
        changed.push(name.clone());
    }

    // Re-read our own output before it replaces the file.
    if konnect_sexp::parser::parse_sexp(&content).is_err() {
        return Ok(CallToolResult::error(
            "Internal error: the edited .kicad_dru does not parse — nothing was written.",
        ));
    }
    write_atomic(&dru, &content)?;

    Ok(CallToolResult::text(
        serde_json::to_string_pretty(&json!({
            "success": true,
            "layer": layer,
            "file": dru.display().to_string(),
            "rules": changed,
            "note": "Custom DRC rules live in .kicad_dru; reload the project in KiCAD to apply them."
        }))
        .unwrap(),
    ))
}

/// Insert `rule_text` into a `.kicad_dru`, replacing any existing top-level
/// `(rule "name" …)` with the same name so repeated calls do not stack up.
fn upsert_dru_rule(content: &str, name: &str, rule_text: &str) -> String {
    let needle = format!("(rule \"{name}\"");
    for start in konnect_sexp::writer::find_block_starts(content, "rule") {
        if !content[start..].starts_with(&needle) {
            continue;
        }
        if let Some((s, e)) = konnect_sexp::writer::find_balanced_block(content, start) {
            return format!("{}{}{}", &content[..s], rule_text, &content[e..]);
        }
    }
    let mut out = content.trim_end().to_string();
    out.push_str("\n\n");
    out.push_str(rule_text);
    out.push('\n');
    out
}

// ─── Check clearance ─────────────────────────────────────────────────────────

async fn handle_check_clearance(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let ref1 = match require_str(args, "ref1") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let ref2 = match require_str(args, "ref2") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    let pos1 = find_footprint_position(&tree, &ref1)?;
    let pos2 = find_footprint_position(&tree, &ref2)?;

    let dx = pos2.0 - pos1.0;
    let dy = pos2.1 - pos1.1;
    let distance = (dx * dx + dy * dy).sqrt();

    Ok(CallToolResult::json(&json!({
        "ref1": ref1,
        "ref2": ref2,
        "pos1": { "x": pos1.0, "y": pos1.1 },
        "pos2": { "x": pos2.0, "y": pos2.1 },
        "distance_mm": (distance * 1000.0).round() / 1000.0
    })))
}

/// Look up the board-space (x, y) position of a footprint by its reference designator.
fn find_footprint_position(
    tree: &konnect_sexp::parser::SexpNode,
    reference: &str,
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

    Ok((fp_x, fp_y))
}

#[cfg(test)]
mod design_rule_tests {
    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn ctx() -> ToolContext {
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

    /// A minimal tab-indented board, as KiCAD writes them.
    const BOARD: &str = "(kicad_pcb\n\t(version 20260206)\n\t(generator \"pcbnew\")\n\t(setup\n\t\t(pad_to_mask_clearance 0)\n\t)\n)\n";

    fn project(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let board = dir.join("b.kicad_pcb");
        let pro = dir.join("b.kicad_pro");
        std::fs::write(&board, BOARD).unwrap();
        std::fs::write(
            &pro,
            r#"{"board":{"design_settings":{"rules":{"min_clearance":0.0}}},"meta":{"version":3}}"#,
        )
        .unwrap();
        (board, pro)
    }

    /// The constraints belong in .kicad_pro. Writing them into the board's
    /// (setup …) produced a file KiCAD refused to open, while reporting success.
    #[tokio::test]
    async fn design_rules_go_to_the_project_and_leave_the_board_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (board, pro) = project(dir.path());

        let r = handle_set_design_rules(
            &json!({ "board": board.display().to_string(),
                     "min_clearance": 0.127, "min_trace_width": 0.15,
                     "min_via_size": 0.45, "min_via_drill": 0.2 }),
            &ctx(),
        )
        .await
        .unwrap();
        assert!(!r.is_error);

        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            BOARD,
            "the .kicad_pcb must not be touched at all"
        );

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pro).unwrap()).unwrap();
        let rules = &v["board"]["design_settings"]["rules"];
        assert_eq!(rules["min_clearance"], 0.127);
        // KiCAD's own key names, not the tool's argument names.
        assert_eq!(rules["min_track_width"], 0.15);
        assert_eq!(rules["min_via_diameter"], 0.45);
        assert_eq!(rules["min_through_hole_diameter"], 0.2);
        assert_eq!(v["meta"]["version"], 3, "unrelated keys preserved");
    }

    #[tokio::test]
    async fn get_design_rules_reads_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let (board, _) = project(dir.path());
        handle_set_design_rules(
            &json!({ "board": board.display().to_string(), "min_trace_width": 0.2 }),
            &ctx(),
        )
        .await
        .unwrap();

        let r = handle_get_design_rules(&json!({ "board": board.display().to_string() }), &ctx())
            .await
            .unwrap();
        let text = match &r.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["rules"]["min_trace_width"], 0.2);
    }

    #[tokio::test]
    async fn missing_project_file_is_an_error_not_a_board_write() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("b.kicad_pcb");
        std::fs::write(&board, BOARD).unwrap();

        let r = handle_set_design_rules(
            &json!({ "board": board.display().to_string(), "min_clearance": 0.1 }),
            &ctx(),
        )
        .await
        .unwrap();
        assert!(r.is_error, "must not silently fall back to editing the board");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
    }

    /// Custom rules belong in .kicad_dru — `(rule …)` inside the board's
    /// `(setup …)` makes kicad-cli report "Failed to load board".
    #[tokio::test]
    async fn layer_constraints_write_a_dru_file_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (board, _) = project(dir.path());
        let dru = dir.path().join("b.kicad_dru");

        for clearance in [0.2, 0.3] {
            let r = handle_set_layer_constraints(
                &json!({ "board": board.display().to_string(),
                         "layer": "F.Cu", "min_clearance": clearance }),
                &ctx(),
            )
            .await
            .unwrap();
            assert!(!r.is_error);
        }

        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD, "board untouched");
        let text = std::fs::read_to_string(&dru).unwrap();
        assert_eq!(
            text.matches("(rule ").count(),
            1,
            "repeat calls must replace the rule, not stack: {text}"
        );
        assert!(text.contains("0.3mm"), "latest value wins: {text}");
        assert!(text.starts_with("(version 1)"));
        konnect_sexp::parser::parse_sexp(&text).expect(".kicad_dru must parse");
    }

    #[test]
    fn upsert_replaces_only_the_named_rule() {
        let src = "(version 1)\n\n(rule \"a\"\n\t(constraint clearance (min 0.1mm)))\n\n(rule \"b\"\n\t(constraint clearance (min 0.2mm)))\n";
        let out = upsert_dru_rule(src, "a", "(rule \"a\"\n\t(constraint clearance (min 0.9mm)))");
        assert!(out.contains("0.9mm"));
        assert!(out.contains("(rule \"b\""), "sibling rule survived: {out}");
        assert_eq!(out.matches("(rule ").count(), 2);
    }
}

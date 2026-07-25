//! Tool trait definitions, ToolContext, and all toolset modules.

pub mod cli;
pub mod config;
pub mod design_review;
pub mod integration;
pub mod library;
pub mod manufacturing;
pub mod pcb_board;
pub mod pcb_components;
pub mod pcb_export;
pub mod pcb_routing;
pub mod project;
pub mod sch_analysis;
pub mod sch_batch;
pub mod sch_bridge;
pub mod sch_components;
pub mod sch_export;
pub mod sch_hierarchy;
pub mod sch_wiring;
pub mod schematic_builder;
pub mod svg_import;
pub mod templates;
pub mod verification;

use crate::mcp::protocol::{CallToolResult, McpToolDescription};
use crate::router::ToolRouter;
use konnect_sexp::writer::SexpEdit;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ─── Tool Handler Type ────────────────────────────────────────────────────────

pub type ToolHandlerFn = Arc<
    dyn Fn(
            &Value,
            Arc<ToolContext>,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<CallToolResult>> + Send>>
        + Send
        + Sync,
>;

// ─── ToolDef ─────────────────────────────────────────────────────────────────

/// A single tool definition: schema + async handler.
#[derive(Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub handler: ToolHandlerFn,
}

impl ToolDef {
    pub fn to_mcp_description(&self) -> McpToolDescription {
        McpToolDescription {
            name: self.name.to_string(),
            description: self.description.to_string(),
            input_schema: self.input_schema.clone(),
        }
    }
}

// Implement Debug manually because handler is not Debug
impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish()
    }
}

// ─── ToolContext ──────────────────────────────────────────────────────────────

/// Shared context passed to every tool handler.
/// Contains config, the tool router, lazily-initialized KiCAD clients, and the
/// per-call observer (used by `get_recent_calls` / `server_stats` meta-tools).
pub struct ToolContext {
    pub config: ServerConfig,
    pub router: Arc<ToolRouter>,
    pub observer: crate::observability::CallObserver,
    /// In-memory TTL cache for repeated JLCPCB parts-database queries.
    pub jlcpcb_cache: QueryCache,
}

impl ToolContext {
    /// Construct a context with an in-memory-only observer (no JSONL). Used by
    /// tests and by callers that don't need persistent call logs.
    pub fn new(config: ServerConfig, router: Arc<ToolRouter>) -> Self {
        ToolContext {
            config,
            router,
            observer: crate::observability::CallObserver::new(None),
            jlcpcb_cache: QueryCache::default(),
        }
    }

    /// Construct a context with a specific observer — wired in by `McpHandler`
    /// so the JSONL log and in-memory ring are shared across all tool calls.
    pub fn new_with_observer(
        config: ServerConfig,
        router: Arc<ToolRouter>,
        observer: crate::observability::CallObserver,
    ) -> Self {
        ToolContext {
            config,
            router,
            observer,
            jlcpcb_cache: QueryCache::default(),
        }
    }
}

// ─── QueryCache ───────────────────────────────────────────────────────────────

/// A small in-memory, TTL-based cache for repeated read-only query results
/// (JSON values keyed by a caller-constructed string). One instance lives on
/// `ToolContext` for the life of the server, shared across all tool calls.
pub struct QueryCache {
    ttl: std::time::Duration,
    entries: std::sync::Mutex<std::collections::HashMap<String, (Value, std::time::Instant)>>,
}

impl QueryCache {
    pub fn new(ttl: std::time::Duration) -> Self {
        QueryCache {
            ttl,
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Returns a cached value for `key` if present and not yet expired.
    pub fn get(&self, key: &str) -> Option<Value> {
        let entries = self.entries.lock().unwrap();
        entries.get(key).and_then(|(value, inserted_at)| {
            if inserted_at.elapsed() < self.ttl {
                Some(value.clone())
            } else {
                None
            }
        })
    }

    /// Stores `value` under `key`, overwriting any existing (possibly expired) entry.
    pub fn put(&self, key: String, value: Value) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(key, (value, std::time::Instant::now()));
    }
}

impl Default for QueryCache {
    /// 5-minute TTL — long enough to skip redundant re-queries within a single
    /// design session, short enough that a `download_jlcpcb_database` refresh
    /// is reflected without needing an explicit cache-invalidation hook.
    fn default() -> Self {
        QueryCache::new(std::time::Duration::from_secs(300))
    }
}

// ─── ServerConfig ─────────────────────────────────────────────────────────────

/// Subset of the server configuration relevant to tool execution.
/// This is the config that flows from `konnect::Config` into the core crate.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub kicad_cli: String,
    pub kicad_binary: String,
    pub ipc_address: String,
    pub project_dir: Option<std::path::PathBuf>,
    pub jlcpcb_db_path: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod query_cache_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn miss_on_unknown_key() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn put_then_get_roundtrips() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        cache.put("key".to_string(), json!({ "count": 3 }));
        assert_eq!(cache.get("key"), Some(json!({ "count": 3 })));
    }

    #[test]
    fn entry_expires_after_ttl() {
        let cache = QueryCache::new(std::time::Duration::from_millis(10));
        cache.put("key".to_string(), json!("value"));
        assert_eq!(cache.get("key"), Some(json!("value")));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(cache.get("key").is_none());
    }

    #[test]
    fn put_overwrites_existing_entry() {
        let cache = QueryCache::new(std::time::Duration::from_secs(60));
        cache.put("key".to_string(), json!("first"));
        cache.put("key".to_string(), json!("second"));
        assert_eq!(cache.get("key"), Some(json!("second")));
    }
}

// ─── Helper macro for defining tools ─────────────────────────────────────────

/// Shorthand for building a ToolDef with a typed async handler function.
///
/// Usage:
/// ```rust,ignore
/// tool!(
///     "tool_name",
///     "Description of what it does.",
///     json_schema,        // serde_json::Value
///     |args, ctx| async move {
///         // handler body
///         Ok(CallToolResult::text("done"))
///     }
/// )
/// ```
#[macro_export]
macro_rules! tool {
    ($name:expr, $desc:expr, $schema:expr, $handler:expr) => {{
        let h: $crate::tools::ToolHandlerFn = std::sync::Arc::new(move |args, ctx| {
            let args = args.clone();
            let ctx = ctx.clone();
            Box::pin(async move { ($handler)(&args, &*ctx).await })
        });
        $crate::tools::ToolDef {
            name: $name,
            description: $desc,
            input_schema: $schema,
            handler: h,
        }
    }};
}

// ─── Argument helpers ─────────────────────────────────────────────────────────

/// Build a structured `InvalidArgument` CallToolResult. Used by the
/// `require_*` helpers so every handler that uses them emits structured
/// errors the client / observer can match on — no per-handler change needed.
fn invalid_arg(field: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        crate::mcp::error::ToolErrorKind::InvalidArgument {
            field: field.to_string(),
            reason: reason.to_string(),
        },
        format!("Argument '{}' is invalid: {}", field, reason),
    )
}

/// Extract a required string argument, returning a structured
/// `InvalidArgument` error result if missing or not a string.
pub fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, CallToolResult> {
    args[key]
        .as_str()
        .ok_or_else(|| invalid_arg(key, "missing or not a string"))
}

/// Extract an optional string argument.
pub fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args[key].as_str()
}

/// Extract a required f64 argument. Returns a structured `InvalidArgument`
/// error result if missing or not a number.
pub fn require_f64(args: &Value, key: &str) -> Result<f64, CallToolResult> {
    args[key]
        .as_f64()
        .ok_or_else(|| invalid_arg(key, "missing or not a number"))
}

/// Extract an optional f64.
pub fn opt_f64(args: &Value, key: &str) -> Option<f64> {
    args[key].as_f64()
}

/// Extract a required path string and return it as a PathBuf, using
/// `anyhow::Error`. Use this variant with `?` inside handlers that return
/// `anyhow::Result`. The surrounding dispatch will stringify the error and
/// surface it as `ToolErrorKind::HandlerError`.
pub fn get_path(args: &Value, key: &str) -> anyhow::Result<std::path::PathBuf> {
    let s = args[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: '{}'", key))?;
    Ok(std::path::PathBuf::from(s))
}

/// Project name used in symbol/sheet `(instances (project "..." ...))` entries:
/// the schematic's file stem, matching what eeschema writes when it saves a
/// standalone root sheet.
pub fn project_name_for(sch_path: &std::path::Path) -> String {
    sch_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Minimal valid blank schematic, with a freshly generated root `(uuid ...)`.
/// The root UUID is mandatory: KiCAD's netlister resolves symbol instance
/// paths against it and silently forms no wire-only nets when it's missing.
pub fn blank_schematic_template() -> String {
    format!(
        "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(uuid \"{}\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n",
        konnect_sexp::writer::new_uuid()
    )
}

/// Root UUID of a loaded schematic, assigning a fresh one when the file
/// predates Konnect writing root UUIDs — the file is repaired on its next
/// overwrite. Instance paths are built as "/<root-uuid>[/<sheet-uuid>…]".
pub fn ensure_root_uuid(sch: &mut konnect_schematic_editor::Schematic) -> String {
    match &sch.uuid {
        Some(u) => u.clone(),
        None => {
            let u = konnect_sexp::writer::new_uuid();
            sch.uuid = Some(u.clone());
            u
        }
    }
}

// ─── Schematic text helpers ──────────────────────────────────────────────────

/// Byte range of the placed `(symbol …)` block whose Reference property is
/// `reference`, for the text-editing tool paths.
///
/// Works regardless of indentation — eeschema saves with tabs, this crate's
/// writer uses two spaces — and skips library definitions inside `lib_symbols`,
/// which carry a Reference property of their own (`"R"`, `"#PWR"`, or whatever
/// a hand-authored library sets) but never a `lib_id`. Only placed instances
/// have one, so that's the discriminator.
pub fn find_symbol_instance_block(content: &str, reference: &str) -> Option<(usize, usize)> {
    let ref_search = format!(r#"(property "Reference" "{reference}""#);
    let mut from = 0usize;

    while let Some(rel) = content[from..].find(&ref_search) {
        let ref_pos = from + rel;
        if let Some((start, end)) =
            konnect_sexp::writer::find_enclosing_block(content, "symbol", ref_pos)
        {
            if content[start..end].contains("(lib_id ") {
                return Some((start, end));
            }
        }
        from = ref_pos + ref_search.len();
    }
    None
}

/// How a rename landed: how many `(symbol …)` blocks were renamed (a multi-unit
/// part has one per unit) and how many instance entries were rewritten.
pub struct RenameOutcome {
    pub units: usize,
    pub instances: usize,
}

/// Byte edits that rename a component, updating **both** places KiCAD 6+ stores
/// a designator.
///
/// A designator lives in the `Reference` property (what eeschema draws) *and*
/// in the per-sheet instance entry
/// `(instances (project … (path … (reference "…") (unit N))))` — and it is the
/// instance entry that the netlister and "Update PCB from Schematic" read.
/// Rewriting only the property leaves the two disagreeing, so PCB sync keeps
/// failing on the *old* designator.
///
/// Returns edits rather than new content so a batch handler can fold them in
/// with its other `SexpEdit`s and apply everything in one pass. Every offset is
/// relative to the `content` passed in: the scan walks forward past each symbol
/// block it has handled instead of re-searching rewritten text, so multi-unit
/// parts — which repeat the designator across one block per unit — are renamed
/// completely without invalidating the offsets already collected.
///
/// All block matching is indentation-agnostic: eeschema/KiCAD 10 writes tabs
/// while this crate's writer writes two spaces, so no matcher may assume either.
pub fn rename_symbol_edits(
    content: &str,
    old: &str,
    new: &str,
) -> Result<(Vec<SexpEdit>, RenameOutcome), String> {
    use konnect_sexp::writer::{find_balanced_block, find_block_starts};

    let mut edits = Vec::new();
    let mut outcome = RenameOutcome {
        units: 0,
        instances: 0,
    };
    let mut cursor = 0usize;

    while let Some((rel_start, rel_end)) = find_symbol_instance_block(&content[cursor..], old) {
        let (sym_start, sym_end) = (cursor + rel_start, cursor + rel_end);
        cursor = sym_end;
        let sym_block = &content[sym_start..sym_end];

        // 1. The Reference property.
        let field_search = r#"(property "Reference" ""#;
        let val_start = sym_block
            .find(field_search)
            .map(|o| sym_start + o + field_search.len())
            .ok_or_else(|| format!("'{old}' has no 'Reference' property"))?;
        let val_end = content[val_start..]
            .find('"')
            .map(|o| val_start + o)
            .ok_or_else(|| format!("'Reference' property on '{old}' is malformed"))?;
        edits.push(SexpEdit::replace(val_start, val_end, new.to_string()));

        // 2. Every (reference "…") inside this same symbol's instances block.
        //    Hand-authored or pre-KiCAD-6 symbols carry none; that is not an
        //    error, just nothing to keep in sync.
        if let Some(&inst_rel) = find_block_starts(sym_block, "instances").first() {
            let (inst_start, inst_end) = find_balanced_block(sym_block, inst_rel)
                .ok_or_else(|| format!("'{old}' has a malformed (instances …) block"))?;
            let inst_block = &sym_block[inst_start..inst_end];
            let inst_abs = sym_start + inst_start;

            for rel in find_block_starts(inst_block, "reference") {
                let rest = &inst_block[rel..];
                let Some(q_open) = rest.find('"') else {
                    continue;
                };
                let q_len = rest[q_open + 1..]
                    .find('"')
                    .ok_or_else(|| format!("'{old}' has a malformed instance (reference …)"))?;
                let start = inst_abs + rel + q_open + 1;
                edits.push(SexpEdit::replace(start, start + q_len, new.to_string()));
                outcome.instances += 1;
            }
        }

        outcome.units += 1;
    }

    if outcome.units == 0 {
        return Err(format!("symbol '{old}' not found in this schematic"));
    }
    Ok((edits, outcome))
}

#[cfg(test)]
mod symbol_block_tests {
    use super::*;

    /// Instance blocks as eeschema writes them: tab-indented, and preceded by a
    /// lib_symbols definition carrying its own Reference property.
    const EESCHEMA_STYLE: &str = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n\t\t\t(property \"Reference\" \"R\"\n\t\t\t\t(at 2.032 0 90)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 80 0)\n\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 102 78 0)\n\t\t)\n\t\t(property \"Value\" \"10k\"\n\t\t\t(at 102 82 0)\n\t\t)\n\t)\n)\n";

    /// Same shape, two-space indented, as this crate's writer emits.
    const KONNECT_STYLE: &str = "(kicad_sch\n  (lib_symbols\n    (symbol \"Device:R\"\n      (property \"Reference\" \"R\"\n        (at 2.032 0 90)\n      )\n    )\n  )\n  (symbol\n    (lib_id \"Device:R\")\n    (at 100 80 0)\n    (property \"Reference\" \"R1\"\n      (at 102 78 0)\n    )\n  )\n)\n";

    #[test]
    fn finds_instance_in_tab_indented_file() {
        let (start, end) = find_symbol_instance_block(EESCHEMA_STYLE, "R1").expect("R1 block");
        let block = &EESCHEMA_STYLE[start..end];
        assert!(block.starts_with("(symbol"));
        assert!(block.contains("(lib_id \"Device:R\")"));
        assert!(block.contains("\"R1\""));
        assert!(
            block.contains("\"10k\""),
            "block must span the whole symbol"
        );
    }

    #[test]
    fn finds_instance_in_space_indented_file() {
        let (start, end) = find_symbol_instance_block(KONNECT_STYLE, "R1").expect("R1 block");
        assert!(KONNECT_STYLE[start..end].contains("(lib_id \"Device:R\")"));
    }

    #[test]
    fn library_definition_is_not_mistaken_for_an_instance() {
        // A hand-authored library whose default Reference matches a placed
        // instance's designator must not shadow the instance.
        let sch = "(kicad_sch\n\t(lib_symbols\n\t\t(symbol \"Custom:Thing\"\n\t\t\t(property \"Reference\" \"U1\"\n\t\t\t\t(at 0 0 0)\n\t\t\t)\n\t\t)\n\t)\n\t(symbol\n\t\t(lib_id \"Custom:Thing\")\n\t\t(property \"Reference\" \"U1\"\n\t\t\t(at 5 5 0)\n\t\t)\n\t)\n)\n";
        let (start, end) = find_symbol_instance_block(sch, "U1").expect("instance");
        assert!(
            sch[start..end].contains("(lib_id "),
            "must skip the lib_symbols definition and return the placed instance"
        );
    }

    #[test]
    fn unknown_reference_is_none() {
        assert!(find_symbol_instance_block(EESCHEMA_STYLE, "R99").is_none());
    }

    #[test]
    fn reference_prefix_does_not_match_longer_designator() {
        // "R1" must not match the R12 instance.
        let sch = "(kicad_sch\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(property \"Reference\" \"R12\"\n\t\t\t(at 1 1 0)\n\t\t)\n\t)\n)\n";
        assert!(find_symbol_instance_block(sch, "R1").is_none());
    }
}

#[cfg(test)]
mod arg_helper_tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use serde_json::json;

    #[test]
    fn require_str_missing_produces_structured_invalid_argument() {
        let args = json!({});
        let err = require_str(&args, "path").expect_err("should fail");
        assert!(err.is_error);
        assert_eq!(
            extract_error_kind(&err).as_deref(),
            Some("invalid_argument")
        );
        // The body carries the field name so clients can branch.
        let body = match &err.content[0] {
            crate::mcp::protocol::ToolContent::Text { text } => text.clone(),
            _ => panic!(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["error"]["field"], "path");
    }

    #[test]
    fn require_f64_non_number_produces_structured_invalid_argument() {
        let args = json!({ "x": "not a number" });
        let err = require_f64(&args, "x").expect_err("should fail");
        assert_eq!(
            extract_error_kind(&err).as_deref(),
            Some("invalid_argument")
        );
    }

    #[test]
    fn require_str_present_returns_value() {
        let args = json!({ "name": "ok" });
        let v = require_str(&args, "name").expect("should parse");
        assert_eq!(v, "ok");
    }
}

// ─── KiCAD config directory detection ────────────────────────────────────────

/// Find the KiCAD user config directory by probing for installed version directories.
/// Checks versions in descending order: 10.0, 9.0, 8.0, then bare "kicad".
pub fn kicad_config_dir() -> std::path::PathBuf {
    let base = kicad_config_base();
    let versions = ["10.0", "9.0", "8.0"];
    for ver in &versions {
        let dir = base.join(ver);
        if dir.is_dir() {
            return dir;
        }
    }
    // Fallback: bare kicad dir or 10.0 (will be created on first use)
    base.join("10.0")
}

/// Platform-specific base directory for KiCAD configs.
fn kicad_config_base() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        std::path::PathBuf::from(appdata).join("kicad")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Preferences")
            .join("kicad")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        std::path::PathBuf::from(home).join(".config").join("kicad")
    }
}

// ─── KiCAD symbol library resolution ────────────────────────────────────────

/// Resolve a lib_id like "Device:R" to the full symbol S-expression definition.
/// KiCAD 10 stores symbols in .kicad_symdir directories, one .kicad_sym file per symbol.
/// Returns the symbol block with the lib_id prefix (e.g. "Device:R") as the symbol name.
/// Delegates to `konnect_schematic_editor::library`, which is the single
/// implementation.
///
/// This used to be a second, independently-maintained copy that had drifted:
/// it counted parens without skipping quoted strings (so any symbol with an
/// unbalanced paren in a property value failed to resolve), never consulted the
/// symbol library tables, and — unlike the real implementation — did not prefix
/// or embed `(extends "Parent")` parents, so `replace_component` wrote derived
/// symbols KiCAD could not resolve while `add_schematic_component` handled them
/// correctly.
pub fn resolve_lib_symbol(lib_id: &str) -> Option<String> {
    let resolved = konnect_schematic_editor::library::resolve_lib_symbol(lib_id);
    if resolved.is_none() {
        tracing::warn!("Symbol '{}' not found in any symbol library", lib_id);
    }
    resolved
}

/// Structured "this lib_id doesn't exist" error, with did-you-mean hints —
/// silently accepting an unresolvable lib_id writes a netlist-invisible
/// component with an empty pin list (#34).
pub fn lib_symbol_not_found_error(lib_id: &str) -> CallToolResult {
    let library = lib_id.split(':').next().unwrap_or(lib_id);
    let mut msg = if !konnect_schematic_editor::library::library_exists(library) {
        format!(
            "Library '{}' not found in the installed KiCAD symbol libraries \
             (lib_id '{}'). Check the library name, the KiCAD install, or \
             KICAD10_SYMBOL_DIR.",
            library, lib_id
        )
    } else {
        format!(
            "Library symbol '{}' not found in the installed KiCAD libraries.",
            lib_id
        )
    };
    let suggestions = konnect_schematic_editor::library::suggest_symbols(lib_id, 3);
    if !suggestions.is_empty() {
        msg.push_str(&format!(
            " Did you mean: {}? (KiCAD 10 renamed several older symbol names)",
            suggestions.join(", ")
        ));
    }
    CallToolResult::error(msg)
}

/// Insert a symbol definition into the schematic's lib_symbols section.
/// Creates the lib_symbols section if it doesn't exist. Skips if already present.
///
/// Returns `false` when `lib_id` cannot be resolved — callers must surface
/// that as an error rather than writing a definition-less instance (#34).
#[must_use]
pub fn ensure_lib_symbol_in_schematic(content: &mut String, lib_id: &str) -> bool {
    // Check if already present
    let lib_id_check = format!("(symbol \"{}\"", lib_id);
    if content.contains(&lib_id_check) {
        return true;
    }

    // Resolve the symbol from KiCAD libraries
    let sym_def = match resolve_lib_symbol(lib_id) {
        Some(s) => s,
        None => return false,
    };

    // Ensure lib_symbols section exists
    if !content.contains("(lib_symbols") {
        if let Some(insert_after) = content.find(")\n") {
            content.insert_str(insert_after + 2, "\n\t(lib_symbols\n\t)\n");
        }
    }

    // Find the closing paren of lib_symbols and insert before it
    if let Some(ls_start) = content.find("(lib_symbols") {
        let mut depth = 0i32;
        let mut ls_end = ls_start;
        for (i, ch) in content[ls_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        ls_end = ls_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let indented = sym_def
            .lines()
            .map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("\t\t{}", l)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        content.insert_str(ls_end, &format!("\n{}\n\t", indented));
    }
    true
}

// ─── Net references in .kicad_pcb ────────────────────────────────────────────

/// How a board refers to a net.
///
/// KiCAD changed this between format versions: files up to ~20250513 declare a
/// numbered table (`(net 1 "GND")`) and reference the number, while 20260206
/// dropped the table entirely and references nets by name (`(net "GND")`).
/// Writing the wrong form — or worse, `(net 0)` — produces copper that belongs
/// to no net: a ground pour that is not connected to ground, which DRC does not
/// flag because isolated copper is legal.
#[derive(Debug, Clone, PartialEq)]
pub enum NetRef {
    /// `(net "GND")` — KiCAD 10's current format.
    Named(String),
    /// `(net 1)` + `(net_name "GND")` — the older numbered table.
    Numbered(i32, String),
}

impl NetRef {
    /// The net fields to write inside a `(zone …)`, in this board's format.
    pub fn zone_fields(&self) -> String {
        match self {
            NetRef::Named(name) => format!("(net \"{name}\")"),
            NetRef::Numbered(id, name) => format!("(net {id}) (net_name \"{name}\")"),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            NetRef::Named(n) | NetRef::Numbered(_, n) => n,
        }
    }

    /// The numeric code, where the board still has one.
    pub fn code(&self) -> Option<i32> {
        match self {
            NetRef::Numbered(id, _) => Some(*id),
            NetRef::Named(_) => None,
        }
    }
}

/// Resolve `net_name` against the nets actually present on `content`.
///
/// Returns `None` when the board has no such net. Callers MUST surface that as
/// an error: the previous behaviour was to fall back to net 0, which writes a
/// pour connected to nothing and is invisible until the board is fabricated.
pub fn resolve_net(content: &str, net_name: &str) -> Option<NetRef> {
    let quoted = format!("\"{net_name}\"");

    // Numbered form first: `(net <digits> "NAME")`.
    let b = content.as_bytes();
    for start in konnect_sexp::writer::find_block_starts(content, "net") {
        let rest = &content[start + "(net".len()..];
        let trimmed = rest.trim_start();
        let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        let after = trimmed[digits.len()..].trim_start();
        if after.starts_with(&quoted) {
            if let Ok(id) = digits.parse::<i32>() {
                return Some(NetRef::Numbered(id, net_name.to_string()));
            }
        }
        let _ = b;
    }

    // Named form: `(net "NAME")`.
    for start in konnect_sexp::writer::find_block_starts(content, "net") {
        let rest = &content[start + "(net".len()..];
        if rest.trim_start().starts_with(&quoted) {
            return Some(NetRef::Named(net_name.to_string()));
        }
    }
    None
}

/// The error to return when a caller names a net the board does not have.
pub fn net_not_found_error(content: &str, net_name: &str) -> CallToolResult {
    let mut known: Vec<String> = konnect_sexp::writer::find_block_starts(content, "net")
        .into_iter()
        .filter_map(|s| {
            let rest = content[s + "(net".len()..].trim_start();
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start();
            let inner = rest.strip_prefix('"')?;
            let end = inner.find('"')?;
            Some(inner[..end].to_string()).filter(|n| !n.is_empty())
        })
        .collect();
    known.sort();
    known.dedup();
    let sample: Vec<&String> = known.iter().take(12).collect();

    CallToolResult::error(format!(
        "Net '{net_name}' does not exist on this board. Refusing to write copper \
         with no net — it would be connected to nothing, which DRC does not flag \
         for a zone. Nets on this board: {}{}",
        sample
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        if known.len() > sample.len() {
            format!(" … ({} total)", known.len())
        } else {
            String::new()
        }
    ))
}

#[cfg(test)]
mod net_ref_tests {
    use super::*;

    /// KiCAD 10 (20260206) has no numeric net table at all.
    const NAMED: &str = "(kicad_pcb\n\t(version 20260206)\n\t(footprint \"x\"\n\t\t(pad \"1\" smd rect\n\t\t\t(net \"GND\")\n\t\t)\n\t\t(pad \"2\" smd rect\n\t\t\t(net \"+3V3\")\n\t\t)\n\t)\n)\n";
    /// Older boards declare `(net N "NAME")`.
    const NUMBERED: &str = "(kicad_pcb\n\t(version 20250513)\n\t(net 0 \"\")\n\t(net 1 \"GND\")\n\t(net 2 \"+5V\")\n)\n";

    #[test]
    fn resolves_named_format() {
        assert_eq!(
            resolve_net(NAMED, "GND"),
            Some(NetRef::Named("GND".into()))
        );
        assert_eq!(resolve_net(NAMED, "GND").unwrap().zone_fields(), "(net \"GND\")");
        assert!(resolve_net(NAMED, "GND").unwrap().code().is_none());
    }

    #[test]
    fn resolves_numbered_format() {
        let n = resolve_net(NUMBERED, "GND").expect("GND is net 1");
        assert_eq!(n, NetRef::Numbered(1, "GND".into()));
        assert_eq!(n.zone_fields(), "(net 1) (net_name \"GND\")");
        assert_eq!(resolve_net(NUMBERED, "+5V").unwrap().code(), Some(2));
    }

    /// The bug: an unknown net used to become net 0 — a pour joined to nothing.
    #[test]
    fn unknown_net_is_none_not_zero() {
        assert_eq!(resolve_net(NAMED, "VBUS"), None);
        assert_eq!(resolve_net(NUMBERED, "VBUS"), None);
    }

    #[test]
    fn a_net_name_that_is_a_prefix_of_another_does_not_match() {
        let c = "(kicad_pcb\n\t(pad\n\t\t(net \"GND_ANALOG\")\n\t)\n)\n";
        assert_eq!(resolve_net(c, "GND"), None, "prefix must not match");
        assert!(resolve_net(c, "GND_ANALOG").is_some());
    }
}

// ─── Board file writes, synchronised with a running KiCAD ────────────────────

use serde_json::json;

/// What happened to KiCAD's view of the board around a file write.
#[derive(Debug, Clone, PartialEq)]
pub enum BoardSync {
    /// KiCAD is not running, or has a different board open. Nothing to do.
    NotOpen,
    /// KiCAD had this board open and was told to reload it from disk.
    Reloaded,
    /// KiCAD had this board open but the reload failed; the file on disk is
    /// correct, the editor is showing something older.
    ReloadFailed(String),
}

impl BoardSync {
    pub fn as_json(&self) -> Value {
        match self {
            BoardSync::NotOpen => json!("not_open"),
            BoardSync::Reloaded => json!("reloaded"),
            BoardSync::ReloadFailed(e) => json!({ "status": "reload_failed", "error": e }),
        }
    }

    /// A note for the caller when the editor may now be stale.
    pub fn note(&self) -> Option<String> {
        match self {
            BoardSync::ReloadFailed(_) => Some(
                "KiCAD has this board open and could not be reloaded — use File > Revert \
                 in KiCAD, or its in-memory copy will overwrite this change on save."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// Write a `.kicad_pcb` and keep a running KiCAD in step with it.
///
/// The file is authoritative. IPC is used only to stop the editor from holding
/// — and later re-saving — a stale copy of a board this tool just changed:
///
/// 1. If KiCAD has *this* board open, ask it to save first, so anything the
///    user changed by hand is on disk and included in what we edit.
/// 2. Write the file (validated and backed up by `write_atomic_checked`).
/// 3. Ask KiCAD to revert, so the editor reloads what was written.
///
/// When KiCAD is not running, or has a different board open, steps 1 and 3 are
/// skipped and this is a plain validated write — which is why the tools work
/// headless.
pub async fn write_board_synced(
    ipc_address: &str,
    path: &std::path::Path,
    content: &str,
) -> anyhow::Result<BoardSync> {
    let open_here = board_sync_before(ipc_address, path).await;
    konnect_sexp::writer::write_atomic_checked(path, content, "kicad_pcb")?;
    Ok(board_sync_after(ipc_address, open_here).await)
}

/// Step 1 of the protocol: if KiCAD has this exact board open, flush its
/// in-memory edits to disk so they are part of what we are about to change.
/// Returns whether KiCAD had it open.
pub async fn board_sync_before(ipc_address: &str, path: &std::path::Path) -> bool {
    let open_here = crate::tools::pcb_board::ipc_targets_board(ipc_address.to_string(), path).await;
    if open_here {
        let addr = ipc_address.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            konnect_ipc::client::KiCadIpcClient::new(&addr).save_board()
        })
        .await;
    }
    open_here
}

/// Step 3 of the protocol: make KiCAD reload the file that was just written.
///
/// Split out from [`write_board_synced`] so a write performed by something
/// other than us — `kicad-cli --save-board`, for instance — can still leave the
/// editor consistent with disk.
pub async fn board_sync_after(ipc_address: &str, was_open: bool) -> BoardSync {
    if !was_open {
        return BoardSync::NotOpen;
    }
    let addr = ipc_address.to_string();
    let reverted = tokio::task::spawn_blocking(move || {
        let c = konnect_ipc::client::KiCadIpcClient::new(&addr);
        c.revert_board()?;
        let _ = c.refresh_editor();
        Ok::<(), anyhow::Error>(())
    })
    .await;

    match reverted {
        Ok(Ok(())) => BoardSync::Reloaded,
        Ok(Err(e)) => BoardSync::ReloadFailed(e.to_string()),
        Err(e) => BoardSync::ReloadFailed(e.to_string()),
    }
}

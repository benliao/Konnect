//! Text-based S-expression editor for KiCAD files.
//!
//! All modifications are performed as **targeted string edits** on the raw file
//! content rather than full parse → serialize round-trips. This preserves
//! KiCAD's exact formatting and avoids the "single-line collapse" corruption
//! that sexpdata.dumps() caused in the Python backend.
//!
//! # Usage Pattern (all handlers must follow this)
//!
//! ```rust,ignore
//! let content = std::fs::read_to_string(&path)?;
//! let mut edits = Vec::new();
//! edits.push(SexpEdit::insert_before_closing(parent_close_offset, new_sexp));
//! edits.push(SexpEdit::replace_span(start, end, new_value));
//! let new_content = apply_edits(content, edits);
//! write_atomic(&path, &new_content)?;
//! ```
//!
//! Edits **must** be applied in **reverse byte-offset order** so that earlier
//! offsets are not invalidated by later insertions.

use crate::SexpError;
use std::io::Write;
use std::path::Path;

// ─── Edit Types ───────────────────────────────────────────────────────────────

/// A single targeted text edit to apply to file content.
#[derive(Debug, Clone)]
pub struct SexpEdit {
    /// Byte offset where the edit starts.
    pub start: usize,
    /// Byte offset where the edit ends (exclusive). For pure insertions, end == start.
    pub end: usize,
    /// Replacement text (empty string = deletion).
    pub replacement: String,
}

impl SexpEdit {
    /// Insert `text` at the given byte offset (no deletion).
    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        SexpEdit {
            start: offset,
            end: offset,
            replacement: text.into(),
        }
    }

    /// Replace a span of bytes with new text.
    pub fn replace(start: usize, end: usize, text: impl Into<String>) -> Self {
        SexpEdit {
            start,
            end,
            replacement: text.into(),
        }
    }

    /// Delete a span of bytes.
    pub fn delete(start: usize, end: usize) -> Self {
        SexpEdit {
            start,
            end,
            replacement: String::new(),
        }
    }
}

// ─── Apply Edits ─────────────────────────────────────────────────────────────

/// Apply a list of edits to `content` and return the modified string.
///
/// Edits are sorted in **reverse byte-offset order** automatically, so the
/// caller does not need to pre-sort them. This ensures that applying one edit
/// does not invalidate the offsets of subsequent edits.
pub fn apply_edits(mut content: String, mut edits: Vec<SexpEdit>) -> String {
    // Sort by start offset descending
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));

    for edit in edits {
        assert!(edit.start <= edit.end, "Edit start > end");
        assert!(edit.end <= content.len(), "Edit end out of bounds");
        content.replace_range(edit.start..edit.end, &edit.replacement);
    }

    content
}

// ─── Atomic File Write ────────────────────────────────────────────────────────

/// Write `content` to `path` atomically with fsync.
///
/// Writes to a `.tmp` sibling file first, then renames. This prevents
/// corrupted writes if the process is killed mid-write. The KiCAD MCP
/// protocol requires that reads immediately after writes see the new data,
/// so fsync is mandatory.
pub fn write_atomic(path: &Path, content: &str) -> Result<(), SexpError> {
    let tmp_path = path.with_extension("kicad_tmp");

    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
        f.sync_all()?; // fsync — mandatory
    }

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Write a KiCAD document only if it survives a round-trip re-parse.
///
/// Every tool here edits KiCAD files as raw string splices, so a mis-computed
/// offset does not fail loudly — it writes a structurally different file that
/// KiCAD then refuses to open. Re-reading our own output before it replaces the
/// user's file turns that class of bug into a failed tool call instead of an
/// unopenable project.
///
/// `expect_root` is the document's required root tag (`kicad_sch`, `kicad_pcb`).
/// Checks: the text parses, there is exactly one top-level block, its tag is
/// `expect_root`, and parens balance outside of quoted strings.
pub fn write_atomic_checked(
    path: &Path,
    content: &str,
    expect_root: &str,
) -> Result<(), SexpError> {
    check_document(content, expect_root)?;
    write_atomic(path, content)
}

/// The structural checks behind [`write_atomic_checked`], separated so callers
/// can validate a candidate edit before deciding what to do about it.
pub fn check_document(content: &str, expect_root: &str) -> Result<(), SexpError> {
    // Paren balance, ignoring anything inside quoted strings.
    let bytes = content.as_bytes();
    let (mut depth, mut i) = (0i64, 0usize);
    let mut closed_at = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'(' => {
                if depth == 0 && closed_at.is_some() {
                    return Err(SexpError::InvalidValue(
                        "document has more than one top-level block".into(),
                    ));
                }
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(SexpError::InvalidValue(format!(
                        "unbalanced ')' at byte {i}"
                    )));
                }
                if depth == 0 {
                    closed_at = Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    if depth != 0 {
        return Err(SexpError::InvalidValue(format!(
            "{depth} unclosed '(' at end of document"
        )));
    }
    if closed_at.is_none() {
        return Err(SexpError::InvalidValue("document is empty".into()));
    }

    let tree = crate::parser::parse_sexp(content)?;
    match tree.head() {
        Some(t) if t == expect_root => Ok(()),
        Some(t) => Err(SexpError::InvalidValue(format!(
            "root tag is '{t}', expected '{expect_root}'"
        ))),
        None => Err(SexpError::InvalidValue(format!(
            "document has no '{expect_root}' root tag"
        ))),
    }
}

// ─── Balanced-Paren Block Finder ─────────────────────────────────────────────

/// Find the byte range of the balanced-paren S-expression block starting at
/// `start_offset` in `content`. Returns `(block_start, block_end)` where
/// `content[block_start..block_end]` is the complete `(...)` block.
///
/// Used to delete entire symbol/wire/label blocks.
pub fn find_balanced_block(content: &str, start_offset: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = start_offset;

    // Skip to opening paren
    while i < bytes.len() && bytes[i] != b'(' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }

    let block_start = i;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape_next = false;

    while i < bytes.len() {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
        } else if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((block_start, i + 1));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    None // Unbalanced
}

/// Find the byte range of a block plus any leading whitespace/newline,
/// so deletion leaves clean formatting.
pub fn find_block_with_leading_whitespace(
    content: &str,
    start_offset: usize,
) -> Option<(usize, usize)> {
    let (block_start, block_end) = find_balanced_block(content, start_offset)?;

    // Walk backwards from block_start to consume leading whitespace
    let bytes = content.as_bytes();
    let mut ws_start = block_start;
    while ws_start > 0 && (bytes[ws_start - 1] == b' ' || bytes[ws_start - 1] == b'\t') {
        ws_start -= 1;
    }
    // Also consume a preceding newline if present
    if ws_start > 0 && bytes[ws_start - 1] == b'\n' {
        ws_start -= 1;
        if ws_start > 0 && bytes[ws_start - 1] == b'\r' {
            ws_start -= 1;
        }
    }

    Some((ws_start, block_end))
}

/// Byte offsets of every `(tag …)` block opening in `content`, at any
/// indentation and nesting depth.
///
/// Matches whole tags only — `find_block_starts(c, "symbol")` will not match
/// `(symbol_instances` — and skips matches inside quoted strings, so a property
/// value like `"(label foo)"` is never mistaken for a block.
///
/// Prefer this over `rfind("\n  (tag")`: KiCAD's own writers indent with tabs
/// while this crate's writer uses two spaces, so a fixed-width literal silently
/// finds nothing in eeschema-saved files.
pub fn find_block_starts(content: &str, tag: &str) -> Vec<usize> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut in_string = false;
    let mut escape_next = false;

    for i in 0..bytes.len() {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
        } else if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == b'(' && content[i + 1..].starts_with(tag) {
            // The tag must be followed by a delimiter, not more identifier
            // characters: `(symbol` must not match inside `(symbol_instances`.
            let after = bytes.get(i + 1 + tag.len()).copied();
            let delimited = matches!(
                after,
                None | Some(b' ')
                    | Some(b'\t')
                    | Some(b'\n')
                    | Some(b'\r')
                    | Some(b'(')
                    | Some(b')')
            );
            if delimited {
                out.push(i);
            }
        }
    }
    out
}

/// Byte range of the innermost `(tag …)` block enclosing `pos`.
///
/// Indentation-agnostic; returns `(block_start, block_end)` where
/// `content[block_start..block_end]` is the complete block.
pub fn find_enclosing_block(content: &str, tag: &str, pos: usize) -> Option<(usize, usize)> {
    find_block_starts(content, tag)
        .into_iter()
        .rev()
        .filter(|&start| start <= pos)
        .find_map(|start| find_balanced_block(content, start).filter(|&(_, end)| end > pos))
}

/// Byte range of the *top-level* item enclosing `pos` — the block one level
/// inside the document root, e.g. the `(symbol …)` or `(wire …)` that owns a
/// nested `(uuid …)`.
///
/// Indentation-agnostic, unlike walking back to a literal `"\n  ("`: KiCAD
/// indents with tabs, so a fixed-width literal finds nothing in eeschema-saved
/// files. Returns `None` if `pos` is outside the root block or is the root's
/// own direct content.
pub fn find_top_level_item(content: &str, pos: usize) -> Option<(usize, usize)> {
    let bytes = content.as_bytes();
    let (mut depth, mut i) = (0usize, 0usize);
    let mut in_string = false;
    let mut escape_next = false;
    // The most recent depth-1 → depth-2 opening seen before `pos`.
    let mut candidate = None;

    while i < bytes.len() && i <= pos {
        let b = bytes[i];
        if escape_next {
            escape_next = false;
        } else if in_string {
            if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'(' => {
                    // depth 1 is the document root, so its direct children —
                    // the top-level items — open while depth == 1.
                    if depth == 1 {
                        candidate = Some(i);
                    }
                    depth += 1;
                }
                b')' => {
                    depth = depth.saturating_sub(1);
                    // Left the candidate item without reaching `pos`; it does
                    // not enclose it after all.
                    if depth <= 1 {
                        candidate = None;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    candidate.and_then(|start| find_balanced_block(content, start))
}

// ─── UUID Generation ─────────────────────────────────────────────────────────

/// Generate a new KiCAD-compatible UUID string.
/// KiCAD 9+ requires UUIDs to be quoted in S-expressions: `(uuid "abc-123")`.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_reverse_order() {
        let content = "hello world".to_string();
        let edits = vec![
            SexpEdit::insert(5, " beautiful"),
            SexpEdit::replace(0, 5, "goodbye"),
        ];
        let result = apply_edits(content, edits);
        assert_eq!(result, "goodbye beautiful world");
    }

    #[test]
    fn find_balanced_block_simple() {
        let content = "  (wire (start 1 2) (end 3 4))  ";
        let (s, e) = find_balanced_block(content, 0).unwrap();
        assert_eq!(&content[s..e], "(wire (start 1 2) (end 3 4))");
    }

    #[test]
    fn find_balanced_block_nested() {
        let content = r#"(symbol "U1" (at 10 20 0) (property "Value" "STM32"))"#;
        let (s, e) = find_balanced_block(content, 0).unwrap();
        assert_eq!(&content[s..e], content);
    }

    #[test]
    fn find_balanced_block_quoted_paren() {
        // Parens inside strings must not affect depth count
        let content = r#"(text "hello (world)") "#;
        let (s, e) = find_balanced_block(content, 0).unwrap();
        assert_eq!(&content[s..e], r#"(text "hello (world)")"#);
    }
}

#[cfg(test)]
mod block_start_tests {
    use super::*;

    /// The two indentation styles a .kicad_sch can arrive in: eeschema saves
    /// with tabs, this crate's writer emits two spaces.
    const TABS: &str = "(kicad_sch\n\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(property \"Reference\" \"R1\")\n\t)\n)";
    const SPACES: &str = "(kicad_sch\n  (symbol\n    (lib_id \"Device:R\")\n    (property \"Reference\" \"R1\")\n  )\n)";

    #[test]
    fn finds_block_starts_at_any_indentation() {
        for (label, content) in [("tabs", TABS), ("spaces", SPACES)] {
            let starts = find_block_starts(content, "symbol");
            assert_eq!(starts.len(), 1, "{label}");
            assert!(content[starts[0]..].starts_with("(symbol"), "{label}");
        }
    }

    #[test]
    fn tag_match_requires_a_delimiter() {
        // `(symbol` must not match inside `(symbol_instances`.
        let content = "(root (symbol_instances (path \"/\")) (symbol (lib_id \"x\")))";
        let starts = find_block_starts(content, "symbol");
        assert_eq!(starts.len(), 1);
        assert!(content[starts[0]..].starts_with("(symbol (lib_id"));
    }

    #[test]
    fn matches_inside_quoted_strings_are_ignored() {
        let content = "(root (property \"Note\" \"see (symbol foo)\") (symbol (lib_id \"x\")))";
        let starts = find_block_starts(content, "symbol");
        assert_eq!(starts.len(), 1, "the quoted '(symbol' is data, not a block");
        assert!(content[starts[0]..].starts_with("(symbol (lib_id"));
    }

    #[test]
    fn enclosing_block_is_the_innermost_match() {
        let content =
            "(kicad_sch (lib_symbols (symbol \"Device:R\" (symbol \"R_1_1\" (pin HERE)))))";
        let pos = content.find("HERE").unwrap();
        let (start, end) = find_enclosing_block(content, "symbol", pos).unwrap();
        assert!(
            content[start..end].starts_with("(symbol \"R_1_1\""),
            "expected the innermost enclosing symbol, got {}",
            &content[start..start + 20]
        );
        assert!(end > pos);
    }

    #[test]
    fn enclosing_block_spans_the_whole_block_from_tab_indented_input() {
        let pos = TABS.find("\"R1\"").unwrap();
        let (start, end) = find_enclosing_block(TABS, "symbol", pos).unwrap();
        assert!(TABS[start..end].starts_with("(symbol"));
        assert!(TABS[start..end].contains("(lib_id \"Device:R\")"));
        assert!(TABS[start..end].ends_with(')'));
    }

    #[test]
    fn no_enclosing_block_when_position_is_outside() {
        // Position before any symbol block.
        assert!(find_enclosing_block(TABS, "symbol", 2).is_none());
        // Tag that isn't present at all.
        let pos = TABS.find("\"R1\"").unwrap();
        assert!(find_enclosing_block(TABS, "wire", pos).is_none());
    }

    #[test]
    fn check_document_rejects_the_add_layer_corruption() {
        // The exact shape add_layer produced (copied from a real corrupted
        // board): the new rows were spliced in before F.Cu's closing paren, so
        // they became its children.
        let nested = "(kicad_pcb\n\t(layers\n\t\t(0 \"F.Cu\" signal\n    (1 \"In1.Cu\" power\n    (1 \"In2.Cu\" power)))\n\t\t(2 \"B.Cu\" signal)\n\t)\n)\n";
        // Note this text IS paren-balanced with a single correct root — the
        // nesting is structural, not lexical. So the cheap document check
        // cannot catch it, which is exactly why add_layer additionally
        // validates the layer table (flat rows, unique ids, unique names).
        assert!(
            check_document(nested, "kicad_pcb").is_ok(),
            "the corruption balances — document-level checks alone are not enough"
        );

        // The checks that DO fire:
        assert!(check_document("(kicad_pcb\n\t(layers\n\t)\n", "kicad_pcb").is_err(), "unclosed paren");
        assert!(check_document("(kicad_pcb)\n(kicad_pcb)\n", "kicad_pcb").is_err(), "two roots");
        assert!(check_document("(kicad_sch)\n", "kicad_pcb").is_err(), "wrong root tag");
        assert!(check_document("", "kicad_pcb").is_err(), "empty");
        assert!(check_document("(kicad_pcb))\n", "kicad_pcb").is_err(), "extra close");
    }

    #[test]
    fn check_document_ignores_parens_inside_strings() {
        // A property value with an unmatched paren must not look unbalanced.
        let doc = "(kicad_sch\n\t(property \"D\" \"scheme (pin number consists of\")\n)\n";
        assert!(check_document(doc, "kicad_sch").is_ok(), "string contents are data");
    }

    #[test]
    fn check_document_accepts_a_real_tab_indented_document() {
        let doc = "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n)\n";
        assert!(check_document(doc, "kicad_sch").is_ok());
    }

    /// A nested `(uuid …)` must resolve to the whole top-level item that owns
    /// it, whichever way the file is indented.
    #[test]
    fn top_level_item_found_from_a_nested_position_at_any_indentation() {
        for (label, content) in [("tabs", TABS), ("spaces", SPACES)] {
            let pos = content.find("Reference").unwrap();
            let (s, e) = find_top_level_item(content, pos)
                .unwrap_or_else(|| panic!("{label}: no enclosing top-level item"));
            assert!(
                content[s..e].starts_with("(symbol"),
                "{label}: {}",
                &content[s..e]
            );
            assert!(content[s..e].ends_with(')'), "{label}: block not balanced");
            // The whole symbol, not just the inner property.
            assert!(content[s..e].contains("lib_id"), "{label}: block truncated");
        }
    }

    #[test]
    fn top_level_item_picks_the_owning_sibling_not_a_previous_one() {
        let doc =
            "(kicad_sch\n\t(symbol\n\t\t(uuid \"aaa\")\n\t)\n\t(wire\n\t\t(uuid \"bbb\")\n\t)\n)";
        let pos = doc.find("bbb").unwrap();
        let (s, e) = find_top_level_item(doc, pos).unwrap();
        assert!(doc[s..e].starts_with("(wire"), "{}", &doc[s..e]);
        assert!(
            !doc[s..e].contains("aaa"),
            "leaked into the previous sibling"
        );
    }

    /// Content belonging to the root itself has no enclosing top-level item,
    /// and neither does an offset past the end of the document.
    #[test]
    fn top_level_item_declines_root_level_and_out_of_range_positions() {
        let doc = "(kicad_sch\n\t(version 20250610)\n)";
        assert_eq!(
            find_top_level_item(doc, doc.find("kicad_sch").unwrap()),
            None
        );
        assert_eq!(find_top_level_item(doc, doc.len() + 50), None);
    }

    /// A paren inside a quoted string must not be counted as nesting, or the
    /// depth tracking drifts and the wrong block gets returned.
    #[test]
    fn top_level_item_ignores_parens_inside_strings() {
        let doc = "(kicad_sch\n\t(text \"a (b\")\n\t(symbol\n\t\t(uuid \"zzz\")\n\t)\n)";
        let pos = doc.find("zzz").unwrap();
        let (s, e) = find_top_level_item(doc, pos).unwrap();
        assert!(doc[s..e].starts_with("(symbol"), "{}", &doc[s..e]);
    }
}

//! `pcb_components` toolset — query, move, rotate, edit, align, and delete
//! footprints on the PCB.
//!
//! Everything that changes a footprint already on the board edits the
//! `.kicad_pcb` file directly, like `pcb_routing` and `pcb_export`. The toolset
//! used to run on the KiCAD IPC API, which meant a session mixing component and
//! routing tools had KiCAD's in-memory board and the file on disk disagreeing —
//! whichever saved last silently won. One backend means the `board` argument
//! always names the file that is actually changed, and the tools work headless.
//!
//! Writes go through `crate::tools::write_board_synced`, so a KiCAD that has
//! this exact board open saves before and reloads after; with no KiCAD (or a
//! different board open) it is a plain validated write.
//!
//! Still on IPC: `place_component`, `place_component_array` and
//! `duplicate_component`. Creating a footprint from nothing needs the library
//! resolved (fp-lib-table → `.pretty` → `.kicad_mod` → a board `(footprint …)`
//! block), which is a subsystem of its own; in a normal workflow footprints
//! arrive from schematic→PCB sync rather than being placed one at a time.
//! `get_board_2d_view` shells out to kicad-cli and never used IPC.
//!
//! ## The coordinate rule, as measured
//!
//! A footprint's children — `(property …)`, `(pad …)`, `(fp_text …)`, the
//! `fp_line`/`fp_poly` graphics — hold coordinates **relative to the footprint
//! origin**, unlike a schematic symbol's fields, which are absolute. The angle
//! in a child's `(at x y angle)` is the other way round: it is in **board**
//! space, so it turns with the footprint while its x/y stay put.
//!
//! Measured against KiCAD's own saves rather than derived (see the module
//! tests, which encode the same rule):
//!
//! * 67 consecutive KiCAD-written revisions of a real 20260206 board
//!   (`ODB2.kicad_pcb`) contain 481 footprint moves. A raw diff of a move shows
//!   **exactly one changed line** — the footprint's own `(at …)`. Across all of
//!   them 3904 child `(at …)` x/y were unchanged and **0** were translated.
//! * The same history contains 2 pure rotations, covering 62 child `(at …)`
//!   nodes: **62/62** had their angle advanced by exactly the rotation delta and
//!   **0** had their x/y touched. Widening to 967 cross-instance comparisons of
//!   the same library footprint placed at different angles across 36 boards:
//!   2555/2556 pad angles differ by exactly the rotation delta, and 0/2556 pad
//!   x/y are explained by rotating the local coordinates.
//! * The child angle is absolute and **omitted when it works out to 0** — which
//!   is why a `(pad "1" smd rect (at -0.4 0.8))` shows up on a footprint rotated
//!   180°, and why rotating a footprint has to *add* an angle to pads that do
//!   not carry one.
//!
//! The one exception is a `(zone …)` nested in a footprint (a keepout): its
//! `(pts …)` are in board coordinates. Moving such a footprint by editing only
//! its own `(at …)` would leave the keepout behind, so those footprints are
//! refused rather than silently half-moved.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{
    get_path, require_f64, require_str, write_board_synced, BoardSync, ToolContext, ToolDef,
};
use konnect_ipc::client::KiCadIpcClient;
use konnect_sexp::writer::{
    apply_edits, check_document, find_balanced_block, find_block_with_leading_whitespace, SexpEdit,
};
use serde_json::json;
use std::path::Path;

// ─── IPC helper ───────────────────────────────────────────────────────────────
//
// Only the three tools that still create footprints use this.

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
//
// `tag_opens_at`, `top_level_block_starts`, `child_block_starts`, `delete_range`,
// `commit` and `write_note` are the primitives `pcb_routing` established for
// this migration, copied verbatim because they are private to that module.
// Lifting them into `tools/mod.rs` is the obvious follow-up now that three
// toolsets share them; it is deliberately not done here so this change touches
// one file.

/// What to tell the caller about KiCAD's own window after a write.
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

/// True when the `(` at `open` opens a `(tag …)` block — whole tags only, so
/// `"footprint"` never matches `(footprint_thing`.
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
/// One string-aware pass. It ignores same-named blocks nested inside another
/// item, and unlike a `"\n\t(tag"` literal it does not care whether the file is
/// tab-indented (KiCAD) or space-indented (this crate's writer).
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

/// A direct child of a footprint: its tag and the byte range it occupies.
#[derive(Debug, Clone)]
struct Child {
    tag: String,
    start: usize,
    end: usize,
}

/// Every **direct child** block of the block opening at `block_start`, in file
/// order, with its tag.
///
/// The same string-aware scan as [`top_level_block_starts`], scoped to one
/// block: a footprint's own `(at …)` is a direct child, the `(at …)` inside one
/// of its properties is not, and neither is anything that merely looks like a
/// block inside a quoted string.
fn child_blocks(content: &str, block_start: usize, block_end: usize) -> Vec<Child> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut i) = (0usize, block_start);
    let (mut in_string, mut escape) = (false, false);
    let stop = block_end.min(bytes.len());
    let mut open_children: Vec<usize> = Vec::new();

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
                    if depth == 1 {
                        open_children.push(i);
                    }
                    depth += 1;
                }
                b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 1 {
                        if let Some(start) = open_children.pop() {
                            out.push(Child {
                                tag: tag_at(content, start),
                                start,
                                end: i + 1,
                            });
                        }
                    } else if depth == 0 {
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

/// The tag of the block opening at `open`: `(fp_line …)` → `"fp_line"`.
fn tag_at(content: &str, open: usize) -> String {
    content
        .get(open + 1..)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect()
        })
        .unwrap_or_default()
}

/// The byte range to delete for the block at `block_start`, or the reason to
/// refuse.
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
    let Some((del_start, del_end)) = find_block_with_leading_whitespace(content, block_start)
    else {
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

// ─── Numbers and angles ───────────────────────────────────────────────────────

/// Round to KiCAD's own coordinate precision, so `89.99999999999999` never
/// reaches the file.
fn round6(v: f64) -> f64 {
    let r = (v * 1e6).round() / 1e6;
    if r == 0.0 {
        0.0 // fold -0.0, which would otherwise be written as "-0"
    } else {
        r
    }
}

/// A number as KiCAD writes it: shortest decimal that round-trips, no exponent,
/// no trailing `.0`.
fn fmt_num(v: f64) -> String {
    format!("{}", round6(v))
}

/// Keep a rotation inside one turn without changing which way it faces.
///
/// `-90` is left as `-90` rather than normalised to `270`: KiCAD itself writes
/// the negative form (a real rotation from 90° to −90° in the measured history
/// wrote `(at 172.2 108 -90)` and `(at 0 -6.36 -90)`), so preserving it keeps
/// our output in the file's own idiom.
fn wrap_angle(a: f64) -> f64 {
    let r = round6(a);
    if r.abs() >= 360.0 {
        round6(r % 360.0)
    } else {
        r
    }
}

/// A child's fixed rotation *relative to its footprint*, in `[0, 360)`.
///
/// Turning a footprint is `child = footprint + offset`, and the offset has to be
/// normalised before it is re-applied or the arithmetic drifts: a footprint at
/// 270° asked for 360° lands on 0°, and a child at 0° would come out as −270°
/// rather than the 90° that is the same direction and what KiCAD writes.
fn angle_offset(child: f64, footprint: f64) -> f64 {
    let off = round6(child - footprint).rem_euclid(360.0);
    round6(off)
}

/// An `(at x y [angle])` node, with the byte range of each number as written.
///
/// The token ranges are what make an edit surgical: setting x rewrites the x
/// token alone, so the file's own spacing — and its own number formatting, which
/// is not always shortest-form — survives everywhere else. A move that puts a
/// footprint back where it started therefore leaves the file byte-identical.
#[derive(Debug, Clone)]
struct AtNode {
    /// Byte range of the whole `(at …)` block.
    start: usize,
    end: usize,
    /// Byte ranges of the numeric tokens, in order: x, y, and angle when present.
    tokens: Vec<(usize, usize)>,
    x: f64,
    y: f64,
    /// Board-space angle; 0 when the node omits it, which is what an omitted
    /// angle means.
    angle: f64,
    has_angle: bool,
}

/// Parse the `(at …)` block opening at `node_start`.
///
/// `None` — refuse — when it is not an `(at …)`, when it has the wrong number of
/// fields, or when any field is not a plain number. On 36 real 20260206 boards
/// every one of 57409 `(at …)` fields was numeric, so anything else is unknown
/// syntax and not something to guess at.
fn parse_at(content: &str, node_start: usize) -> Option<AtNode> {
    let (start, end) = find_balanced_block(content, node_start)?;
    let inner = content.get(start + 1..end.checked_sub(1)?)?;
    let after_tag = inner.strip_prefix("at")?;
    if after_tag
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace() && c != ')')
    {
        return None; // `(atom …)`, not `(at …)`
    }

    let base = start + 1 + "at".len();
    let bytes = after_tag.as_bytes();
    let (mut tokens, mut vals) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let s = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        vals.push(after_tag.get(s..i)?.parse::<f64>().ok()?);
        tokens.push((base + s, base + i));
    }
    if vals.len() != 2 && vals.len() != 3 {
        return None;
    }
    Some(AtNode {
        start,
        end,
        x: vals[0],
        y: vals[1],
        angle: vals.get(2).copied().unwrap_or(0.0),
        has_angle: vals.len() == 3,
        tokens,
    })
}

impl AtNode {
    /// Rewrite token `i` to `v` — or nothing at all when the token already holds
    /// that value.
    ///
    /// The comparison is on the *value*, not the text: a board written by
    /// something other than KiCAD may say `157.150000`, and a move that leaves x
    /// alone must not restyle it to `157.15`.
    fn set_token(&self, content: &str, i: usize, v: f64) -> Option<SexpEdit> {
        let &(s, e) = self.tokens.get(i)?;
        // A splice that reaches outside the node it belongs to is a bug in the
        // scan, not something to write to the user's board.
        if s < self.start || e > self.end {
            return None;
        }
        let v = round6(v);
        if content.get(s..e).and_then(|t| t.parse::<f64>().ok()) == Some(v) {
            return None;
        }
        Some(SexpEdit::replace(s, e, fmt_num(v)))
    }

    /// The edit that gives this node the board-space angle `angle`.
    ///
    /// An existing angle field is rewritten in place. A node that omits the
    /// angle only gains one when the new angle is non-zero, because omitted
    /// *means* zero — adding `(at x y 0)` everywhere would churn the file for
    /// nothing.
    fn set_angle(&self, content: &str, angle: f64) -> Option<SexpEdit> {
        let angle = wrap_angle(angle);
        if self.has_angle {
            return self.set_token(content, 2, angle);
        }
        if angle == 0.0 {
            return None;
        }
        let after_last = self.tokens.last()?.1;
        // The new field goes inside the node, never past its closing paren.
        if after_last >= self.end {
            return None;
        }
        Some(SexpEdit::insert(after_last, format!(" {}", fmt_num(angle))))
    }
}

// ─── Footprints on the board ──────────────────────────────────────────────────

/// Direct children of a footprint whose `(at …)` angle is in **board** space and
/// so has to turn with the footprint even when the file omits it.
///
/// Measured: on 36 real boards only `property`, `pad` and `fp_text` carry an
/// `(at …)` with an angle, and every one of them tracks the footprint's own.
/// Anything else that turns up with an explicit angle is rotated too (see
/// [`rotate_edits`]); this list is only about *adding* an angle that is missing,
/// which is a guess unless the tag is known to want one.
const ANGLE_CHILDREN: [&str; 3] = ["property", "pad", "fp_text"];

/// Direct children of a footprint whose geometry is in **board** coordinates
/// rather than relative to the footprint origin.
///
/// A `(zone …)` nested in a footprint — a keepout — holds absolute `(xy …)`
/// points. 3 of 1862 footprints measured have one. Moving the footprint by
/// editing its own `(at …)` would leave the keepout where it was, so a footprint
/// carrying one is refused rather than half-moved.
const ABSOLUTE_GEOMETRY_CHILDREN: [&str; 1] = ["zone"];

/// A `(property "Name" "Value" …)` inside a footprint.
#[derive(Debug, Clone)]
struct FootprintProperty {
    name: String,
    value: Option<String>,
    /// Byte range of the value token exactly as written, quotes included, so it
    /// can be replaced without disturbing the rest of the property.
    value_range: Option<(usize, usize)>,
    at: Option<AtNode>,
}

/// A top-level `(footprint …)` as it exists on the board, with the byte range it
/// occupies so it can be edited or deleted.
#[derive(Debug, Clone)]
struct BoardFootprint {
    start: usize,
    end: usize,
    /// The `Library:Footprint` id in the block header.
    library_id: Option<String>,
    uuid: Option<String>,
    layer: Option<String>,
    reference: Option<String>,
    value: Option<String>,
    /// The footprint's own `(at …)` — the only thing a move changes.
    at: Option<AtNode>,
    properties: Vec<FootprintProperty>,
    /// Every direct child that carries its own `(at …)`, with its tag. These are
    /// what a rotation turns.
    child_ats: Vec<(String, AtNode)>,
    pad_count: usize,
    /// Tags of direct children whose coordinates are absolute — non-empty means
    /// this footprint cannot be moved by editing its `(at …)` alone.
    absolute_children: Vec<String>,
}

impl BoardFootprint {
    /// A short name for error messages and reports.
    fn label(&self) -> String {
        match (&self.reference, &self.uuid) {
            (Some(r), _) => r.clone(),
            (None, Some(u)) => format!("uuid {u}"),
            (None, None) => format!("the footprint at byte {}", self.start),
        }
    }

    /// The footprint's own `(at …)`, or the reason it cannot be edited.
    fn require_at(&self, board: &Path) -> Result<&AtNode, String> {
        self.at.as_ref().ok_or_else(|| {
            format!(
                "Refusing to change '{}' in '{}': it has no readable (at x y …) of its own. \
                 Nothing was written.",
                self.label(),
                board.display()
            )
        })
    }

    /// Whether this footprint can be repositioned by editing its own `(at …)`.
    fn require_relative_geometry(&self, board: &Path, what: &str) -> Result<(), String> {
        if self.absolute_children.is_empty() {
            return Ok(());
        }
        Err(format!(
            "Refusing to {what} '{}' in '{}': it contains a nested ({}) whose points are in \
             board coordinates, not footprint coordinates, so moving the footprint alone would \
             leave it behind. Move this one in KiCAD. Nothing was written.",
            self.label(),
            board.display(),
            self.absolute_children.join("), (")
        ))
    }
}

/// Read the token starting at or after `from`: a quoted string with its escapes
/// resolved, or a bare atom. Returns the text and the byte range it occupies,
/// quotes included.
fn read_token(content: &str, from: usize, stop: usize) -> Option<(String, usize, usize)> {
    let bytes = content.as_bytes();
    let mut i = from;
    while i < stop && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= stop {
        return None;
    }
    if bytes[i] == b'"' {
        let start = i;
        i += 1;
        let mut text = String::new();
        while i < stop {
            match bytes[i] {
                b'\\' if i + 1 < stop => {
                    text.push(content[i + 1..].chars().next()?);
                    i += 2;
                }
                b'"' => return Some((text, start, i + 1)),
                _ => {
                    let c = content[i..].chars().next()?;
                    text.push(c);
                    i += c.len_utf8();
                }
            }
        }
        return None; // unterminated
    }
    let start = i;
    while i < stop && !bytes[i].is_ascii_whitespace() && bytes[i] != b'(' && bytes[i] != b')' {
        i += 1;
    }
    (i > start).then(|| (content[start..i].to_string(), start, i))
}

/// `"a \"b\""` — a string as KiCAD would write it.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Parse a `(property …)` block. Handles both the quoted form KiCAD writes for
/// user fields and the bare-atom form it uses for `(property ki_fp_filters "C_*")`.
fn parse_property(content: &str, child: &Child) -> Option<FootprintProperty> {
    let after_tag = child.start + 1 + "property".len();
    let (name, _, name_end) = read_token(content, after_tag, child.end)?;
    let value = read_token(content, name_end, child.end)
        .filter(|(_, s, _)| content.as_bytes().get(*s) == Some(&b'"'));
    let at = child_blocks(content, child.start, child.end)
        .iter()
        .find(|c| c.tag == "at")
        .and_then(|c| parse_at(content, c.start));
    Some(FootprintProperty {
        name,
        value: value.as_ref().map(|(v, _, _)| v.clone()),
        value_range: value.as_ref().map(|(_, s, e)| (*s, *e)),
        at,
    })
}

/// Every **top-level** `(footprint …)` on the board, in file order.
///
/// A footprint nested inside another block is part of that block's definition
/// and is never touched here.
fn collect_footprints(content: &str) -> Vec<BoardFootprint> {
    top_level_block_starts(content, "footprint")
        .into_iter()
        .filter_map(|s| {
            let (start, end) = find_balanced_block(content, s)?;
            let header_from = start + 1 + "footprint".len();
            let library_id = read_token(content, header_from, end)
                .filter(|(_, s, _)| content.as_bytes().get(*s) == Some(&b'"'))
                .map(|(v, _, _)| v);

            let mut fp = BoardFootprint {
                start,
                end,
                library_id,
                uuid: None,
                layer: None,
                reference: None,
                value: None,
                at: None,
                properties: Vec::new(),
                child_ats: Vec::new(),
                pad_count: 0,
                absolute_children: Vec::new(),
            };

            for child in child_blocks(content, start, end) {
                match child.tag.as_str() {
                    // The footprint's own position. Only ever the direct child —
                    // a property's or pad's `(at …)` is one level deeper and is
                    // never mistaken for it.
                    "at" => fp.at = parse_at(content, child.start),
                    "uuid" | "layer" => {
                        let after = child.start + 1 + child.tag.len();
                        let v = read_token(content, after, child.end).map(|(v, _, _)| v);
                        if child.tag == "uuid" {
                            fp.uuid = v;
                        } else {
                            fp.layer = v;
                        }
                    }
                    "property" => {
                        if let Some(p) = parse_property(content, &child) {
                            match p.name.as_str() {
                                "Reference" => fp.reference = p.value.clone(),
                                "Value" => fp.value = p.value.clone(),
                                _ => {}
                            }
                            if let Some(at) = p.at.clone() {
                                fp.child_ats.push((child.tag.clone(), at));
                            }
                            fp.properties.push(p);
                        }
                    }
                    _ => {
                        if child.tag == "pad" {
                            fp.pad_count += 1;
                        }
                        if ABSOLUTE_GEOMETRY_CHILDREN.contains(&child.tag.as_str()) {
                            fp.absolute_children.push(child.tag.clone());
                        }
                        if let Some(at) = child_blocks(content, child.start, child.end)
                            .iter()
                            .find(|c| c.tag == "at")
                            .and_then(|c| parse_at(content, c.start))
                        {
                            fp.child_ats.push((child.tag.clone(), at));
                        }
                    }
                }
            }
            Some(fp)
        })
        .collect()
}

/// How the caller named a footprint. `reference` is what the tools have always
/// taken; `uuid` is accepted too, because it is what the routing tools report
/// and it is unambiguous when two boards share a designator scheme.
enum ComponentKey {
    Reference(String),
    Uuid(String),
}

impl ComponentKey {
    fn matches(&self, fp: &BoardFootprint) -> bool {
        match self {
            ComponentKey::Reference(r) => fp.reference.as_deref() == Some(r.as_str()),
            ComponentKey::Uuid(u) => fp.uuid.as_deref() == Some(u.as_str()),
        }
    }

    fn describe(&self) -> String {
        match self {
            ComponentKey::Reference(r) => format!("reference '{r}'"),
            ComponentKey::Uuid(u) => format!("uuid '{u}'"),
        }
    }
}

/// The footprint `key` names, or the error explaining what is on the board
/// instead. Every tool that takes a component resolves it through here, so they
/// all refuse the same way.
fn find_footprint(
    content: &str,
    key: &ComponentKey,
    board: &Path,
) -> Result<BoardFootprint, CallToolResult> {
    collect_footprints(content)
        .into_iter()
        .find(|f| key.matches(f))
        .ok_or_else(|| CallToolResult::error(unknown_component_message(content, key, board)))
}

/// Read the `reference` / `uuid` argument, or the error to return.
fn component_key(args: &serde_json::Value) -> Result<ComponentKey, CallToolResult> {
    if let Some(r) = args["reference"].as_str().filter(|s| !s.is_empty()) {
        return Ok(ComponentKey::Reference(r.to_string()));
    }
    if let Some(u) = args["uuid"].as_str().filter(|s| !s.is_empty()) {
        return Ok(ComponentKey::Uuid(u.to_string()));
    }
    // `reference` is the documented argument, so name it in the error.
    Err(require_str(args, "reference").unwrap_err())
}

/// Why a key did not name a footprint. A bare "not found" sends the caller
/// looking in the wrong place, so say what *is* on the board.
fn unknown_component_message(content: &str, key: &ComponentKey, board: &Path) -> String {
    let fps = collect_footprints(content);
    if let ComponentKey::Reference(r) = key {
        if let Some(fp) = fps.iter().find(|f| f.uuid.as_deref() == Some(r.as_str())) {
            return format!(
                "'{r}' is the uuid of footprint '{}' on '{}', not a reference designator. \
                 Pass it as uuid.",
                fp.reference.as_deref().unwrap_or("?"),
                board.display()
            );
        }
    }
    let mut refs: Vec<&str> = fps.iter().filter_map(|f| f.reference.as_deref()).collect();
    refs.sort_unstable();
    let shown: Vec<&str> = refs.iter().copied().take(20).collect();
    let more = refs.len().saturating_sub(shown.len());
    format!(
        "No footprint with {} on '{}'. The board has {} footprint(s){}{}. Run \
         get_component_list to see them all.",
        key.describe(),
        board.display(),
        fps.len(),
        if shown.is_empty() {
            String::new()
        } else {
            format!(": {}", shown.join(", "))
        },
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

/// One footprint as JSON, shared by every tool that reports one.
fn footprint_json(fp: &BoardFootprint) -> serde_json::Value {
    json!({
        "reference": fp.reference,
        "value": fp.value,
        "footprint": fp.library_id,
        // uuid is what every tool here also accepts as an identifier.
        "uuid": fp.uuid,
        "x": fp.at.as_ref().map(|a| a.x),
        "y": fp.at.as_ref().map(|a| a.y),
        "rotation": fp.at.as_ref().map(|a| a.angle),
        "layer": fp.layer,
        "pad_count": fp.pad_count
    })
}

/// The edits that move `fp` to `(x, y)`.
///
/// Only the footprint's own `(at …)` changes: every child coordinate is relative
/// to the footprint origin and moves with it for free. Measured against 481
/// footprint moves in KiCAD's own save history, where a move changed exactly one
/// line of the file and 0 of 4012 child `(at …)` were translated.
fn move_edits(
    content: &str,
    board: &Path,
    fp: &BoardFootprint,
    x: f64,
    y: f64,
) -> Result<Vec<SexpEdit>, String> {
    fp.require_relative_geometry(board, "move")?;
    let at = fp.require_at(board)?;
    Ok([at.set_token(content, 0, x), at.set_token(content, 1, y)]
        .into_iter()
        .flatten()
        .collect())
}

/// The edits that set `fp`'s rotation to `angle` degrees.
///
/// The footprint's own `(at …)` angle becomes `angle`, and every child `(at …)`
/// angle advances by the same delta while its x/y are left exactly where they
/// are. Measured: 62/62 child angles in KiCAD's own rotations advanced by the
/// delta, 0/62 child x/y moved, and 0 of 2556 pad positions across 36 boards are
/// explained by rotating the local coordinates.
fn rotate_edits(
    content: &str,
    board: &Path,
    fp: &BoardFootprint,
    angle: f64,
) -> Result<Vec<SexpEdit>, String> {
    fp.require_relative_geometry(board, "rotate")?;
    let at = fp.require_at(board)?;
    let target = wrap_angle(angle);

    let mut edits: Vec<SexpEdit> = at.set_angle(content, target).into_iter().collect();
    if target == at.angle {
        return Ok(edits);
    }
    for (tag, child) in &fp.child_ats {
        // An explicit angle always turns. A missing one only gains a value for
        // the tags measured to carry board-space angles — for anything else,
        // adding a field the format may not accept there is a guess.
        if child.has_angle || ANGLE_CHILDREN.contains(&tag.as_str()) {
            // Re-derive from the child's own offset rather than adding the raw
            // delta, so the number written stays inside one turn.
            let turned = target + angle_offset(child.angle, at.angle);
            edits.extend(child.set_angle(content, turned));
        }
    }
    Ok(edits)
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "place_component",
            "Place a footprint on the PCB at the given position and layer via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":      { "type": "string" },
                    "footprint":  { "type": "string", "description": "Library:Footprint (e.g. 'Resistor_SMD:R_0402')" },
                    "reference":  { "type": "string", "description": "Reference designator" },
                    "x":          { "type": "number" },
                    "y":          { "type": "number" },
                    "rotation":   { "type": "number", "default": 0 },
                    "layer":      { "type": "string", "default": "F.Cu" }
                },
                "required": ["board", "footprint", "reference", "x", "y"]
            }),
            |args, ctx| async move { handle_place_component(args, ctx).await }
        ),
        tool!(
            "move_component",
            "Move a placed footprint to a new X/Y position by editing the .kicad_pcb file \
             (no KiCAD IPC required). Footprint-relative pad and text offsets are left alone.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'U3'" },
                    "uuid":      { "type": "string", "description": "Footprint uuid, as an alternative to reference" },
                    "x":         { "type": "number" },
                    "y":         { "type": "number" }
                },
                "required": ["board", "x", "y"]
            }),
            |args, ctx| async move { handle_move_component(args, ctx).await }
        ),
        tool!(
            "rotate_component",
            "Set the rotation angle of a placed footprint by editing the .kicad_pcb file \
             (no KiCAD IPC required). Pad and text angles turn with it; their offsets do not move.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'U3'" },
                    "uuid":      { "type": "string", "description": "Footprint uuid, as an alternative to reference" },
                    "rotation":  { "type": "number", "description": "Absolute rotation angle in degrees" }
                },
                "required": ["board", "rotation"]
            }),
            |args, ctx| async move { handle_rotate_component(args, ctx).await }
        ),
        tool!(
            "delete_component",
            "Remove a footprint from the board by editing the .kicad_pcb file \
             (no KiCAD IPC required).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'U3'" },
                    "uuid":      { "type": "string", "description": "Footprint uuid, as an alternative to reference" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_delete_component(args, ctx).await }
        ),
        tool!(
            "edit_component",
            "Update the Value (or another existing property) of a placed footprint by editing \
             the .kicad_pcb file (no KiCAD IPC required).",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'U3'" },
                    "uuid":      { "type": "string", "description": "Footprint uuid, as an alternative to reference" },
                    "value":     { "type": "string", "description": "New value string" },
                    "properties": {
                        "type": "object",
                        "description": "Other existing footprint properties to set, by name. \
                                        Properties the footprint does not already have are refused."
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_edit_component(args, ctx).await }
        ),
        tool!(
            "find_component",
            "Find a footprint on the board by reference designator (or uuid) and return its \
             position, read from the .kicad_pcb file.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string", "description": "Reference designator, e.g. 'U3'" },
                    "uuid":      { "type": "string", "description": "Footprint uuid, as an alternative to reference" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_find_component(args, ctx).await }
        ),
        tool!(
            "get_component_pads",
            "Return the pad positions and net assignments for a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":     { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["board", "reference"]
            }),
            |args, ctx| async move { handle_get_component_pads(args, ctx).await }
        ),
        tool!(
            "get_pad_position",
            "Return the board-space position of a specific pad number on a footprint.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "reference":   { "type": "string" },
                    "pad_number":  { "type": "string" }
                },
                "required": ["board", "reference", "pad_number"]
            }),
            |args, ctx| async move { handle_get_pad_position(args, ctx).await }
        ),
        tool!(
            "get_component_list",
            "List all footprints on the board with their positions, layers, and values, read \
             from the .kicad_pcb file (no KiCAD IPC required).",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "layer": { "type": "string", "description": "Only footprints on this layer, e.g. 'B.Cu'" }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_component_list(args, ctx).await }
        ),
        tool!(
            "place_component_array",
            "Place multiple copies of a footprint in a grid or line array via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":        { "type": "string" },
                    "footprint":    { "type": "string" },
                    "start_x":      { "type": "number" },
                    "start_y":      { "type": "number" },
                    "count_x":      { "type": "integer", "description": "Number of columns" },
                    "count_y":      { "type": "integer", "description": "Number of rows", "default": 1 },
                    "spacing_x":    { "type": "number", "description": "Column spacing in mm" },
                    "spacing_y":    { "type": "number", "description": "Row spacing in mm", "default": 0 },
                    "ref_prefix":   { "type": "string", "description": "Reference prefix (e.g. 'R')", "default": "U" },
                    "ref_start":    { "type": "integer", "description": "Starting reference number", "default": 1 }
                },
                "required": ["board", "footprint", "start_x", "start_y", "count_x", "spacing_x"]
            }),
            |args, ctx| async move { handle_place_array(args, ctx).await }
        ),
        tool!(
            "align_components",
            "Align multiple footprints along a common X or Y axis by editing the .kicad_pcb \
             file (no KiCAD IPC required). All-or-nothing: one unknown reference writes nothing.",
            json!({
                "type": "object",
                "properties": {
                    "board":       { "type": "string" },
                    "references":  { "type": "array", "items": { "type": "string" } },
                    "axis":        { "type": "string", "description": "'x' aligns to a common X, 'y' to a common Y", "default": "x" },
                    "value":       { "type": "number", "description": "Target coordinate to align to" }
                },
                "required": ["board", "references", "value"]
            }),
            |args, ctx| async move { handle_align_components(args, ctx).await }
        ),
        tool!(
            "duplicate_component",
            "Duplicate an existing footprint at a new position via KiCAD IPC.",
            json!({
                "type": "object",
                "properties": {
                    "board":         { "type": "string" },
                    "reference":     { "type": "string", "description": "Reference to duplicate" },
                    "new_reference": { "type": "string", "description": "New reference designator" },
                    "x":             { "type": "number" },
                    "y":             { "type": "number" }
                },
                "required": ["board", "reference", "new_reference", "x", "y"]
            }),
            |args, ctx| async move { handle_duplicate_component(args, ctx).await }
        ),
        tool!(
            "get_board_2d_view",
            "Render the PCB as a 2-D image using kicad-cli and return it as a base64 PNG.",
            json!({
                "type": "object",
                "properties": {
                    "board":  { "type": "string" },
                    "layers": {
                        "type": "array",
                        "description": "Layers to include (empty = default copper + silkscreen)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_get_board_2d_view(args, ctx).await }
        ),
    ]
}

// ─── Handlers: reads ──────────────────────────────────────────────────────────

async fn handle_get_component_list(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let layer = args["layer"].as_str().map(String::from);

    let content = std::fs::read_to_string(&board_path)?;
    let items: Vec<serde_json::Value> = collect_footprints(&content)
        .iter()
        .filter(|fp| {
            layer
                .as_deref()
                .is_none_or(|l| fp.layer.as_deref() == Some(l))
        })
        .map(footprint_json)
        .collect();

    Ok(CallToolResult::json(&json!({
        "count": items.len(),
        "components": items,
        "target": board_path.display().to_string(),
        "source": "file"
    })))
}

async fn handle_find_component(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let key = match component_key(args) {
        Ok(k) => k,
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let fp = match find_footprint(&content, &key, &board_path) {
        Ok(f) => f,
        Err(e) => return Ok(e),
    };

    let mut out = footprint_json(&fp);
    out["properties"] = json!(fp
        .properties
        .iter()
        .map(|p| json!({ "name": p.name, "value": p.value }))
        .collect::<Vec<_>>());
    out["target"] = json!(board_path.display().to_string());
    out["source"] = json!("file");
    Ok(CallToolResult::json(&out))
}

// ─── Handlers: writes ─────────────────────────────────────────────────────────

async fn handle_move_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let key = match component_key(args) {
        Ok(k) => k,
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

    let content = std::fs::read_to_string(&board_path)?;
    let fp = match find_footprint(&content, &key, &board_path) {
        Ok(f) => f,
        Err(e) => return Ok(e),
    };

    let (from_x, from_y) = fp.at.as_ref().map(|a| (a.x, a.y)).unzip();
    let edits = match move_edits(&content, &board_path, &fp, x, y) {
        Ok(e) => e,
        Err(e) => return Ok(CallToolResult::error(e)),
    };
    let sync = match commit(&ctx.config.ipc_address, &board_path, content, edits).await {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "moved": fp.reference, "uuid": fp.uuid,
        "x": round6(x), "y": round6(y),
        "previous_x": from_x, "previous_y": from_y,
        "rotation": fp.at.as_ref().map(|a| a.angle),
        "layer": fp.layer,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_rotate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let key = match component_key(args) {
        Ok(k) => k,
        Err(e) => return Ok(e),
    };
    let rotation = match require_f64(args, "rotation") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let fp = match find_footprint(&content, &key, &board_path) {
        Ok(f) => f,
        Err(e) => return Ok(e),
    };

    let was = fp.at.as_ref().map(|a| a.angle);
    let edits = match rotate_edits(&content, &board_path, &fp, rotation) {
        Ok(e) => e,
        Err(e) => return Ok(CallToolResult::error(e)),
    };
    let turned = edits.len().saturating_sub(1);
    let sync = match commit(&ctx.config.ipc_address, &board_path, content, edits).await {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "rotated": fp.reference, "uuid": fp.uuid,
        "rotation": wrap_angle(rotation), "previous_rotation": was,
        "x": fp.at.as_ref().map(|a| a.x), "y": fp.at.as_ref().map(|a| a.y),
        // Pad and text angles are in board space, so they turned too; their
        // offsets from the footprint origin did not move.
        "child_angles_updated": turned,
        "layer": fp.layer,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

async fn handle_delete_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let key = match component_key(args) {
        Ok(k) => k,
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let fp = match find_footprint(&content, &key, &board_path) {
        Ok(f) => f,
        Err(e) => return Ok(e),
    };

    let (del_start, del_end) = match delete_range(
        &content,
        fp.start,
        fp.end,
        &format!("footprint {}", fp.label()),
        &board_path,
    ) {
        Ok(r) => r,
        Err(e) => return Ok(CallToolResult::error(e)),
    };

    let removed = footprint_json(&fp);
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
        "deleted": fp.reference, "uuid": fp.uuid,
        "component": removed,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        // Deleting the footprint does not delete the traces that were routed to
        // its pads; those segments are top-level items and stay on the board.
        "note": format!(
            "{} Any traces routed to this footprint's pads remain — use query_traces / \
             delete_trace to clean them up.",
            write_note(&sync)
        )
    })))
}

async fn handle_edit_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let key = match component_key(args) {
        Ok(k) => k,
        Err(e) => return Ok(e),
    };

    // What to set: `value` is shorthand for the "Value" property.
    let mut wanted: Vec<(String, String)> = Vec::new();
    if let Some(v) = args["value"].as_str() {
        wanted.push(("Value".to_string(), v.to_string()));
    }
    if let Some(map) = args["properties"].as_object() {
        for (name, v) in map {
            let Some(v) = v.as_str() else {
                return Ok(CallToolResult::error(format!(
                    "Property '{name}' must be a string; footprint properties are text fields."
                )));
            };
            wanted.push((name.clone(), v.to_string()));
        }
    }
    if wanted.is_empty() {
        return Ok(CallToolResult::error(
            "Nothing to change: pass 'value', or 'properties' as a name→value object.".to_string(),
        ));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let fp = match find_footprint(&content, &key, &board_path) {
        Ok(f) => f,
        Err(e) => return Ok(e),
    };

    // Renaming a footprint on the PCB alone desynchronises it from the
    // schematic, which is what the next netlist update would undo anyway.
    if wanted.iter().any(|(n, _)| n == "Reference") {
        return Ok(CallToolResult::error(format!(
            "Refusing to change the Reference of '{}' here: the designator lives in the \
             schematic and PCB sync would put it back. Rename it with \
             edit_schematic_component, then run Tools > Update PCB from Schematic. \
             Nothing was written.",
            fp.label()
        )));
    }

    let mut edits = Vec::new();
    let mut changed = Vec::new();
    for (name, new_value) in &wanted {
        let Some(prop) = fp.properties.iter().find(|p| &p.name == name) else {
            let have: Vec<&str> = fp.properties.iter().map(|p| p.name.as_str()).collect();
            return Ok(CallToolResult::error(format!(
                "Footprint '{}' has no property '{name}'. It has: {}. Adding a new property \
                 needs a layer and text placement that only KiCAD can choose sensibly, so \
                 nothing was written.",
                fp.label(),
                have.join(", ")
            )));
        };
        let Some((s, e)) = prop.value_range else {
            return Ok(CallToolResult::error(format!(
                "Property '{name}' on '{}' has no quoted value field to replace. Nothing was \
                 written.",
                fp.label()
            )));
        };
        if prop.value.as_deref() == Some(new_value.as_str()) {
            continue; // already says that — leave the bytes alone
        }
        changed.push(json!({
            "name": name, "from": prop.value, "to": new_value
        }));
        edits.push(SexpEdit::replace(s, e, quoted(new_value)));
    }

    if edits.is_empty() {
        return Ok(CallToolResult::json(&json!({
            "reference": fp.reference, "uuid": fp.uuid,
            "changed": [],
            "target": board_path.display().to_string(),
            "source": "file",
            "note": "Every property already had the requested value; nothing was written."
        })));
    }

    let sync = match commit(&ctx.config.ipc_address, &board_path, content, edits).await {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "reference": fp.reference, "uuid": fp.uuid,
        "footprint": fp.library_id,
        "changed": changed,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": format!(
            "{} The schematic still holds the old value — change it there too, or the next \
             Update PCB from Schematic will revert this.",
            write_note(&sync)
        )
    })))
}

async fn handle_align_components(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let axis = args["axis"].as_str().unwrap_or("x").to_ascii_lowercase();
    if axis != "x" && axis != "y" {
        return Ok(CallToolResult::error(format!(
            "axis must be 'x' (align to a common X) or 'y' (align to a common Y), not '{axis}'."
        )));
    }
    let value = match require_f64(args, "value") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let refs: Vec<String> = args["references"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if refs.is_empty() {
        return Ok(CallToolResult::error(
            "references must be a non-empty array of reference designators.".to_string(),
        ));
    }

    let content = std::fs::read_to_string(&board_path)?;
    let all = collect_footprints(&content);

    // All-or-nothing: resolve every reference before writing anything, so a
    // typo in the last one does not leave the first half of the row moved.
    let mut targets = Vec::new();
    for reference in &refs {
        let key = ComponentKey::Reference(reference.clone());
        let Some(fp) = all.iter().find(|f| key.matches(f)) else {
            return Ok(CallToolResult::error(format!(
                "{} No footprint was moved.",
                unknown_component_message(&content, &key, &board_path)
            )));
        };
        targets.push(fp);
    }

    let mut edits = Vec::new();
    let mut aligned = Vec::new();
    for fp in &targets {
        let at = match fp.require_at(&board_path) {
            Ok(a) => a,
            Err(e) => {
                return Ok(CallToolResult::error(format!(
                    "{e} No footprint was moved."
                )))
            }
        };
        let (nx, ny) = if axis == "y" {
            (at.x, value)
        } else {
            (value, at.y)
        };
        match move_edits(&content, &board_path, fp, nx, ny) {
            Ok(e) => edits.extend(e),
            Err(e) => {
                return Ok(CallToolResult::error(format!(
                    "{e} No footprint was moved."
                )))
            }
        }
        aligned.push(json!({
            "reference": fp.reference, "uuid": fp.uuid,
            "x": round6(nx), "y": round6(ny),
            "previous_x": at.x, "previous_y": at.y
        }));
    }

    let sync = match commit(&ctx.config.ipc_address, &board_path, content, edits).await {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };

    Ok(CallToolResult::json(&json!({
        "aligned_count": aligned.len(),
        "axis": axis, "value": round6(value),
        "components": aligned,
        "target": board_path.display().to_string(),
        "source": "file",
        "kicad_sync": sync.as_json(),
        "note": write_note(&sync)
    })))
}

// ─── Handlers: pads (already file-based) ─────────────────────────────────────

async fn handle_get_component_pads(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let content = std::fs::read_to_string(&board_path)?;
    let tree = konnect_sexp::parser::parse_sexp(&content)?;

    // Find the footprint with matching reference
    let fp_node = tree.find_all("footprint").into_iter().find(|fp| {
        fp.find_all("property").iter().any(|p| {
            p.get(1).and_then(|n| n.as_str()) == Some("Reference")
                && p.get(2).and_then(|n| n.as_str()) == Some(reference.as_str())
        })
    });

    let fp_node = match fp_node {
        Some(n) => n,
        None => {
            return Ok(CallToolResult::error(unknown_component_message(
                &content,
                &ComponentKey::Reference(reference),
                &board_path,
            )))
        }
    };

    let fp_at = fp_node.find("at");
    let fp_x = fp_at.and_then(|a| a.get_f64(1)).unwrap_or(0.0);
    let fp_y = fp_at.and_then(|a| a.get_f64(2)).unwrap_or(0.0);
    let fp_rot = fp_at.and_then(|a| a.get_f64(3)).unwrap_or(0.0);

    let pads: Vec<serde_json::Value> = fp_node
        .find_all("pad")
        .iter()
        .filter_map(|pad| {
            let number = pad.get(1)?.as_str()?.to_string();
            let pad_at = pad.find("at")?;
            let local_x = pad_at.get_f64(1)?;
            let local_y = pad_at.get_f64(2)?;
            // Transform local pad coords to board space (rotation only).
            // Uses the canonical KiCAD transform — see konnect_sexp::geometry.
            let (board_x, board_y) =
                konnect_sexp::geometry::transform_pad(local_x, local_y, fp_x, fp_y, fp_rot);
            let net = pad
                .find("net")
                .and_then(|n| n.get(2))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            Some(json!({ "number": number, "x": board_x, "y": board_y, "net": net }))
        })
        .collect();

    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pad_count": pads.len(), "pads": pads }),
    ))
}

async fn handle_get_pad_position(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let pad_number = match require_str(args, "pad_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pads_result = handle_get_component_pads(args, ctx).await?;
    if pads_result.is_error {
        return Ok(pads_result);
    }
    // Parse the result and filter for the specific pad number
    if let Some(crate::mcp::protocol::ToolContent::Text { text }) = pads_result.content.first() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(pads) = parsed["pads"].as_array() {
                if let Some(pad) = pads
                    .iter()
                    .find(|p| p["number"].as_str() == Some(&pad_number))
                {
                    return Ok(CallToolResult::json(pad));
                }
            }
        }
    }
    Ok(CallToolResult::error(format!(
        "Pad '{}' not found",
        pad_number
    )))
}

// ─── Handlers: still on IPC ───────────────────────────────────────────────────
//
// These three create a footprint that is not on the board yet, which means
// resolving the library id through fp-lib-table to a `.pretty` directory and a
// `.kicad_mod`, then transforming that into a board `(footprint …)` block with
// fresh uuids and net-less pads. That is a subsystem of its own and is left for
// a later phase; until then they need KiCAD running with the board open, and
// they change KiCAD's in-memory board rather than the `board` file argument.

async fn handle_place_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let footprint = match require_str(args, "footprint") {
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
    let rotation = args["rotation"].as_f64().unwrap_or(0.0);
    let layer = args["layer"].as_str().unwrap_or("F.Cu").to_string();

    let fp = ipc!(ctx, |c| c
        .place_footprint(&footprint, x, y, rotation, &layer));
    Ok(CallToolResult::json(&json!({
        "placed": fp.reference,
        "footprint": fp.footprint,
        "x": fp.position.x, "y": fp.position.y,
        "rotation": fp.rotation, "layer": fp.layer,
        "source": "ipc",
        "note": "Placed in KiCAD's in-memory board. Save in KiCAD before using the file-based \
                 tools, or the placement will not be in the file they edit."
    })))
}

async fn handle_place_array(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let footprint = match require_str(args, "footprint") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let start_x = match require_f64(args, "start_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let start_y = match require_f64(args, "start_y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let count_x = args["count_x"].as_u64().unwrap_or(1) as usize;
    let count_y = args["count_y"].as_u64().unwrap_or(1) as usize;
    let spacing_x = match require_f64(args, "spacing_x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let spacing_y = args["spacing_y"].as_f64().unwrap_or(spacing_x);
    let prefix = args["ref_prefix"].as_str().unwrap_or("U").to_string();
    let ref_start = args["ref_start"].as_u64().unwrap_or(1) as usize;

    let mut placed = Vec::new();
    let mut n = ref_start;
    for row in 0..count_y {
        for col in 0..count_x {
            let x = start_x + col as f64 * spacing_x;
            let y = start_y + row as f64 * spacing_y;
            let reference = format!("{prefix}{n}");
            let fp_id = footprint.clone();
            let ref2 = reference.clone();
            match with_ipc(ctx.config.ipc_address.clone(), move |c| {
                c.place_footprint(&fp_id, x, y, 0.0, "F.Cu")
            })
            .await?
            {
                Ok(fp) => placed
                    .push(json!({ "reference": ref2, "x": fp.position.x, "y": fp.position.y })),
                Err(e) => {
                    return Ok(CallToolResult::error(format!(
                        "IPC error placing {}: {}",
                        reference, e
                    )))
                }
            }
            n += 1;
        }
    }
    Ok(CallToolResult::json(&json!({
        "placed_count": placed.len(),
        "components": placed,
        "source": "ipc",
        "note": "Placed in KiCAD's in-memory board. Save in KiCAD before using the file-based \
                 tools, or the placements will not be in the file they edit."
    })))
}

async fn handle_duplicate_component(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let _new_reference = match require_str(args, "new_reference") {
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

    // Get the source footprint's footprint ID and rotation
    let ref_ipc = reference.clone();
    let src = ipc!(ctx, |c| {
        c.get_footprint(&ref_ipc)?
            .ok_or_else(|| anyhow::anyhow!("Footprint '{}' not found", ref_ipc))
    });

    let fp = ipc!(ctx, |c| c.place_footprint(
        &src.footprint,
        x,
        y,
        src.rotation,
        &src.layer
    ));
    Ok(CallToolResult::json(&json!({
        "duplicated_from": reference,
        "new_reference": fp.reference,
        "x": fp.position.x, "y": fp.position.y,
        "source": "ipc",
        "note": "Placed in KiCAD's in-memory board. Save in KiCAD before using the file-based \
                 tools, or the copy will not be in the file they edit."
    })))
}

async fn handle_get_board_2d_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use base64::Engine;
    let board_path = get_path(args, "board")?;
    let layers: Vec<String> = args["layers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "F.Cu".into(),
                "B.Cu".into(),
                "F.SilkS".into(),
                "B.SilkS".into(),
                "Edge.Cuts".into(),
            ]
        });

    let tmp = board_path.with_extension("render.png");
    let layer_refs: Vec<&str> = layers.iter().map(String::as_str).collect();
    super::cli::render_pcb_png(&ctx.config.kicad_cli, &board_path, &tmp, &layer_refs).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(CallToolResult::image(b64, "image/png"))
}

#[cfg(test)]
mod component_file_tests {
    //! The fixtures are **tab-indented**, like every file KiCAD 10 writes, while
    //! this crate's own writer emits two spaces. A matcher with hardcoded
    //! leading whitespace passes against self-written files and silently finds
    //! nothing in the user's real board — the dominant bug class in this repo —
    //! so one fixture is space-indented and one is on a single line.

    use super::*;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext {
        // Empty ipc_address: none of the migrated tools talk to KiCAD any more,
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

    /// KiCAD 10 (20260206), tab-indented, transcribed from a real board.
    ///
    /// Deliberately adversarial:
    /// * `(gr_text "REV (A"` — an unbalanced paren inside a quoted string.
    /// * U3's Description contains the literal text `(footprint` and `(at 1 2)`,
    ///   which must never be seen as blocks.
    /// * U3's own `(at …)` comes *after* a property that has its own `(at …)`,
    ///   so a scan that took the first `(at` in the block would pick the wrong
    ///   one.
    /// * R1's pads omit the angle, which is what an angle of 0 looks like.
    /// * J1 carries a nested keepout `(zone …)` whose points are in board
    ///   coordinates.
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
            "\t(footprint \"Package_SO:SOIC-8_3.9x4.9mm_P1.27mm\"",
            "\t\t(layer \"F.Cu\")",
            "\t\t(uuid \"fp-u3\")",
            "\t\t(descr \"see (footprint \\\"other\\\") at (at 1 2)\")",
            "\t\t(property \"Reference\" \"U3\"",
            "\t\t\t(at 0 -3.4 90)",
            "\t\t\t(layer \"F.SilkS\")",
            "\t\t\t(uuid \"prop-u3-ref\")",
            "\t\t)",
            "\t\t(at 106.8 89 90)",
            "\t\t(property \"Value\" \"LM358\"",
            "\t\t\t(at 0 3.4 270)",
            "\t\t\t(layer \"F.Fab\")",
            "\t\t\t(uuid \"prop-u3-val\")",
            "\t\t)",
            "\t\t(pad \"1\" smd roundrect",
            "\t\t\t(at -2.475 -1.905 90)",
            "\t\t\t(size 1.5 0.6)",
            "\t\t\t(layers \"F.Cu\" \"F.Mask\" \"F.Paste\")",
            "\t\t\t(net \"GND\")",
            "\t\t\t(uuid \"pad-u3-1\")",
            "\t\t)",
            "\t\t(fp_line",
            "\t\t\t(start -2.5 -2.5)",
            "\t\t\t(end 2.5 -2.5)",
            "\t\t\t(layer \"F.SilkS\")",
            "\t\t\t(uuid \"line-u3\")",
            "\t\t)",
            "\t)",
            "\t(footprint \"Resistor_SMD:R_0402_1005Metric\"",
            "\t\t(layer \"B.Cu\")",
            "\t\t(uuid \"fp-r1\")",
            "\t\t(at 10.5 20.25)",
            "\t\t(property \"Reference\" \"R1\"",
            "\t\t\t(at 0 -1.16 0)",
            "\t\t\t(layer \"B.SilkS\")",
            "\t\t\t(uuid \"prop-r1-ref\")",
            "\t\t)",
            "\t\t(property \"Value\" \"10k\"",
            "\t\t\t(at 0 1.16 0)",
            "\t\t\t(layer \"B.Fab\")",
            "\t\t\t(uuid \"prop-r1-val\")",
            "\t\t)",
            "\t\t(pad \"1\" smd rect",
            "\t\t\t(at -0.485 0)",
            "\t\t\t(size 0.6 0.5)",
            "\t\t\t(net \"+3V3\")",
            "\t\t\t(uuid \"pad-r1-1\")",
            "\t\t)",
            "\t\t(pad \"2\" smd rect",
            "\t\t\t(at 0.485 0)",
            "\t\t\t(size 0.6 0.5)",
            "\t\t\t(uuid \"pad-r1-2\")",
            "\t\t)",
            "\t)",
            "\t(footprint \"Connector:USB_C\"",
            "\t\t(layer \"F.Cu\")",
            "\t\t(uuid \"fp-j1\")",
            "\t\t(at 50 60)",
            "\t\t(property \"Reference\" \"J1\"",
            "\t\t\t(at 0 -6.36 0)",
            "\t\t\t(uuid \"prop-j1-ref\")",
            "\t\t)",
            "\t\t(zone",
            "\t\t\t(layer \"B.Cu\")",
            "\t\t\t(uuid \"keepout-j1\")",
            "\t\t\t(polygon",
            "\t\t\t\t(pts",
            "\t\t\t\t\t(xy 47.23 55.365) (xy 52.77 55.365) (xy 52.77 64.635)",
            "\t\t\t\t)",
            "\t\t\t)",
            "\t\t)",
            "\t)",
            "\t(segment",
            "\t\t(start 1 1)",
            "\t\t(end 2 2)",
            "\t\t(width 0.2)",
            "\t\t(layer \"F.Cu\")",
            "\t\t(net \"GND\")",
            "\t\t(uuid \"seg-1\")",
            "\t)",
            ")",
            "",
        ]
        .join("\n")
    }

    /// The same board written the way this crate's own writer does it — two
    /// spaces — plus one footprint squeezed onto a single line. Nothing here may
    /// depend on the indentation.
    fn space_indented_board() -> String {
        [
            "(kicad_pcb",
            "  (version 20260206)",
            "  (footprint \"Resistor_SMD:R_0402_1005Metric\"",
            "    (layer \"F.Cu\")",
            "    (uuid \"fp-r9\")",
            "    (at 3.0 4.0)",
            "    (property \"Reference\" \"R9\" (at 0 -1.16 0) (uuid \"p-r9\"))",
            "    (pad \"1\" smd rect (at -0.485 0) (uuid \"pad-r9\"))",
            "  )",
            "  (footprint \"Capacitor_SMD:C_0402\" (layer \"F.Cu\") (uuid \"fp-c9\") \
             (at 7 8 180) (property \"Reference\" \"C9\" (at 0 1 180)))",
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

    fn fp_by_ref(content: &str, reference: &str) -> BoardFootprint {
        collect_footprints(content)
            .into_iter()
            .find(|f| f.reference.as_deref() == Some(reference))
            .unwrap_or_else(|| panic!("no footprint {reference}"))
    }

    /// The text of one footprint block, for byte-level comparisons.
    fn fp_text(content: &str, reference: &str) -> String {
        let fp = fp_by_ref(content, reference);
        content[fp.start..fp.end].to_string()
    }

    // ─── Block location ───────────────────────────────────────────────────────

    #[test]
    fn collect_footprints_sees_only_real_top_level_footprints() {
        let content = kicad10_board();
        let fps = collect_footprints(&content);
        // The `(footprint` inside U3's description string is not a block, and
        // the unbalanced `(` in the gr_text must not shift the depth counter.
        assert_eq!(fps.len(), 3, "{fps:#?}");
        let refs: Vec<&str> = fps.iter().filter_map(|f| f.reference.as_deref()).collect();
        assert_eq!(refs, vec!["U3", "R1", "J1"]);
        for fp in &fps {
            assert!(content[fp.start..fp.end].starts_with("(footprint"));
            assert!(content[fp.start..fp.end].ends_with(')'));
        }
    }

    #[test]
    fn a_nested_at_is_never_mistaken_for_the_footprints_own() {
        let content = kicad10_board();
        let u3 = fp_by_ref(&content, "U3");
        let at = u3.at.as_ref().expect("U3 has an (at …)");
        // U3's first *textual* `(at` is its Reference property's `(at 0 -3.4 90)`
        // and the description mentions `(at 1 2)`. Neither is the footprint's.
        assert_eq!((at.x, at.y, at.angle), (106.8, 89.0, 90.0));
        assert_eq!(&content[at.start..at.end], "(at 106.8 89 90)");
    }

    #[test]
    fn footprint_metadata_is_read_off_the_file() {
        let content = kicad10_board();
        let u3 = fp_by_ref(&content, "U3");
        assert_eq!(
            u3.library_id.as_deref(),
            Some("Package_SO:SOIC-8_3.9x4.9mm_P1.27mm")
        );
        assert_eq!(u3.uuid.as_deref(), Some("fp-u3"));
        assert_eq!(u3.layer.as_deref(), Some("F.Cu"));
        assert_eq!(u3.value.as_deref(), Some("LM358"));
        assert_eq!(u3.pad_count, 1);

        let r1 = fp_by_ref(&content, "R1");
        assert_eq!(r1.layer.as_deref(), Some("B.Cu"));
        assert_eq!(r1.pad_count, 2);
        // A footprint whose `(at …)` has no angle is at 0°, not "unknown".
        assert_eq!(r1.at.as_ref().map(|a| a.angle), Some(0.0));
        assert!(!r1.at.as_ref().unwrap().has_angle);
    }

    #[test]
    fn indentation_is_never_assumed() {
        let content = space_indented_board();
        let fps = collect_footprints(&content);
        assert_eq!(fps.len(), 2, "{fps:#?}");
        let r9 = fp_by_ref(&content, "R9");
        assert_eq!(r9.at.as_ref().map(|a| (a.x, a.y)), Some((3.0, 4.0)));
        // The single-line footprint parses exactly like the indented ones.
        let c9 = fp_by_ref(&content, "C9");
        assert_eq!(
            c9.at.as_ref().map(|a| (a.x, a.y, a.angle)),
            Some((7.0, 8.0, 180.0))
        );
    }

    // ─── Reads ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_component_list_reads_the_board_file() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let res =
            handle_get_component_list(&json!({ "board": board.to_str().unwrap() }), &test_ctx())
                .await
                .unwrap();
        assert!(!res.is_error, "{:?}", body(&res));
        let b = body(&res);
        assert_eq!(b["count"], json!(3));
        assert_eq!(b["source"], json!("file"));
        assert_eq!(b["components"][0]["reference"], json!("U3"));
        assert_eq!(b["components"][0]["x"], json!(106.8));
        assert_eq!(b["components"][0]["rotation"], json!(90.0));
        assert_eq!(b["components"][1]["layer"], json!("B.Cu"));

        // Filtering happens on the file's own layer field.
        let back = handle_get_component_list(
            &json!({ "board": board.to_str().unwrap(), "layer": "B.Cu" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert_eq!(body(&back)["count"], json!(1));
    }

    #[tokio::test]
    async fn find_component_takes_a_reference_or_a_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let ctx = test_ctx();

        let by_ref = handle_find_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "U3" }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!by_ref.is_error, "{:?}", body(&by_ref));
        assert_eq!(body(&by_ref)["uuid"], json!("fp-u3"));

        let by_uuid = handle_find_component(
            &json!({ "board": board.to_str().unwrap(), "uuid": "fp-u3" }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(body(&by_uuid)["reference"], json!("U3"));
        assert_eq!(body(&by_uuid)["value"], json!("LM358"));
    }

    // ─── Move ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn move_round_trip_leaves_the_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();

        let moved = handle_move_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "U3", "x": 120.5, "y": 42.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!moved.is_error, "{:?}", body(&moved));
        let b = body(&moved);
        assert_eq!(b["source"], json!("file"));
        // No KiCAD to synchronise with: the write still lands, which is what
        // makes the toolset work headless.
        assert_eq!(b["kicad_sync"], json!("not_open"));
        assert_eq!(b["previous_x"], json!(106.8));
        assert_eq!(b["previous_y"], json!(89.0));

        let after = std::fs::read_to_string(&board).unwrap();
        assert_ne!(after, original);
        assert!(after.contains("(at 120.5 42 90)"));

        // Back to where it started.
        let back = handle_move_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "U3", "x": 106.8, "y": 89.0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(!back.is_error, "{:?}", body(&back));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn move_changes_only_the_footprints_own_at() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        handle_move_component(
            &json!({ "board": board.to_str().unwrap(), "uuid": "fp-u3", "x": 200.0, "y": 300.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();

        let after = std::fs::read_to_string(&board).unwrap();
        // Property and pad offsets are relative to the footprint origin, so a
        // move must not touch them. Measured on 481 real KiCAD moves: a move is
        // exactly one changed line.
        let changed: Vec<(&str, &str)> = original
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(
            changed,
            vec![("\t\t(at 106.8 89 90)", "\t\t(at 200 300 90)")],
            "a move must change exactly the footprint's own (at …)"
        );
        // Every other footprint is untouched byte for byte.
        assert_eq!(fp_text(&after, "R1"), fp_text(&original, "R1"));
        assert_eq!(fp_text(&after, "J1"), fp_text(&original, "J1"));
    }

    #[tokio::test]
    async fn move_refuses_a_footprint_whose_keepout_is_in_board_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        let res = handle_move_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "J1", "x": 1.0, "y": 2.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let msg = error_text(&res);
        assert!(msg.contains("(zone)"), "{msg}");
        assert!(msg.contains("Nothing was written"), "{msg}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    // ─── Rotate ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rotate_turns_the_footprint_and_its_child_angles_but_not_their_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        // 90° → −90°, the exact rotation measured in KiCAD's own save history,
        // where it wrote `(at 172.2 108 -90)` and turned every property with it.
        let res = handle_rotate_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "U3", "rotation": -90.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", body(&res));
        assert_eq!(body(&res)["previous_rotation"], json!(90.0));

        let after = std::fs::read_to_string(&board).unwrap();
        let u3 = fp_text(&after, "U3");
        // The footprint's own angle, its position untouched.
        assert!(u3.contains("(at 106.8 89 -90)"), "{u3}");
        // Each child angle advanced by the same −180°; the offsets did not move.
        assert!(u3.contains("(at 0 -3.4 -90)"), "reference field: {u3}");
        assert!(u3.contains("(at 0 3.4 90)"), "value field (270−180): {u3}");
        assert!(u3.contains("(at -2.475 -1.905 -90)"), "pad: {u3}");
        // Graphics carry no angle and are left exactly as they were.
        assert!(u3.contains("(start -2.5 -2.5)"), "{u3}");
        // Other footprints untouched.
        assert_eq!(fp_text(&after, "R1"), fp_text(&original, "R1"));
    }

    #[tokio::test]
    async fn rotate_gives_an_angle_less_pad_the_footprints_new_angle() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());

        handle_rotate_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "R1", "rotation": 90.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();

        let after = std::fs::read_to_string(&board).unwrap();
        let r1 = fp_text(&after, "R1");
        assert!(r1.contains("(at 10.5 20.25 90)"), "{r1}");
        // An omitted angle means 0, so a rotated rect pad has to gain one — or
        // it would render unrotated under a rotated footprint.
        assert!(r1.contains("(at -0.485 0 90)"), "{r1}");
        assert!(r1.contains("(at 0.485 0 90)"), "{r1}");
        // Properties that already carried an explicit 0 are advanced in place.
        assert!(r1.contains("(at 0 -1.16 90)"), "{r1}");
        assert!(r1.contains("(at 0 1.16 90)"), "{r1}");
    }

    #[tokio::test]
    async fn rotating_by_a_full_turn_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        // 450° wraps to 90°, which U3 already has.
        let res = handle_rotate_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "U3", "rotation": 450.0 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", body(&res));
        assert_eq!(body(&res)["rotation"], json!(90.0));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    // ─── Delete ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_removes_exactly_one_footprint_and_leaves_the_rest_intact() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        let res = handle_delete_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "R1" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", body(&res));
        assert_eq!(body(&res)["component"]["pad_count"], json!(2));

        let after = std::fs::read_to_string(&board).unwrap();
        // Still a board.
        assert!(check_document(&after, "kicad_pcb").is_ok());
        let left = collect_footprints(&after);
        assert_eq!(left.len(), 2);
        // The survivors are byte-identical, and so is everything around them.
        assert_eq!(fp_text(&after, "U3"), fp_text(&original, "U3"));
        assert_eq!(fp_text(&after, "J1"), fp_text(&original, "J1"));
        assert!(after.contains("(uuid \"seg-1\")"), "the trace survived");
        // Removing the block and its leading indentation leaves no blank line.
        assert!(!after.contains("\n\t\n"), "stray blank line: {after}");
        assert!(!after.contains("R_0402"), "the footprint really went");
    }

    // ─── Edit ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_component_sets_the_value_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        let res = handle_edit_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "R1", "value": "4k7" }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", body(&res));
        assert_eq!(body(&res)["changed"][0]["from"], json!("10k"));

        let after = std::fs::read_to_string(&board).unwrap();
        assert!(after.contains("(property \"Value\" \"4k7\""));
        // One line changed, nothing else.
        let changed = original
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 1);
    }

    #[tokio::test]
    async fn edit_component_refuses_a_property_the_footprint_does_not_have() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        let res = handle_edit_component(
            &json!({
                "board": board.to_str().unwrap(), "reference": "R1",
                "properties": { "MPN": "RC0402FR-074K7L" }
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let msg = error_text(&res);
        assert!(msg.contains("no property 'MPN'"), "{msg}");
        assert!(msg.contains("Reference, Value"), "{msg}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn edit_component_refuses_to_rename_a_designator() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        let res = handle_edit_component(
            &json!({
                "board": board.to_str().unwrap(), "reference": "R1",
                "properties": { "Reference": "R99" }
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        // The designator lives in the schematic; changing it only here is undone
        // by the next PCB sync.
        assert!(error_text(&res).contains("edit_schematic_component"));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    // ─── Align ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn align_moves_every_named_footprint_in_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());

        let res = handle_align_components(
            &json!({
                "board": board.to_str().unwrap(),
                "references": ["U3", "R1"], "axis": "y", "value": 55.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(!res.is_error, "{:?}", body(&res));
        assert_eq!(body(&res)["aligned_count"], json!(2));

        let after = std::fs::read_to_string(&board).unwrap();
        assert!(after.contains("(at 106.8 55 90)"), "{after}");
        assert!(after.contains("(at 10.5 55)"), "{after}");
    }

    #[tokio::test]
    async fn align_is_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);

        let res = handle_align_components(
            &json!({
                "board": board.to_str().unwrap(),
                "references": ["U3", "NOPE"], "axis": "x", "value": 5.0
            }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let msg = error_text(&res);
        assert!(msg.contains("No footprint was moved"), "{msg}");
        // U3 came first and resolved fine; it must still not have moved.
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    // ─── Refusals ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_unknown_reference_refuses_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let original = kicad10_board();
        let board = board_file(dir.path(), &original);
        let ctx = test_ctx();
        let b = board.to_str().unwrap();

        let calls: Vec<CallToolResult> = vec![
            handle_move_component(
                &json!({ "board": b, "reference": "U9", "x": 1, "y": 2 }),
                &ctx,
            )
            .await
            .unwrap(),
            handle_rotate_component(
                &json!({ "board": b, "reference": "U9", "rotation": 90 }),
                &ctx,
            )
            .await
            .unwrap(),
            handle_delete_component(&json!({ "board": b, "reference": "U9" }), &ctx)
                .await
                .unwrap(),
            handle_edit_component(
                &json!({ "board": b, "reference": "U9", "value": "x" }),
                &ctx,
            )
            .await
            .unwrap(),
            handle_find_component(&json!({ "board": b, "reference": "U9" }), &ctx)
                .await
                .unwrap(),
        ];
        for res in &calls {
            let msg = error_text(res);
            assert!(msg.contains("No footprint with reference 'U9'"), "{msg}");
            // The message names what is actually there instead of dead-ending.
            assert!(msg.contains("U3"), "{msg}");
        }
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
    }

    #[tokio::test]
    async fn a_uuid_passed_as_a_reference_is_named_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let res = handle_move_component(
            &json!({ "board": board.to_str().unwrap(), "reference": "fp-u3", "x": 1, "y": 2 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        let msg = error_text(&res);
        assert!(msg.contains("is the uuid of footprint 'U3'"), "{msg}");
        assert!(msg.contains("Pass it as uuid"), "{msg}");
    }

    #[tokio::test]
    async fn naming_no_component_at_all_is_an_argument_error() {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(dir.path(), &kicad10_board());
        let res = handle_move_component(
            &json!({ "board": board.to_str().unwrap(), "x": 1, "y": 2 }),
            &test_ctx(),
        )
        .await
        .unwrap();
        assert!(res.is_error);
    }

    // ─── The primitives themselves ────────────────────────────────────────────

    #[test]
    fn an_at_node_with_a_non_numeric_field_is_refused_not_guessed_at() {
        // Unknown syntax: better a failed tool call than a rewritten field.
        assert!(parse_at("(at 1 2 3)", 0).is_some());
        assert!(parse_at("(at 1 2)", 0).is_some());
        assert!(parse_at("(at 1)", 0).is_none());
        assert!(parse_at("(at 1 2 3 4)", 0).is_none());
        assert!(parse_at("(at 1 2 unlocked)", 0).is_none());
        assert!(parse_at("(atom 1 2)", 0).is_none());
    }

    #[test]
    fn setting_a_number_to_what_it_already_says_edits_nothing() {
        // Keeps the file's own formatting: a board written by another tool may
        // say `157.150000`, and a move that does not change x must not restyle it.
        let content = "(at 157.150000 2)";
        let at = parse_at(content, 0).unwrap();
        assert!(at.set_token(content, 0, 157.15).is_none());
        assert!(at.set_token(content, 1, 3.0).is_some());
    }

    #[test]
    fn delete_range_refuses_a_range_that_is_not_the_block() {
        let content = kicad10_board();
        let fp = fp_by_ref(&content, "R1");
        let board = std::path::Path::new("/tmp/x.kicad_pcb");
        assert!(delete_range(&content, fp.start, fp.end, "R1", board).is_ok());
        // A start that is not a block opening must not silently delete from 0 —
        // the defect that once erased a whole file.
        let err = delete_range(&content, fp.start, fp.end - 1, "R1", board).unwrap_err();
        assert!(err.contains("Refusing to delete"), "{err}");
    }
}

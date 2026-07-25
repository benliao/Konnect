//! Library symbol resolution — loads symbol definitions from KiCAD's installed libraries.
//!
//! KiCAD 10 stores symbols in `.kicad_symdir` directories:
//! ```text
//! C:\KiCad\10.0\share\kicad\symbols\Device.kicad_symdir\R.kicad_sym
//! C:\KiCad\10.0\share\kicad\symbols\power.kicad_symdir\VCC.kicad_sym
//! ```
//!
//! This module resolves a `lib_id` like `"Device:R"` to the full symbol S-expression
//! definition, and can inject it into a Schematic's `lib_symbols` section.

use crate::sexp::{parser, SexpNode};
use crate::Schematic;
use std::path::{Path, PathBuf};

/// Resolve a lib_id (e.g. "Device:R") to the full symbol S-expression string.
/// The returned string is the raw content of the `(symbol "R" ...)` block,
/// with the name prefixed as `"Device:R"`.
pub fn resolve_lib_symbol(lib_id: &str) -> Option<String> {
    resolve_lib_symbol_in(lib_id, None)
}

/// Resolve a lib_id, consulting `project_dir`'s `sym-lib-table` first.
///
/// The symbol library tables are the source of truth — that is where KiCAD
/// records project-local and user-registered libraries. Scanning the install
/// directories is only a fallback; relying on it alone made every custom
/// library unresolvable and forced users to copy `.kicad_sym` files into the
/// KiCAD app bundle.
pub fn resolve_lib_symbol_in(lib_id: &str, project_dir: Option<&Path>) -> Option<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return None;
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);

    // 1. The library tables.
    if let Some(path) = library_path_from_tables(library_name, project_dir) {
        // A table entry names either a single-file library or a KiCAD 10
        // symdir; try the per-symbol file inside the latter.
        for candidate in [path.join(format!("{}.kicad_sym", symbol_name)), path] {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if let Some(block) = extract_symbol_block(&content, symbol_name) {
                    return Some(prefix_symbol_block(&block, library_name, symbol_name));
                }
            }
        }
    }

    // 2. Fallback: scan the installed symbol directories.
    for base_dir in find_symbol_dirs() {
        // KiCAD 10: Library.kicad_symdir/SymbolName.kicad_sym, then the
        // KiCAD 8/9 fallback: a single Library.kicad_sym file.
        let candidates = [
            base_dir
                .join(format!("{}.kicad_symdir", library_name))
                .join(format!("{}.kicad_sym", symbol_name)),
            base_dir.join(format!("{}.kicad_sym", library_name)),
        ];
        for path in candidates {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(block) = extract_symbol_block(&content, symbol_name) {
                return Some(prefix_symbol_block(&block, library_name, symbol_name));
            }
        }
    }
    None
}

/// Qualify a raw library symbol block for embedding in a schematic: the outer
/// symbol name and any `(extends "Parent")` gain the `Library:` prefix.
///
/// Unit sub-symbols ("Name_0_1", "Name_1_1") must stay UNPREFIXED: eeschema
/// names only the outer symbol with the library prefix and refuses to load a
/// schematic whose units carry it ("Failed to load schematic" — verified
/// against kicad-cli 10.0 and the KiCAD demo corpus, which embeds units
/// without the prefix).
fn prefix_symbol_block(block: &str, library_name: &str, symbol_name: &str) -> String {
    // `extract_symbol_block` returns a block that starts with its own header,
    // so this rewrites the outer name and nothing else.
    let mut out = block.replacen(
        &format!("(symbol \"{}\"", symbol_name),
        &format!("(symbol \"{}:{}\"", library_name, symbol_name),
        1,
    );
    if let Some((parent, start, end)) = find_extends(&out) {
        if !parent.contains(':') {
            out.replace_range(start..end, &format!("\"{}:{}\"", library_name, parent));
        }
    }
    out
}

/// The `(extends "Parent")` parent name and the byte range of its quoted
/// argument. Scans string-aware so an `(extends "` sequence sitting inside a
/// description value is not mistaken for the real tag.
fn find_extends(block: &str) -> Option<(String, usize, usize)> {
    const TAG: &[u8] = b"(extends";
    let b = block.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i = end_of_string(b, i);
                continue;
            }
            b'(' if b.get(i..i + TAG.len()) == Some(TAG) => {
                let mut j = i + TAG.len();
                while matches!(b.get(j), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let end = end_of_string(b, j);
                    let parent = block.get(j + 1..end.saturating_sub(1))?.to_string();
                    return Some((parent, j, end));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Resolve a lib_id to a parsed SexpNode tree.
pub fn resolve_lib_symbol_node(lib_id: &str) -> Option<SexpNode> {
    let raw = resolve_lib_symbol(lib_id)?;
    parser::parse(&raw).ok()
}

/// Ensure a library symbol definition is present in the schematic's lib_symbols section.
/// If the symbol is already present (by name), does nothing.
/// If the lib_symbols node doesn't exist in raw_other, creates one.
/// Handles `(extends "ParentName")` — automatically embeds the parent symbol too.
///
/// Returns `false` when `lib_id` cannot be resolved from the installed
/// libraries — callers MUST surface that as an error: a symbol instance
/// without an embedded definition is invisible to KiCAD's netlister and
/// yields empty pin lists downstream (#34).
#[must_use]
pub fn ensure_lib_symbol(schematic: &mut Schematic, lib_id: &str) -> bool {
    // Check if already present
    let check_name = format!("\"{}\"", lib_id);
    let already_present = schematic.raw_other.iter().any(|node| {
        if node.tag() == Some("lib_symbols") {
            let content = format!("{:?}", node);
            content.contains(&check_name)
        } else {
            false
        }
    });
    if already_present {
        return true;
    }

    // A schematic sits next to its project's sym-lib-table, so its own
    // directory is the project scope for library resolution.
    let project_dir = schematic.filepath().parent().map(|p| p.to_path_buf());

    // Resolve the symbol's raw text to check for (extends "ParentName")
    let sym_raw = match resolve_lib_symbol_in(lib_id, project_dir.as_deref()) {
        Some(r) => r,
        None => return false,
    };

    // Check for (extends "ParentName") and resolve the parent too.
    // Note: sym_raw already has prefixed names (e.g. extends "MCU_Microchip_ATmega:ATmega48PV-10A")
    // so we use the prefixed parent name directly as the lib_id for the recursive call.
    if let Some(extends_pos) = sym_raw.find("(extends \"") {
        let after = &sym_raw[extends_pos + 10..];
        if let Some(end) = after.find('"') {
            let parent_lib_id = &after[..end]; // Already has library prefix
            if parent_lib_id.contains(':') {
                // The child resolved, so its parent lives in the same library
                // file; a failure here would be a broken library, not a bad
                // lib_id from the caller.
                let _ = ensure_lib_symbol(schematic, parent_lib_id);
            }
        }
    }

    // Now resolve and embed the symbol itself
    let sym_node = match parser::parse(&sym_raw).ok() {
        Some(n) => n,
        None => return false,
    };

    // Find or create the lib_symbols node
    let lib_syms_idx = schematic
        .raw_other
        .iter()
        .position(|n| n.tag() == Some("lib_symbols"));

    match lib_syms_idx {
        Some(idx) => {
            // Append the symbol to the existing lib_symbols list
            if let SexpNode::List(ref mut children) = schematic.raw_other[idx] {
                children.push(sym_node);
            }
        }
        None => {
            // Create a new lib_symbols node with this symbol
            let lib_syms =
                SexpNode::List(vec![SexpNode::Atom("lib_symbols".to_string()), sym_node]);
            // Insert at the beginning of raw_other (lib_symbols should come early)
            schematic.raw_other.insert(0, lib_syms);
        }
    }
    true
}

/// Whether `library_name` (e.g. "Device") is registered in a symbol library
/// table or present in an installed symbol dir, in either the KiCAD 10 symdir
/// layout or the legacy single-file one.
pub fn library_exists(library_name: &str) -> bool {
    library_exists_in(library_name, None)
}

/// [`library_exists`], also consulting `project_dir`'s `sym-lib-table`.
pub fn library_exists_in(library_name: &str, project_dir: Option<&Path>) -> bool {
    if library_path_from_tables(library_name, project_dir).is_some_and(|p| p.exists()) {
        return true;
    }
    find_symbol_dirs().iter().any(|base| {
        base.join(format!("{}.kicad_symdir", library_name)).is_dir()
            || base.join(format!("{}.kicad_sym", library_name)).is_file()
    })
}

/// Symbol names similar to the one in `lib_id`, for did-you-mean hints when a
/// lib_id doesn't resolve (#34: LLM callers habitually reach for KiCAD ≤9
/// names like `Device:CP` that KiCAD 10 renamed). Returns full `Library:Name`
/// ids, closest first, at most `limit`.
pub fn suggest_symbols(lib_id: &str, limit: usize) -> Vec<String> {
    let parts: Vec<&str> = lib_id.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Vec::new();
    }
    let (library_name, symbol_name) = (parts[0], parts[1]);
    let wanted = symbol_name.to_lowercase();

    let mut candidates: Vec<String> = Vec::new();
    for base in find_symbol_dirs() {
        let symdir = base.join(format!("{}.kicad_symdir", library_name));
        if let Ok(entries) = std::fs::read_dir(&symdir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("kicad_sym") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        candidates.push(stem.to_string());
                    }
                }
            }
        }
        // Legacy single-file library: one pass over its top-level symbols.
        let legacy = base.join(format!("{}.kicad_sym", library_name));
        if let Ok(content) = std::fs::read_to_string(&legacy) {
            candidates.extend(top_level_symbol_names(&content));
        }
    }
    candidates.sort();
    candidates.dedup();

    rank_candidates(&wanted, candidates, limit)
        .into_iter()
        .map(|name| format!("{}:{}", library_name, name))
        .collect()
}

/// Rank `candidates` by similarity to `wanted` (already lowercased), keeping
/// at most `limit`, closest first. Pure so it's unit-testable without an
/// installed KiCAD.
fn rank_candidates(wanted: &str, candidates: Vec<String>, limit: usize) -> Vec<String> {
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .filter_map(|name| {
            let lower = name.to_lowercase();
            // Stylized matches cover the classic KiCAD ≤9 shorthands the
            // renames expanded (CP → C_Polarized, R_POT_TRIM →
            // R_Potentiometer_Trim); substring containment covers truncations;
            // otherwise edit distance, capped so unrelated names don't surface.
            let dist = if stylized_match(wanted, &lower)
                || lower.contains(wanted)
                || wanted.contains(&lower)
            {
                1
            } else {
                edit_distance(wanted, &lower)
            };
            let cutoff = (wanted.len().max(lower.len()) * 2).div_ceil(3);
            (dist <= cutoff).then_some((dist, name))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, n)| n).collect()
}

/// Shorthand relationships between a wanted name and a candidate (both
/// lowercase): the wanted name is the candidate's initials ("cp" vs
/// "c_polarized"), or both split into the same number of `_` tokens with each
/// wanted token a prefix of the candidate's ("r_pot_trim" vs
/// "r_potentiometer_trim").
fn stylized_match(wanted: &str, cand: &str) -> bool {
    let toks = |s: &str| -> Vec<String> {
        s.split(['_', '-', '.'])
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (w, c) = (toks(wanted), toks(cand));
    if w.len() == 1 && c.len() >= 2 {
        let initials: String = c.iter().filter_map(|t| t.chars().next()).collect();
        if initials == w[0] {
            return true;
        }
    }
    !w.is_empty() && w.len() == c.len() && w.iter().zip(&c).all(|(a, b)| b.starts_with(a.as_str()))
}

/// Plain Levenshtein distance, O(len(a)·len(b)) with a single-row table.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev_diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            let val = (prev_diag + cost).min(row[j] + 1).min(row[j + 1] + 1);
            prev_diag = row[j + 1];
            row[j + 1] = val;
        }
    }
    row[b.len()]
}

/// A `(symbol "NAME" ...)` block located in a `.kicad_sym` file.
struct SymbolBlock {
    name: String,
    /// Byte offset of the opening `(`.
    start: usize,
    /// Byte offset one past the matching `)`. Equal to `start` while the block
    /// is still open (i.e. never closed — a truncated file).
    end: usize,
    /// List nesting depth, 1 for the outermost list in the file.
    depth: usize,
}

/// Byte offset one past the closing quote of the string starting at `i`
/// (`b[i]` must be `"`). Backslash escapes the next byte, so `\"` stays inside
/// the string. Runs to the end on an unterminated string.
fn end_of_string(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// If the list opening at `i` is `(symbol "NAME" ...)`, the name and the offset
/// just past its closing quote.
fn symbol_header_at(b: &[u8], i: usize) -> Option<String> {
    const TAG: &[u8] = b"symbol";
    let mut j = i + 1;
    if b.get(j..j + TAG.len())? != TAG {
        return None;
    }
    j += TAG.len();
    // The tag must end here — `symbol_name` etc. is a different tag.
    if !matches!(b.get(j), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        return None;
    }
    while matches!(b.get(j), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        j += 1;
    }
    if b.get(j) != Some(&b'"') {
        return None;
    }
    let end = end_of_string(b, j);
    // Trim the surrounding quotes; the name itself is taken verbatim, which
    // matches how the rest of this module compares it.
    let raw = b.get(j + 1..end.saturating_sub(1))?;
    Some(String::from_utf8_lossy(raw).into_owned())
}

/// Locate every `(symbol "NAME" ...)` block in a `.kicad_sym` file.
///
/// The scan tracks quoted strings and backslash escapes rather than counting
/// raw parens, because KiCAD's own libraries ship quoted values with unbalanced
/// parentheses — pin names like `PA13(JTMS` in `MCU_ST_STM32H5`/`H7`,
/// descriptions like `… scheme (pin number consists of …` in the
/// `Connector_Generic*` families, and a stray `)` in `Regulator_Switching`.
/// A raw-paren counter runs off on those and the block never closes, so the
/// symbol reads back as "not found".
fn scan_symbol_blocks(content: &str) -> Vec<SymbolBlock> {
    let b = content.as_bytes();
    let mut blocks: Vec<SymbolBlock> = Vec::new();
    // One entry per open list: the index into `blocks` if it is a symbol block.
    let mut stack: Vec<Option<usize>> = Vec::new();
    let mut i = 0usize;

    while i < b.len() {
        match b[i] {
            b'"' => i = end_of_string(b, i),
            b'(' => {
                let slot = symbol_header_at(b, i).map(|name| {
                    blocks.push(SymbolBlock {
                        name,
                        start: i,
                        end: i,
                        depth: stack.len() + 1,
                    });
                    blocks.len() - 1
                });
                stack.push(slot);
                i += 1;
            }
            b')' => {
                if let Some(Some(k)) = stack.pop() {
                    blocks[k].end = i + 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    // Drop blocks whose closing paren was never reached (truncated file).
    blocks.retain(|s| s.end > s.start);
    blocks
}

/// The outermost symbol blocks — the library's actual symbols. Unit
/// sub-symbols (`R_0_1`) nest one level deeper inside their parent and are
/// excluded, so a unit can never be mistaken for a symbol of the same name.
fn top_level_symbol_blocks(content: &str) -> Vec<SymbolBlock> {
    let mut blocks = scan_symbol_blocks(content);
    let Some(min_depth) = blocks.iter().map(|s| s.depth).min() else {
        return blocks;
    };
    blocks.retain(|s| s.depth == min_depth);
    blocks
}

/// Extract a top-level `(symbol "NAME" ...)` block from `.kicad_sym` content.
pub fn extract_symbol_block(content: &str, symbol_name: &str) -> Option<String> {
    let block = top_level_symbol_blocks(content)
        .into_iter()
        .find(|s| s.name == symbol_name)?;
    // `start`/`end` sit on ASCII parens, so these are always char boundaries.
    Some(content[block.start..block.end].to_string())
}

/// Names of every top-level symbol in `.kicad_sym` content, in file order.
pub fn top_level_symbol_names(content: &str) -> Vec<String> {
    top_level_symbol_blocks(content)
        .into_iter()
        .map(|s| s.name)
        .collect()
}

// ─── Symbol library tables ───────────────────────────────────────────────────

/// KiCAD's per-user configuration directories, newest version first.
///
/// KiCAD 6+ nests config under a version directory
/// (`~/Library/Preferences/kicad/10.0/`), so code that stops at the base
/// `kicad/` directory finds no library tables at all and silently falls back to
/// scanning the install — which is why registered custom libraries appeared to
/// be ignored entirely.
pub fn kicad_config_dirs() -> Vec<PathBuf> {
    let base = {
        #[cfg(target_os = "windows")]
        {
            std::env::var("APPDATA").ok().map(|a| PathBuf::from(a).join("kicad"))
        }
        #[cfg(target_os = "macos")]
        {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join("Library/Preferences/kicad"))
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
                .ok()
                .map(|c| c.join("kicad"))
        }
    };
    let Some(base) = base else {
        return Vec::new();
    };

    // Version subdirectories, highest first, then the base itself for the
    // pre-6 flat layout.
    let mut versions: Vec<(f64, PathBuf)> = std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.is_dir() {
                return None;
            }
            let n: f64 = p.file_name()?.to_str()?.parse().ok()?;
            Some((n, p))
        })
        .collect();
    versions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut out: Vec<PathBuf> = versions.into_iter().map(|(_, p)| p).collect();
    if base.is_dir() {
        out.push(base);
    }
    out
}

/// One `(lib (name …) (type …) (uri …))` row of a symbol library table.
struct LibTableEntry {
    name: String,
    kind: String,
    uri: String,
}

fn parse_lib_table(content: &str) -> Vec<LibTableEntry> {
    let Ok(root) = parser::parse(content) else {
        return Vec::new();
    };
    root.find_all("lib")
        .iter()
        .filter_map(|n| {
            Some(LibTableEntry {
                name: n.get_value("name")?.to_string(),
                kind: n.get_value("type").unwrap_or("KiCad").to_string(),
                uri: n.get_value("uri")?.to_string(),
            })
        })
        .collect()
}

/// Expand `${VAR}` in a library-table URI. KiCAD writes every stock entry as
/// `${KICAD10_SYMBOL_DIR}/Device.kicad_sym`, and it defines those variables
/// internally rather than in the shell this server inherits — so the symbol
/// path variables fall back to the detected install directories.
fn expand_uri_vars(uri: &str) -> String {
    let mut out = String::with_capacity(uri.len());
    let mut rest = uri;
    while let Some(open) = rest.find("${") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let var = &after[..close];
        let value = std::env::var(var).ok().or_else(|| {
            var.contains("SYMBOL_DIR")
                .then(|| find_symbol_dirs().first().map(|p| p.display().to_string()))
                .flatten()
        });
        match value {
            Some(v) => out.push_str(&v),
            // Unresolvable — keep the literal so the caller's exists() check
            // fails rather than silently pointing somewhere wrong.
            None => out.push_str(&rest[open..open + 2 + close + 1]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Symbol library tables to consult, project-local first.
fn sym_lib_tables(project_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = project_dir {
        let p = dir.join("sym-lib-table");
        if p.is_file() {
            out.push(p);
        }
    }
    for cfg in kicad_config_dirs() {
        let p = cfg.join("sym-lib-table");
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

/// The on-disk path registered for `nickname` in the symbol library tables.
///
/// Follows `(type "Table")` rows, which point at another table — KiCAD's stock
/// user table is exactly that, a pointer to the install's own table — with a
/// bounded depth so a self-referential table cannot loop.
pub fn library_path_from_tables(nickname: &str, project_dir: Option<&Path>) -> Option<PathBuf> {
    fn search(tables: Vec<PathBuf>, nickname: &str, depth: u8) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        let mut indirect = Vec::new();
        for table in tables {
            let Ok(content) = std::fs::read_to_string(&table) else {
                continue;
            };
            for entry in parse_lib_table(&content) {
                let path = PathBuf::from(expand_uri_vars(&entry.uri));
                if entry.kind.eq_ignore_ascii_case("table") {
                    indirect.push(path);
                } else if entry.name == nickname {
                    return Some(path);
                }
            }
        }
        search(indirect, nickname, depth - 1)
    }
    search(sym_lib_tables(project_dir), nickname, 4)
}

/// Find directories where KiCAD symbol libraries are stored.
pub fn find_symbol_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(dir) = std::env::var("KICAD10_SYMBOL_DIR") {
        let p = PathBuf::from(&dir);
        if p.is_dir() {
            dirs.push(p);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let candidates = [
            r"C:\KiCad\10.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\10.0\share\kicad\symbols",
            r"C:\KiCad\9.0\share\kicad\symbols",
            r"C:\Program Files\KiCad\9.0\share\kicad\symbols",
        ];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // KiCad on macOS ships its libraries inside the app bundle.
        let mut candidates = vec![
            PathBuf::from("/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"),
            PathBuf::from("/usr/local/share/kicad/symbols"),
        ];
        if let Ok(home) = std::env::var("HOME") {
            // Per-user install (KiCad.app dragged into ~/Applications)
            candidates.push(
                PathBuf::from(home)
                    .join("Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"),
            );
        }
        for p in candidates {
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let candidates = ["/usr/share/kicad/symbols", "/usr/local/share/kicad/symbols"];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() && !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    dirs
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn stylized_match_covers_the_kicad10_renames() {
        // The two shorthands from #34's repro.
        assert!(stylized_match("cp", "c_polarized"));
        assert!(stylized_match("r_pot_trim", "r_potentiometer_trim"));
        // Not everything matches.
        assert!(!stylized_match("cp", "resistor"));
        assert!(!stylized_match("irf830", "irf840"));
    }

    #[test]
    fn rank_candidates_surfaces_the_renamed_symbol() {
        let candidates = vec![
            "C".to_string(),
            "C_Polarized".to_string(),
            "C_Polarized_Small".to_string(),
            "R".to_string(),
            "L".to_string(),
        ];
        let ranked = rank_candidates("cp", candidates, 3);
        assert!(
            ranked.contains(&"C_Polarized".to_string()),
            "CP must suggest C_Polarized, got {ranked:?}"
        );
        assert!(!ranked.contains(&"R".to_string()));
    }

    #[test]
    fn rank_candidates_close_typo_and_cap() {
        let candidates = vec![
            "R_Potentiometer".to_string(),
            "R_Potentiometer_Trim".to_string(),
            "Fuse".to_string(),
        ];
        let ranked = rank_candidates("r_pot_trim", candidates, 2);
        assert_eq!(ranked.len().min(2), ranked.len(), "limit respected");
        assert_eq!(ranked[0], "R_Potentiometer_Trim");
        assert!(!ranked.contains(&"Fuse".to_string()));
    }

    /// KiCAD's own libraries ship quoted values with unbalanced parens. A
    /// raw-paren depth counter runs off on these and the symbol reads back as
    /// "not found" — Connector_Generic, MCU_ST_STM32H5/H7, Isolator and
    /// Regulator_Switching are all affected in a stock KiCAD install.
    #[test]
    fn extract_symbol_block_survives_parens_inside_strings() {
        // Unmatched '(' — the Connector_Generic / STM32 shape.
        let lib = "(kicad_symbol_lib\n\
             \t(symbol \"Conn_02x20\"\n\
             \t\t(property \"Description\" \"double row (pin number consists of\")\n\
             \t\t(symbol \"Conn_02x20_1_1\"\n\
             \t\t\t(pin passive line (at 0 0 0) (name \"PA13(JTMS\") (number \"1\"))\n\
             \t\t)\n\
             \t)\n\
             \t(symbol \"Conn_01x01\"\n\
             \t\t(property \"Value\" \"Conn_01x01\")\n\
             \t)\n\
             )\n";

        let block = extract_symbol_block(lib, "Conn_02x20")
            .expect("symbol with an unmatched '(' in a quoted value must resolve");
        assert!(block.starts_with("(symbol \"Conn_02x20\""));
        assert!(block.ends_with(')'));
        // The block must stop at its own end, not swallow the next symbol.
        assert!(!block.contains("Conn_01x01"), "block overran: {block}");
        // And it must be parseable — this is what the embed path needs.
        parser::parse(&block).expect("extracted block must parse");

        // The following symbol is still reachable.
        assert!(extract_symbol_block(lib, "Conn_01x01").is_some());

        // Unmatched ')' — the Regulator_Switching shape — must not truncate.
        let lib2 = "(kicad_symbol_lib\n\
             \t(symbol \"X\"\n\
             \t\t(property \"Description\" \"Limit 1950mA typ),), 2.7-5.5V\")\n\
             \t\t(property \"Value\" \"X\")\n\
             \t)\n\
             )\n";
        let b2 = extract_symbol_block(lib2, "X").expect("unmatched ')' must not truncate");
        assert!(b2.contains("(property \"Value\" \"X\")"), "truncated: {b2}");
        parser::parse(&b2).expect("extracted block must parse");
    }

    #[test]
    fn escaped_quote_does_not_end_a_string() {
        // Connector_Generic really does embed an escaped quote in a description.
        let lib = "(kicad_symbol_lib\n\
             \t(symbol \"A\"\n\
             \t\t(property \"Description\" \"say \\\"hi (\\\" ok\")\n\
             \t)\n\
             \t(symbol \"B\")\n\
             )\n";
        let a = extract_symbol_block(lib, "A").expect("A resolves");
        assert!(!a.contains("\"B\""), "block overran into B: {a}");
        assert!(extract_symbol_block(lib, "B").is_some());
    }

    #[test]
    fn unit_sub_symbols_are_not_mistaken_for_symbols() {
        let lib = "(kicad_symbol_lib\n\
             \t(symbol \"R\"\n\
             \t\t(symbol \"R_0_1\" (rectangle))\n\
             \t)\n\
             )\n";
        assert_eq!(top_level_symbol_names(lib), vec!["R".to_string()]);
        // A unit is not a resolvable symbol in its own right.
        assert!(extract_symbol_block(lib, "R_0_1").is_none());
    }

    #[test]
    fn prefixing_rewrites_the_outer_name_and_extends_only() {
        let block = "(symbol \"ATmega48\"\n\
             \t(extends \"ATmega8\")\n\
             \t(property \"Note\" \"see (symbol \\\"ATmega48\\\") elsewhere\")\n\
             )";
        let out = prefix_symbol_block(block, "MCU_Atmel", "ATmega48");
        assert!(out.starts_with("(symbol \"MCU_Atmel:ATmega48\""));
        assert!(out.contains("(extends \"MCU_Atmel:ATmega8\")"));
        // The property text is data, not structure — it must be left alone.
        assert!(out.contains("see (symbol \\\"ATmega48\\\") elsewhere"));
    }

    /// A library registered in a project's sym-lib-table must resolve without
    /// being copied into the KiCAD install. Previously only the install
    /// directories were scanned, so project-local libraries were invisible.
    #[test]
    fn project_local_library_resolves_from_sym_lib_table() {
        let proj = tempfile::tempdir().unwrap();
        let lib = proj.path().join("KonnectTestProjLib.kicad_sym");
        std::fs::write(
            &lib,
            "(kicad_symbol_lib\n\t(version 20251024)\n\t(symbol \"CH32V208GBU6\"\n\t\t(property \"Reference\" \"U\")\n\t)\n)\n",
        )
        .unwrap();
        std::fs::write(
            proj.path().join("sym-lib-table"),
            format!(
                "(sym_lib_table\n\t(version 7)\n\t(lib (name \"KonnectTestProjLib\") (type \"KiCad\") (uri \"{}\") (options \"\") (descr \"\"))\n)\n",
                lib.display()
            ),
        )
        .unwrap();

        // A nickname that cannot collide with anything in the host's real
        // KiCAD config, which these functions also consult.
        let got = resolve_lib_symbol_in("KonnectTestProjLib:CH32V208GBU6", Some(proj.path()))
            .expect("project-local library must resolve from its sym-lib-table");
        assert!(
            got.starts_with("(symbol \"KonnectTestProjLib:CH32V208GBU6\""),
            "{got}"
        );
        assert!(library_exists_in("KonnectTestProjLib", Some(proj.path())));

        // Without the project scope it is not findable.
        assert!(resolve_lib_symbol_in("KonnectTestProjLib:CH32V208GBU6", None).is_none());
    }

    /// KiCAD's stock user table is a `(type "Table")` row pointing at the
    /// install's own table — resolution has to follow that indirection.
    #[test]
    fn table_indirection_is_followed() {
        let root = tempfile::tempdir().unwrap();
        let lib = root.path().join("Inner.kicad_sym");
        std::fs::write(
            &lib,
            "(kicad_symbol_lib\n\t(symbol \"R\"\n\t\t(property \"Reference\" \"R\")\n\t)\n)\n",
        )
        .unwrap();

        // Reached only by following the indirection.
        let inner = root.path().join("inner-sym-lib-table");
        std::fs::write(
            &inner,
            format!(
                "(sym_lib_table\n\t(lib (name \"KonnectTestInner\") (type \"KiCad\") (uri \"{}\"))\n)\n",
                lib.display()
            ),
        )
        .unwrap();

        let proj = tempfile::tempdir().unwrap();
        std::fs::write(
            proj.path().join("sym-lib-table"),
            format!(
                "(sym_lib_table\n\t(lib (name \"Pointer\") (type \"Table\") (uri \"{}\"))\n)\n",
                inner.display()
            ),
        )
        .unwrap();

        assert_eq!(
            library_path_from_tables("KonnectTestInner", Some(proj.path())),
            Some(lib)
        );
    }

    #[test]
    fn unresolvable_uri_vars_stay_literal() {
        // Kept literal so the caller's exists() check fails, rather than
        // silently resolving to the wrong place.
        let got = expand_uri_vars("${KONNECT_DEFINITELY_UNSET_XYZZY}/Device.kicad_sym");
        assert_eq!(got, "${KONNECT_DEFINITELY_UNSET_XYZZY}/Device.kicad_sym");
    }

    #[test]
    fn ensure_lib_symbol_reports_failure_for_bogus_lib_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.kicad_sch");
        std::fs::write(
            &path,
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"test\")\n\t(lib_symbols\n\t)\n)\n",
        )
        .unwrap();
        let mut sch = Schematic::load(&path).unwrap();
        // No library named like this exists anywhere.
        assert!(!ensure_lib_symbol(
            &mut sch,
            "Definitely_Not_A_Library_xyzzy:Nope"
        ));
    }
}

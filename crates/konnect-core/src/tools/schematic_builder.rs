//! SchematicBuilder — Structured writer for .kicad_sch files.
//!
//! KiCAD 10's parser requires elements in a specific order. This builder
//! parses an existing schematic into sections, allows adding elements to
//! the correct section, and serializes back with guaranteed valid ordering.
//!
//! Element order (enforced by this builder):
//!   1. Header (version, generator, uuid, paper, title_block)
//!   2. lib_symbols (library symbol definitions)
//!   3. Junctions, no_connects
//!   4. Wires, buses, bus_entries
//!   5. Text annotations
//!   6. Labels (net_label, global_label, hierarchical_label)
//!   7. Symbol instances (ALWAYS LAST)
//!   8. Trailing blocks KiCAD writes after the symbols (sheet_instances,
//!      embedded_fonts)
//!
//! Parsing is indentation-agnostic and string-aware throughout: KiCAD indents
//! with tabs, this crate's writer used two spaces, and library strings contain
//! unbalanced parens (`"PA13(JTMS"`).

use konnect_sexp::writer::{check_document, find_balanced_block, write_atomic_checked};
use std::path::Path;
use tracing::debug;

/// Top-level tags that belong to the schematic header, in the order KiCAD
/// writes them. Everything after the last of these is a body element.
const HEADER_TAGS: &[&str] = &[
    "version",
    "generator",
    "generator_version",
    "uuid",
    "paper",
    "title_block",
];

/// Top-level tags KiCAD writes *after* the symbol instances.
const TRAILING_TAGS: &[&str] = &["sheet_instances", "symbol_instances", "embedded_fonts"];

/// Tag of the block starting at `start` (which must index a `(`).
fn block_tag(content: &str, start: usize) -> &str {
    let after = start + 1;
    let end = content[after..]
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .map(|i| after + i)
        .unwrap_or(content.len());
    &content[after..end]
}

/// Byte ranges of the direct child blocks of the block spanning
/// `block_start..block_end`.
///
/// Indentation-agnostic and string-aware: [`find_balanced_block`] ignores
/// parens inside quoted strings, which real KiCAD libraries contain in
/// abundance (pin names like `PA13(JTMS`, descriptions like
/// `scheme (pin number consists of`). Truncating the haystack at the parent's
/// closing paren keeps the scan from running past the parent.
///
/// Only valid for parents whose non-block content is bare atoms (the root and
/// `lib_symbols`); a quoted string between children would not be skipped.
fn child_blocks(content: &str, block_start: usize, block_end: usize) -> Vec<(usize, usize)> {
    let inner = &content[..block_end.saturating_sub(1)];
    let mut out = Vec::new();
    let mut pos = block_start + 1;
    while pos < inner.len() {
        match find_balanced_block(inner, pos) {
            Some((s, e)) => {
                out.push((s, e));
                pos = e;
            }
            None => break,
        }
    }
    out
}

/// Walk back over whitespace so a slice ending at `pos` has no trailing blanks.
fn trim_ws_back(content: &str, pos: usize) -> usize {
    content[..pos].trim_end().len()
}

/// Structured representation of a .kicad_sch file.
/// Each section holds raw S-expression strings that are written in order.
pub struct SchematicBuilder {
    /// Everything before lib_symbols: version, generator, uuid, paper, title_block
    pub header: String,
    /// Contents inside (lib_symbols ...) — each entry is a complete (symbol "Lib:Name" ...) block
    pub lib_symbols: Vec<String>,
    /// Junction dots
    pub junctions: Vec<String>,
    /// No-connect flags
    pub no_connects: Vec<String>,
    /// Wire segments
    pub wires: Vec<String>,
    /// Bus segments
    pub buses: Vec<String>,
    /// Bus entry points
    pub bus_entries: Vec<String>,
    /// Text annotations
    pub texts: Vec<String>,
    /// Net labels (net_label, global_label, hierarchical_label)
    pub labels: Vec<String>,
    /// Symbol instances — ALWAYS serialized last
    pub symbols: Vec<String>,
    /// Blocks KiCAD writes after the symbols (sheet_instances, embedded_fonts)
    pub trailing: Vec<String>,
}

impl Default for SchematicBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SchematicBuilder {
    /// Create an empty schematic with KiCAD 10 header.
    pub fn new() -> Self {
        let uuid = konnect_sexp::writer::new_uuid();
        SchematicBuilder {
            header: format!(
                "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(uuid \"{}\")\n\t(paper \"A4\")",
                uuid
            ),
            lib_symbols: Vec::new(),
            junctions: Vec::new(),
            no_connects: Vec::new(),
            wires: Vec::new(),
            buses: Vec::new(),
            bus_entries: Vec::new(),
            texts: Vec::new(),
            labels: Vec::new(),
            symbols: Vec::new(),
            trailing: Vec::new(),
        }
    }

    /// Parse an existing .kicad_sch file into structured sections.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::parse(&content)
    }

    /// Parse schematic content into structured sections.
    ///
    /// Indentation-agnostic: KiCAD writes tabs, this crate's writer wrote two
    /// spaces, and the old fixed-width scans (`"\n  ("`) silently matched
    /// nothing in eeschema-saved files — which made a load/save round-trip
    /// erase the whole schematic body.
    pub fn parse(content: &str) -> anyhow::Result<Self> {
        // Refuse to parse anything that is not a structurally sound schematic:
        // a truncated file used to yield a builder that serialized as a valid
        // but *empty* schematic, silently discarding the user's design.
        check_document(content, "kicad_sch")?;

        let mut builder = SchematicBuilder {
            header: String::new(),
            lib_symbols: Vec::new(),
            junctions: Vec::new(),
            no_connects: Vec::new(),
            wires: Vec::new(),
            buses: Vec::new(),
            bus_entries: Vec::new(),
            texts: Vec::new(),
            labels: Vec::new(),
            symbols: Vec::new(),
            trailing: Vec::new(),
        };

        // Split the root block into its direct children. `check_document`
        // already guaranteed a single balanced `(kicad_sch …)` root.
        let (root_start, root_end) = find_balanced_block(content, 0)
            .ok_or_else(|| anyhow::anyhow!("schematic root block is unbalanced"))?;
        let children = child_blocks(content, root_start, root_end);

        // Header = everything before the first non-header child. With no body
        // at all the header must still stop *before* the root's closing paren,
        // otherwise `to_string()` appends a second one.
        let first_body = children
            .iter()
            .position(|&(s, _)| !HEADER_TAGS.contains(&block_tag(content, s)));
        let header_end = match first_body {
            Some(i) => trim_ws_back(content, children[i].0),
            None => trim_ws_back(content, root_end - 1),
        };
        builder.header = content[..header_end].to_string();

        let body = first_body.map(|i| &children[i..]).unwrap_or(&[]);
        for &(start, end) in body {
            let tag = block_tag(content, start);

            if tag == "lib_symbols" {
                for &(s, e) in &child_blocks(content, start, end) {
                    builder.lib_symbols.push(content[s..e].trim().to_string());
                }
                continue;
            }

            let block = content[start..end].to_string();
            match tag {
                "junction" => builder.junctions.push(block),
                "no_connect" => builder.no_connects.push(block),
                "wire" => builder.wires.push(block),
                "bus" => builder.buses.push(block),
                "bus_entry" => builder.bus_entries.push(block),
                "text" => builder.texts.push(block),
                "net_label" | "global_label" | "hierarchical_label" | "label" => {
                    builder.labels.push(block)
                }
                "symbol" => builder.symbols.push(block),
                t if TRAILING_TAGS.contains(&t) => builder.trailing.push(block),
                _ => {
                    debug!(
                        "[SchematicBuilder] Unknown element type: '{}', block len: {}",
                        tag,
                        block.len()
                    );
                    builder.texts.push(block);
                }
            }
        }

        Ok(builder)
    }

    /// Add a lib_symbol definition (if not already present).
    pub fn add_lib_symbol(&mut self, definition: &str) {
        // Check if already present by matching the symbol name
        if let Some(name_start) = definition.find("(symbol \"") {
            let after = &definition[name_start + 9..];
            if let Some(name_end) = after.find('"') {
                let name = &after[..name_end];
                if self
                    .lib_symbols
                    .iter()
                    .any(|s| s.contains(&format!("(symbol \"{}\"", name)))
                {
                    return; // Already present
                }
            }
        }
        self.lib_symbols.push(definition.to_string());
    }

    /// Add a wire segment.
    pub fn add_wire(&mut self, sexp: &str) {
        self.wires.push(sexp.trim().to_string());
    }

    /// Add a junction.
    pub fn add_junction(&mut self, sexp: &str) {
        self.junctions.push(sexp.trim().to_string());
    }

    /// Add a no-connect flag.
    pub fn add_no_connect(&mut self, sexp: &str) {
        self.no_connects.push(sexp.trim().to_string());
    }

    /// Add a label (net_label, global_label, hierarchical_label).
    pub fn add_label(&mut self, sexp: &str) {
        self.labels.push(sexp.trim().to_string());
    }

    /// Add a text annotation.
    pub fn add_text(&mut self, sexp: &str) {
        self.texts.push(sexp.trim().to_string());
    }

    /// Add a symbol instance (always serialized last).
    pub fn add_symbol(&mut self, sexp: &str) {
        self.symbols.push(sexp.trim().to_string());
    }

    /// Serialize to a valid .kicad_sch string with correct element ordering.
    /// (Deliberately an inherent method — this is a file serialization, not a
    /// human-readable Display.)
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut out = String::new();

        // Header — parsed to stop before the root's closing paren, so the
        // single `)` appended at the end is the only one.
        out.push_str(self.header.trim_end());
        out.push('\n');

        // lib_symbols
        out.push_str("\t(lib_symbols\n");
        for sym in &self.lib_symbols {
            out.push_str("\t\t");
            out.push_str(sym);
            out.push('\n');
        }
        out.push_str("\t)\n");

        // Body, in the order KiCAD 10 expects. Tab indentation matches both
        // KiCAD's own writer and the header emitted by `new()`.
        let sections = [
            &self.junctions,
            &self.no_connects,
            &self.wires,
            &self.buses,
            &self.bus_entries,
            &self.texts,
            &self.labels,
            // Symbol instances — ALWAYS LAST, except for the trailing blocks
            // KiCAD itself writes after them.
            &self.symbols,
            &self.trailing,
        ];
        for section in sections {
            for item in section {
                out.push('\t');
                out.push_str(item);
                out.push('\n');
            }
        }

        // Close the root kicad_sch
        out.push_str(")\n");

        out
    }

    /// Write to file atomically (write to .tmp, fsync, rename).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let content = self.to_string();
        // Validate before replacing the user's file: a bad splice must fail the
        // call, not leave an unopenable schematic on disk.
        write_atomic_checked(path, &content, "kicad_sch")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_builder_produces_valid_structure() {
        let builder = SchematicBuilder::new();
        let output = builder.to_string();
        assert!(output.starts_with("(kicad_sch"));
        assert!(output.contains("(version 20250610)"));
        assert!(output.contains("(lib_symbols"));
        assert!(output.ends_with(")\n"));
    }

    #[test]
    fn elements_are_ordered_correctly() {
        let mut builder = SchematicBuilder::new();
        // Add in wrong order — builder should serialize in correct order
        builder.add_symbol("(symbol (lib_id \"Device:R\") (at 100 100 0) (uuid \"sym1\"))");
        builder
            .add_wire("(wire (pts (xy 100 90) (xy 100 100)) (stroke (width 0)) (uuid \"wire1\"))");
        builder.add_label("(net_label \"VCC\" (at 100 85 0) (uuid \"label1\"))");
        builder.add_junction("(junction (at 100 90) (uuid \"junc1\"))");

        let output = builder.to_string();

        // Verify order: junction < wire < label < symbol
        let junc_pos = output.find("(junction").unwrap();
        let wire_pos = output.find("(wire").unwrap();
        let label_pos = output.find("(net_label").unwrap();
        let sym_pos = output.find("(symbol").unwrap();

        assert!(junc_pos < wire_pos, "junction should come before wire");
        assert!(wire_pos < label_pos, "wire should come before label");
        assert!(label_pos < sym_pos, "label should come before symbol");
    }

    #[test]
    fn parse_and_reserialize_preserves_elements() {
        let input = r#"(kicad_sch
	(version 20250610)
	(generator "konnect")
	(generator_version "10.0")
	(paper "A4")
	(lib_symbols
	)
  (wire (pts (xy 100 90) (xy 100 100)) (stroke (width 0) (type default)) (uuid "w1"))
  (net_label "VCC" (at 100 85 0) (effects (font (size 1.27 1.27))) (uuid "l1"))
  (symbol
    (lib_id "Device:R")
    (at 100 100 0)
    (uuid "s1")
    (property "Reference" "R1" (at 100 96 0) (effects (font (size 1.27 1.27))))
    (instances (project "" (path "/" (reference "R1") (unit 1))))
  )
)
"#;

        let builder = SchematicBuilder::parse(input).unwrap();
        assert_eq!(builder.wires.len(), 1);
        assert_eq!(builder.labels.len(), 1);
        assert_eq!(builder.symbols.len(), 1);

        let output = builder.to_string();
        assert!(output.contains("(wire"));
        assert!(output.contains("(net_label"));
        assert!(output.contains("(symbol"));
    }

    /// A real eeschema-saved schematic: TAB indentation throughout. The old
    /// `find("\n  (")` scan matched nothing here, so a load/save round-trip
    /// wrote back a schematic with an empty body.
    const TAB_SCH: &str = "(kicad_sch\n\
\t(version 20250610)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
\t(uuid \"11111111-1111-1111-1111-111111111111\")\n\
\t(paper \"A4\")\n\
\t(lib_symbols\n\
\t\t(symbol \"Device:R\"\n\
\t\t\t(pin_numbers\n\t\t\t\t(hide yes)\n\t\t\t)\n\
\t\t\t(property \"Description\" \"Resistor scheme (pin number consists of\"\n\t\t\t)\n\
\t\t)\n\
\t\t(symbol \"MCU_ST_STM32H5:STM32H5\"\n\
\t\t\t(symbol \"STM32H5_1_1\"\n\
\t\t\t\t(pin bidirectional line\n\t\t\t\t\t(name \"PA13(JTMS\" (effects (font (size 1.27 1.27))))\n\t\t\t\t)\n\
\t\t\t)\n\
\t\t)\n\
\t)\n\
\t(junction\n\t\t(at 100 90)\n\t\t(uuid \"j1\")\n\t)\n\
\t(no_connect\n\t\t(at 120 90)\n\t\t(uuid \"nc1\")\n\t)\n\
\t(wire\n\t\t(pts\n\t\t\t(xy 100 90) (xy 100 100)\n\t\t)\n\t\t(uuid \"w1\")\n\t)\n\
\t(wire\n\t\t(pts\n\t\t\t(xy 100 100) (xy 120 100)\n\t\t)\n\t\t(uuid \"w2\")\n\t)\n\
\t(label \"VCC\"\n\t\t(at 100 85 0)\n\t\t(uuid \"l1\")\n\t)\n\
\t(symbol\n\t\t(lib_id \"Device:R\")\n\t\t(at 100 100 0)\n\t\t(uuid \"s1\")\n\
\t\t(property \"Reference\" \"R1\"\n\t\t\t(at 100 96 0)\n\t\t)\n\t)\n\
\t(symbol\n\t\t(lib_id \"MCU_ST_STM32H5:STM32H5\")\n\t\t(at 150 100 0)\n\t\t(uuid \"s2\")\n\t)\n\
\t(sheet_instances\n\t\t(path \"/\"\n\t\t\t(page \"1\")\n\t\t)\n\t)\n\
\t(embedded_fonts no)\n\
)\n";

    #[test]
    fn tab_indented_schematic_round_trips_without_loss() {
        let builder = SchematicBuilder::parse(TAB_SCH).unwrap();

        assert_eq!(builder.wires.len(), 2, "wires lost");
        assert_eq!(builder.symbols.len(), 2, "symbol instances lost");
        assert_eq!(builder.labels.len(), 1, "labels lost");
        assert_eq!(builder.junctions.len(), 1, "junctions lost");
        assert_eq!(builder.no_connects.len(), 1, "no_connects lost");
        assert_eq!(builder.lib_symbols.len(), 2, "lib_symbols lost");
        assert_eq!(
            builder.trailing.len(),
            2,
            "sheet_instances/embedded_fonts lost"
        );

        let output = builder.to_string();
        konnect_sexp::writer::check_document(&output, "kicad_sch").expect("output must be valid");

        for needle in [
            "(uuid \"w1\")",
            "(uuid \"w2\")",
            "(uuid \"s1\")",
            "(uuid \"s2\")",
            "(uuid \"j1\")",
            "(uuid \"nc1\")",
            "(uuid \"l1\")",
            "(lib_id \"Device:R\")",
            "(lib_id \"MCU_ST_STM32H5:STM32H5\")",
            "(sheet_instances",
            "(embedded_fonts no)",
            "(paper \"A4\")",
        ] {
            assert!(output.contains(needle), "round-trip lost {needle}");
        }

        // Re-parsing the output must be a fixed point.
        let again = SchematicBuilder::parse(&output).unwrap();
        assert_eq!(again.wires.len(), 2);
        assert_eq!(again.symbols.len(), 2);
        assert_eq!(again.lib_symbols.len(), 2);
        assert_eq!(again.to_string(), output);
    }

    #[test]
    fn lib_symbols_extent_ignores_parens_inside_strings() {
        let builder = SchematicBuilder::parse(TAB_SCH).unwrap();
        // The unbalanced "(" inside "PA13(JTMS" and "scheme (pin number
        // consists of" used to make the paren counter overshoot, swallowing
        // the symbol instances into lib_symbols.
        assert_eq!(builder.lib_symbols.len(), 2);
        assert!(builder.lib_symbols[0].starts_with("(symbol \"Device:R\""));
        assert!(builder.lib_symbols[1].contains("PA13(JTMS"));
        for ls in &builder.lib_symbols {
            assert!(
                !ls.contains("(lib_id "),
                "symbol instance swallowed into lib_symbols: {ls}"
            );
        }
    }

    #[test]
    fn truncated_schematic_errors_instead_of_panicking() {
        // Used to panic: "start byte index 1 is out of bounds of ''".
        let truncated =
            "(kicad_sch\n\t(version 20250610)\n\t(lib_symbols\n\t\t(symbol \"Device:R\"\n";
        assert!(SchematicBuilder::parse(truncated).is_err());
        assert!(SchematicBuilder::parse("").is_err());
        assert!(SchematicBuilder::parse("(kicad_pcb\n\t(version 20250610)\n)").is_err());
    }

    #[test]
    fn schematic_without_lib_symbols_is_not_doubly_closed() {
        let input =
            "(kicad_sch\n\t(version 20250610)\n\t(generator \"eeschema\")\n\t(paper \"A4\")\n)\n";
        let builder = SchematicBuilder::parse(input).unwrap();
        let output = builder.to_string();
        konnect_sexp::writer::check_document(&output, "kicad_sch")
            .expect("header-only schematic must not gain a second root close");
        assert!(!output.contains(")\n)\n)"));
        assert!(output.contains("(paper \"A4\")"));
    }

    #[test]
    fn save_round_trip_preserves_tab_indented_file() {
        let dir =
            std::env::temp_dir().join(format!("konnect-sb-{}", konnect_sexp::writer::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.kicad_sch");
        std::fs::write(&path, TAB_SCH).unwrap();

        SchematicBuilder::from_file(&path)
            .unwrap()
            .save(&path)
            .unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("(uuid \"w1\")"), "save() erased the wires");
        assert!(after.contains("(uuid \"s2\")"), "save() erased the symbols");
        assert_eq!(after.matches("(wire").count(), 2);
        assert_eq!(after.matches("(lib_id ").count(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}

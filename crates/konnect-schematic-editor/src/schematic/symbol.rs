use crate::error::{Error, Result};
use crate::sexp::{atom, qstr, tagged, SexpNode};
use crate::types::{At, Property};

fn bool_kw(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

// ---- Symbol -----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Symbol {
    pub lib_id: String,
    pub at: At,
    pub mirror: Option<String>,
    pub unit: u32,
    pub in_bom: bool,
    pub on_board: bool,
    pub dnp: bool,
    pub fields_autoplaced: bool,
    pub uuid: String,
    pub properties: Vec<Property>,
    /// `pin` and `instances` sub-nodes preserved verbatim.
    pub raw_sub_nodes: Vec<SexpNode>,
}

impl Symbol {
    /// Create a new symbol with minimal required fields.
    pub fn new(lib_id: impl Into<String>, x: f64, y: f64) -> Self {
        Symbol {
            lib_id: lib_id.into(),
            at: At::new(x, y),
            mirror: None,
            unit: 1,
            in_bom: true,
            on_board: true,
            dnp: false,
            fields_autoplaced: false,
            uuid: uuid::Uuid::new_v4().to_string(),
            properties: vec![],
            raw_sub_nodes: vec![],
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let lib_id = node
            .get_value("lib_id")
            .ok_or(Error::MissingField("lib_id"))?
            .to_owned();

        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;

        let mirror = node
            .find("mirror")
            .and_then(|n| n.value())
            .map(str::to_owned);
        let unit: u32 = node
            .get_value("unit")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let in_bom = node.get_bool("in_bom").unwrap_or(true);
        let on_board = node.get_bool("on_board").unwrap_or(true);
        let dnp = node.get_bool("dnp").unwrap_or(false);
        let fields_autoplaced = node.find("fields_autoplaced").is_some();
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();

        let properties = node
            .find_all("property")
            .iter()
            .filter_map(|n| Property::from_sexp(n))
            .collect();

        const PRESERVE: &[&str] = &["pin", "instances"];
        let raw_sub_nodes = node
            .args()
            .iter()
            .filter(|n| n.tag().map(|t| PRESERVE.contains(&t)).unwrap_or(false))
            .cloned()
            .collect();

        Ok(Symbol {
            lib_id,
            at,
            mirror,
            unit,
            in_bom,
            on_board,
            dnp,
            fields_autoplaced,
            uuid,
            properties,
            raw_sub_nodes,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("symbol")];
        c.push(tagged("lib_id", vec![qstr(self.lib_id.clone())]));
        c.push(self.at.to_sexp());
        if let Some(m) = &self.mirror {
            c.push(tagged("mirror", vec![atom(m.clone())]));
        }
        c.push(tagged("unit", vec![atom(self.unit.to_string())]));
        c.push(tagged("in_bom", vec![atom(bool_kw(self.in_bom))]));
        c.push(tagged("on_board", vec![atom(bool_kw(self.on_board))]));
        c.push(tagged("dnp", vec![atom(bool_kw(self.dnp))]));
        if self.fields_autoplaced {
            c.push(SexpNode::List(vec![atom("fields_autoplaced")]));
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        for p in &self.properties {
            c.push(p.to_sexp());
        }
        c.extend(self.raw_sub_nodes.iter().cloned());
        SexpNode::List(c)
    }

    // ---- property helpers ---------------------------------------------------

    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value.as_str())
    }

    pub fn set_property(&mut self, name: &str, value: &str) {
        if let Some(p) = self.properties.iter_mut().find(|p| p.name == name) {
            p.value = value.to_owned();
        } else {
            self.properties.push(Property::new(name, value));
        }
    }

    pub fn remove_property(&mut self, name: &str) {
        self.properties.retain(|p| p.name != name);
    }

    pub fn reference(&self) -> Option<&str> {
        self.property("Reference")
    }
    pub fn value_str(&self) -> Option<&str> {
        self.property("Value")
    }
    pub fn footprint(&self) -> Option<&str> {
        self.property("Footprint")
    }
    pub fn datasheet(&self) -> Option<&str> {
        self.property("Datasheet")
    }

    pub fn set_reference(&mut self, v: &str) {
        self.set_property("Reference", v);
    }
    pub fn set_value_str(&mut self, v: &str) {
        self.set_property("Value", v);
    }
    pub fn set_footprint(&mut self, v: &str) {
        self.set_property("Footprint", v);
    }
    /// Position of a named property's text, if it carries an `(at …)`.
    pub fn property_position(&self, name: &str) -> Option<(f64, f64)> {
        self.properties.iter().find(|p| p.name == name)?.position()
    }

    pub fn set_datasheet(&mut self, v: &str) {
        self.set_property("Datasheet", v);
    }

    // ---- instance paths -------------------------------------------------------

    /// Ensure this symbol carries an `(instances (project "name" (path "path"
    /// (reference "ref") (unit N))))` entry. Updates the entry if one already
    /// exists for this project+path, otherwise appends it (creating the
    /// `project`/`instances` wrapper nodes as needed).
    ///
    /// Needed when a sheet is linked to a sub-sheet file that already has
    /// symbols in it (a reused file, or one authored before being linked) —
    /// without this, ERC can't resolve those symbols' hierarchical references.
    pub fn set_instance_path(
        &mut self,
        project_name: &str,
        path: &str,
        reference: &str,
        unit: u32,
    ) {
        if self
            .raw_sub_nodes
            .iter()
            .position(|n| n.tag() == Some("instances"))
            .is_none()
        {
            self.raw_sub_nodes
                .push(SexpNode::List(vec![atom("instances")]));
        }
        let instances_idx = self
            .raw_sub_nodes
            .iter()
            .position(|n| n.tag() == Some("instances"))
            .expect("just ensured present");

        let SexpNode::List(instances_children) = &mut self.raw_sub_nodes[instances_idx] else {
            return;
        };

        let project_idx = instances_children
            .iter()
            .position(|c| c.tag() == Some("project") && c.value() == Some(project_name));
        if project_idx.is_none() {
            instances_children.push(SexpNode::List(vec![
                atom("project"),
                qstr(project_name.to_owned()),
            ]));
        }
        let project_idx = instances_children
            .iter()
            .position(|c| c.tag() == Some("project") && c.value() == Some(project_name))
            .expect("just ensured present");

        let SexpNode::List(project_children) = &mut instances_children[project_idx] else {
            return;
        };

        let new_path_node = SexpNode::List(vec![
            atom("path"),
            qstr(path.to_owned()),
            tagged("reference", vec![qstr(reference.to_owned())]),
            tagged("unit", vec![atom(unit.to_string())]),
        ]);
        match project_children
            .iter()
            .position(|c| c.tag() == Some("path") && c.value() == Some(path))
        {
            Some(idx) => project_children[idx] = new_path_node,
            None => project_children.push(new_path_node),
        }
    }

    /// Whether this symbol already has an instance entry for the given
    /// project name and hierarchical path.
    pub fn has_instance_path(&self, project_name: &str, path: &str) -> bool {
        self.raw_sub_nodes
            .iter()
            .find(|n| n.tag() == Some("instances"))
            .map(|inst| {
                inst.find_all("project").iter().any(|p| {
                    p.value() == Some(project_name)
                        && p.find_all("path").iter().any(|pp| pp.value() == Some(path))
                })
            })
            .unwrap_or(false)
    }

    // ---- position -----------------------------------------------------------

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }

    /// Move the symbol, carrying its Reference/Value/Footprint text with it.
    ///
    /// Field positions are absolute in KiCAD, not relative to the symbol, so
    /// moving only `self.at` leaves every field's text behind at the old
    /// location — after a bulk move the designators end up scattered across the
    /// sheet, disconnected from the parts they label.
    pub fn move_to(&mut self, x: f64, y: f64) {
        self.translate(x - self.at.x, y - self.at.y);
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.x += dx;
        self.at.y += dy;
        for prop in &mut self.properties {
            if let Some((px, py)) = prop.position() {
                prop.set_position(px + dx, py + dy);
            }
        }
    }

    /// Set the symbol's rotation, rotating its field text about the symbol
    /// origin by the same delta.
    ///
    /// A field at offset `o` from the symbol origin moves to `origin + R(Δθ)·o`.
    ///
    /// The field's own angle is left alone. That is measured behaviour, not an
    /// assumption: across eeschema-written sheets, the same library symbol at
    /// 0° and at 90° keeps an identical field angle while its offset rotates —
    /// `power:+5V` goes from `(0, 3.81)` to `(3.81, 0)` with angle `0` in both,
    /// and `CM5IO:R` keeps angle `90` in both. Rewriting the angle here would
    /// diverge from what KiCAD itself produces.
    pub fn set_rotation(&mut self, rot: f64) {
        let delta = rot - self.at.rotation.unwrap_or(0.0);
        self.at.rotation = Some(rot);
        if delta.abs() < f64::EPSILON {
            return;
        }
        // KiCAD's schematic Y axis points down, so a positive (counter-
        // clockwise) symbol rotation is clockwise in raw coordinates.
        let (sin, cos) = (-delta).to_radians().sin_cos();
        let (ox, oy) = (self.at.x, self.at.y);
        for prop in &mut self.properties {
            if let Some((px, py)) = prop.position() {
                let (dx, dy) = (px - ox, py - oy);
                prop.set_position(ox + dx * cos - dy * sin, oy + dx * sin + dy * cos);
            }
        }
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "<Symbol {} ({})>",
            self.reference().unwrap_or("?"),
            self.lib_id
        )
    }
}

// ---- SymbolCollection -------------------------------------------------------

pub struct SymbolCollection {
    symbols: Vec<Symbol>,
}

impl SymbolCollection {
    pub fn new(symbols: Vec<Symbol>) -> Self {
        SymbolCollection { symbols }
    }

    // list-like
    pub fn len(&self) -> usize {
        self.symbols.len()
    }
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Symbol> {
        self.symbols.iter()
    }
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Symbol> {
        self.symbols.iter_mut()
    }
    pub fn get(&self, i: usize) -> Option<&Symbol> {
        self.symbols.get(i)
    }
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Symbol> {
        self.symbols.get_mut(i)
    }
    pub fn as_slice(&self) -> &[Symbol] {
        &self.symbols
    }
    pub fn push(&mut self, s: Symbol) {
        self.symbols.push(s);
    }
    pub fn into_vec(self) -> Vec<Symbol> {
        self.symbols
    }

    // mutation
    pub fn remove_by_reference(&mut self, reference: &str) -> Option<Symbol> {
        let idx = self
            .symbols
            .iter()
            .position(|s| s.reference() == Some(reference))?;
        Some(self.symbols.remove(idx))
    }
    pub fn remove_by_uuid(&mut self, uuid: &str) -> Option<Symbol> {
        let idx = self.symbols.iter().position(|s| s.uuid == uuid)?;
        Some(self.symbols.remove(idx))
    }
    pub fn retain<F: FnMut(&Symbol) -> bool>(&mut self, f: F) {
        self.symbols.retain(f);
    }

    // named access
    pub fn by_reference(&self, r: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|s| s.reference() == Some(r))
    }
    pub fn by_reference_mut(&mut self, r: &str) -> Option<&mut Symbol> {
        self.symbols.iter_mut().find(|s| s.reference() == Some(r))
    }

    // filters
    pub fn reference_startswith(&self, prefix: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                s.reference()
                    .map(|r| r.starts_with(prefix))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn by_value(&self, value: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| s.value_str() == Some(value))
            .collect()
    }

    pub fn value_startswith(&self, prefix: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                s.value_str()
                    .map(|v| v.starts_with(prefix))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn by_lib_id(&self, lib_id: &str) -> Vec<&Symbol> {
        self.symbols.iter().filter(|s| s.lib_id == lib_id).collect()
    }

    // spatial
    pub fn within_circle(&self, x: f64, y: f64, radius: f64) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                let (sx, sy) = s.position();
                dist(sx, sy, x, y) <= radius
            })
            .collect()
    }

    pub fn within_rectangle(&self, x1: f64, y1: f64, x2: f64, y2: f64) -> Vec<&Symbol> {
        let (xmin, xmax) = (x1.min(x2), x1.max(x2));
        let (ymin, ymax) = (y1.min(y2), y1.max(y2));
        self.symbols
            .iter()
            .filter(|s| {
                let (sx, sy) = s.position();
                sx >= xmin && sx <= xmax && sy >= ymin && sy <= ymax
            })
            .collect()
    }

    // bulk ops
    pub fn set_all_dnp(&mut self, dnp: bool) {
        for s in &mut self.symbols {
            if s.reference().map(|r| r.starts_with('#')).unwrap_or(false) {
                continue;
            }
            s.dnp = dnp;
        }
    }
}

impl std::ops::Index<usize> for SymbolCollection {
    type Output = Symbol;
    fn index(&self, i: usize) -> &Symbol {
        &self.symbols[i]
    }
}
impl std::ops::IndexMut<usize> for SymbolCollection {
    fn index_mut(&mut self, i: usize) -> &mut Symbol {
        &mut self.symbols[i]
    }
}
impl<'a> IntoIterator for &'a SymbolCollection {
    type Item = &'a Symbol;
    type IntoIter = std::slice::Iter<'a, Symbol>;
    fn into_iter(self) -> Self::IntoIter {
        self.symbols.iter()
    }
}
impl<'a> IntoIterator for &'a mut SymbolCollection {
    type Item = &'a mut Symbol;
    type IntoIter = std::slice::IterMut<'a, Symbol>;
    fn into_iter(self) -> Self::IntoIter {
        self.symbols.iter_mut()
    }
}

fn dist(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let (dx, dy) = (ax - bx, ay - by);
    (dx * dx + dy * dy).sqrt()
}


#[cfg(test)]
mod field_transform_tests {
    use super::*;
    use crate::sexp::parser;

    /// A symbol at (100, 100) whose Reference text sits 5mm above it.
    fn sym_at(x: f64, y: f64, rot: f64) -> Symbol {
        let text = format!(
            "(symbol\n\t(lib_id \"Device:R\")\n\t(at {x} {y} {rot})\n\t(unit 1)\n\t\
             (property \"Reference\" \"R3\"\n\t\t(at {x} {} 0)\n\t)\n\t\
             (property \"Value\" \"10k\"\n\t\t(at {x} {} 0)\n\t)\n)",
            y + 5.0,
            y + 8.0
        );
        Symbol::from_sexp(&parser::parse(&text).unwrap()).unwrap()
    }

    /// Field positions are absolute; moving only the symbol used to strand
    /// every designator at its old spot.
    #[test]
    fn moving_carries_the_field_text() {
        let mut s = sym_at(100.0, 100.0, 0.0);
        s.move_to(330.2, 196.85);

        assert_eq!(s.at.x, 330.2);
        let (rx, ry) = s.property_position("Reference").unwrap();
        assert!((rx - 330.2).abs() < 1e-6, "x not carried: {rx}");
        // The 5mm offset below the symbol is preserved.
        assert!((ry - 201.85).abs() < 1e-6, "y not carried: {ry}");
    }

    #[test]
    fn translating_carries_the_field_text() {
        let mut s = sym_at(10.0, 20.0, 0.0);
        s.translate(2.54, -1.27);
        let (rx, ry) = s.property_position("Reference").unwrap();
        assert!((rx - 12.54).abs() < 1e-6 && (ry - 23.73).abs() < 1e-6, "{rx},{ry}");
    }

    /// Matches eeschema's measured behaviour: a field at offset (0, +d) at 0°
    /// sits at (+d, 0) at 90°, and its own text angle is unchanged. Verified
    /// against real sheets — power:+5V goes (0, 3.81) -> (3.81, 0), angle 0
    /// both times.
    #[test]
    fn rotating_moves_fields_the_way_eeschema_does() {
        let mut s = sym_at(100.0, 100.0, 0.0);
        s.set_rotation(90.0);

        let (rx, ry) = s.property_position("Reference").unwrap();
        assert!((rx - 105.0).abs() < 1e-6, "x: {rx}");
        assert!((ry - 100.0).abs() < 1e-6, "y: {ry}");

        let angle = s
            .properties
            .iter()
            .find(|p| p.name == "Reference")
            .and_then(|p| p.text_angle())
            .unwrap();
        assert_eq!(angle, 0.0, "eeschema leaves the field angle alone");
    }

    #[test]
    fn rotating_back_to_zero_restores_the_original_layout() {
        let mut s = sym_at(100.0, 100.0, 0.0);
        let before = s.property_position("Reference").unwrap();
        s.set_rotation(180.0);
        s.set_rotation(0.0);
        let after = s.property_position("Reference").unwrap();
        assert!(
            (before.0 - after.0).abs() < 1e-6 && (before.1 - after.1).abs() < 1e-6,
            "{before:?} != {after:?}"
        );
    }
}

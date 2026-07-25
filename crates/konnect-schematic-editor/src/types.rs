use crate::sexp::{atom, qstr, tagged, SexpNode};

// ---- float formatting -------------------------------------------------------

pub fn fmt_f64(v: f64) -> String {
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

// ---- At ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct At {
    pub x: f64,
    pub y: f64,
    pub rotation: Option<f64>,
}

impl At {
    pub fn new(x: f64, y: f64) -> Self {
        At {
            x,
            y,
            rotation: None,
        }
    }

    pub fn with_rotation(x: f64, y: f64, rotation: f64) -> Self {
        At {
            x,
            y,
            rotation: Some(rotation),
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        let s = node.scalar_args();
        let x: f64 = s.first()?.parse().ok()?;
        let y: f64 = s.get(1)?.parse().ok()?;
        let rotation = s.get(2).and_then(|v| v.parse().ok());
        Some(At { x, y, rotation })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut args = vec![atom(fmt_f64(self.x)), atom(fmt_f64(self.y))];
        if let Some(r) = self.rotation {
            args.push(atom(fmt_f64(r)));
        }
        tagged("at", args)
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }

    pub fn distance_to(&self, other: &At) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

// ---- Property ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Property {
    pub name: String,
    pub value: String,
    /// Trailing sub-nodes after name+value (at, effects, show_pin_number, …).
    pub sub_nodes: Vec<SexpNode>,
}

impl Property {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Property {
            name: name.into(),
            value: value.into(),
            sub_nodes: vec![],
        }
    }

    /// The property text's own `(at x y [angle])`, if it carries one.
    ///
    /// KiCAD stores field positions in absolute board coordinates, not relative
    /// to the symbol, so moving or rotating a symbol has to move these too.
    pub fn position(&self) -> Option<(f64, f64)> {
        let at = self.sub_nodes.iter().find(|n| n.tag() == Some("at"))?;
        Some((at.get_float_at(1)?, at.get_float_at(2)?))
    }

    /// The text angle from `(at x y angle)`, if present.
    pub fn text_angle(&self) -> Option<f64> {
        let at = self.sub_nodes.iter().find(|n| n.tag() == Some("at"))?;
        at.get_float_at(3)
    }

    /// Move the property text, preserving its angle.
    pub fn set_position(&mut self, x: f64, y: f64) {
        if let Some(SexpNode::List(children)) =
            self.sub_nodes.iter_mut().find(|n| n.tag() == Some("at"))
        {
            if children.len() > 2 {
                children[1] = SexpNode::Atom(fmt_coord(x));
                children[2] = SexpNode::Atom(fmt_coord(y));
            }
        }
    }

    /// Set the text angle in `(at x y angle)`, adding it if absent.
    pub fn set_text_angle(&mut self, angle: f64) {
        if let Some(SexpNode::List(children)) =
            self.sub_nodes.iter_mut().find(|n| n.tag() == Some("at"))
        {
            let a = SexpNode::Atom(fmt_coord(angle));
            if children.len() > 3 {
                children[3] = a;
            } else if children.len() == 3 {
                children.push(a);
            }
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        let args = node.args();
        let name = args.first()?.text()?.to_owned();
        let value = args.get(1)?.text()?.to_owned();
        let sub_nodes = args
            .iter()
            .skip(2)
            .filter(|n| n.is_list())
            .cloned()
            .collect();
        Some(Property {
            name,
            value,
            sub_nodes,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut children = vec![
            atom("property"),
            qstr(self.name.clone()),
            qstr(self.value.clone()),
        ];
        children.extend(self.sub_nodes.iter().cloned());
        SexpNode::List(children)
    }
}

// ---- Effects (preserved verbatim) ------------------------------------------

#[derive(Debug, Clone)]
pub struct Effects(pub SexpNode);

impl Effects {
    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        Some(Effects(node.clone()))
    }
    pub fn to_sexp(&self) -> SexpNode {
        self.0.clone()
    }
}

// ---- Stroke (preserved verbatim) -------------------------------------------

#[derive(Debug, Clone)]
pub struct Stroke(pub SexpNode);

impl Stroke {
    pub fn from_sexp(node: &SexpNode) -> Option<Self> {
        Some(Stroke(node.clone()))
    }
    pub fn to_sexp(&self) -> SexpNode {
        self.0.clone()
    }
}

// ---- ChangeSet --------------------------------------------------------------

/// A human-readable record of mutations made to a schematic, suitable for
/// returning as an MCP tool response.
#[derive(Debug, Default, Clone)]
pub struct ChangeSet {
    changes: Vec<String>,
}

impl ChangeSet {
    pub fn new() -> Self {
        ChangeSet::default()
    }

    pub fn record(&mut self, msg: impl Into<String>) {
        self.changes.push(msg.into());
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// All recorded changes as a newline-joined string.
    pub fn summary(&self) -> String {
        self.changes.join("\n")
    }

    pub fn changes(&self) -> &[String] {
        &self.changes
    }
}

impl std::fmt::Display for ChangeSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.summary())
    }
}

/// Format a coordinate the way KiCAD does: no trailing `.0` on whole numbers.
fn fmt_coord(v: f64) -> String {
    let r = (v * 1e6).round() / 1e6;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{}", r)
    }
}

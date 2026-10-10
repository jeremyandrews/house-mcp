//! Reads a DXF drawing into site frame geometry.
//!
//! This module only reads and transforms coordinates. Which space a text
//! label falls in, and whether a polygon is valid, are decided by PostGIS when
//! the result is stored.

use std::collections::{BTreeMap, HashMap};

use dxf::{
    Drawing,
    entities::{Entity, EntityType},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::units::LengthUnit;

/// Entities read, counting every copy an INSERT expands to. Bounds the work a
/// drawing of deeply repeated blocks can cause.
pub const MAX_ENTITIES: usize = 100_000;
pub const MAX_BLOCK_DEPTH: usize = 16;
pub const MAX_VERTICES: usize = 10_000;
/// Arcs and circles are stored as line strings with a segment per this many
/// degrees.
const ARC_STEP_DEGREES: f64 = 5.0;

/// What a DXF layer becomes, beyond linework.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayerTarget {
    /// Closed polylines on the layer become spaces of this kind.
    Space(String),
    /// Block insertions on the layer become objects of this kind.
    Object(String),
}

/// Applied after the drawing's units are converted to metres: scale, then
/// rotation about the drawing's origin, then offset.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Transform {
    #[serde(default)]
    pub offset_x: f64,
    #[serde(default)]
    pub offset_y: f64,
    /// Degrees, counterclockwise.
    #[serde(default)]
    pub rotation: f64,
    #[serde(default = "one")]
    pub scale: f64,
}

fn one() -> f64 {
    1.0
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            offset_x: 0.0,
            offset_y: 0.0,
            rotation: 0.0,
            scale: 1.0,
        }
    }
}

pub struct Options {
    /// Overrides the drawing's `$INSUNITS` header when given.
    pub unit: Option<LengthUnit>,
    pub layers: HashMap<String, LayerTarget>,
    pub transform: Transform,
}

#[derive(Debug)]
pub enum ImportError {
    Unreadable(String),
    NoUnits,
    TooManyEntities,
    TooDeep(String),
    SelfReference(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "not a readable DXF file: {e}"),
            Self::NoUnits => write!(
                f,
                "the drawing's $INSUNITS header names no length unit; pass `unit` to say what the drawing is measured in"
            ),
            Self::TooManyEntities => write!(f, "the drawing expands to more than {MAX_ENTITIES} entities"),
            Self::TooDeep(block) => write!(f, "block `{block}` nests deeper than {MAX_BLOCK_DEPTH} levels"),
            Self::SelfReference(block) => write!(f, "block `{block}` inserts itself"),
        }
    }
}

pub struct Linework {
    pub layer: String,
    pub entity: &'static str,
    pub geometry: Value,
    pub text: Option<String>,
    pub text_height: Option<f64>,
}

pub struct SpaceCandidate {
    pub layer: String,
    pub kind: String,
    pub polygon: Value,
}

pub struct ObjectCandidate {
    pub layer: String,
    pub kind: String,
    pub block: String,
    pub point: Value,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct LayerCount {
    pub entities: usize,
    pub spaces: usize,
    pub objects: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Skipped {
    pub layer: String,
    pub entity: String,
    pub reason: String,
}

pub struct Parsed {
    pub unit: LengthUnit,
    pub unit_from_call: bool,
    pub linework: Vec<Linework>,
    pub spaces: Vec<SpaceCandidate>,
    pub objects: Vec<ObjectCandidate>,
    pub layers: BTreeMap<String, LayerCount>,
    /// Each distinct skip with how many entities it applied to.
    pub skipped: BTreeMap<Skipped, usize>,
}

pub fn parse(bytes: &[u8], options: &Options) -> Result<Parsed, ImportError> {
    let drawing =
        Drawing::load(&mut std::io::Cursor::new(bytes)).map_err(|e| ImportError::Unreadable(e.to_string()))?;
    let (unit, unit_from_call) = match options.unit {
        Some(unit) => (unit, true),
        None => (
            LengthUnit::from_insunits(drawing.header.default_drawing_units).ok_or(ImportError::NoUnits)?,
            false,
        ),
    };
    let blocks: HashMap<&str, &dxf::Block> = drawing.blocks().map(|b| (b.name.as_str(), b)).collect();
    let targets: HashMap<String, &LayerTarget> = options
        .layers
        .iter()
        .map(|(name, target)| (name.to_lowercase(), target))
        .collect();
    let mut reader = Reader {
        blocks,
        targets,
        visited: 0,
        stack: Vec::new(),
        out: Parsed {
            unit,
            unit_from_call,
            linework: Vec::new(),
            spaces: Vec::new(),
            objects: Vec::new(),
            layers: BTreeMap::new(),
            skipped: BTreeMap::new(),
        },
    };
    let base = Affine::user(unit.metres(), &options.transform);
    for entity in drawing.entities() {
        if entity.common.is_in_paper_space {
            reader.skip(
                &entity.common.layer,
                entity_name(&entity.specific),
                "in paper space, not model space",
            );
            continue;
        }
        reader.entity(entity, &entity.common.layer, &base)?;
    }
    Ok(reader.out)
}

struct Reader<'a> {
    blocks: HashMap<&'a str, &'a dxf::Block>,
    targets: HashMap<String, &'a LayerTarget>,
    visited: usize,
    stack: Vec<&'a str>,
    out: Parsed,
}

impl<'a> Reader<'a> {
    fn target(&self, layer: &str) -> Option<&'a LayerTarget> {
        self.targets.get(&layer.to_lowercase()).copied()
    }

    fn skip(&mut self, layer: &str, entity: &str, reason: &str) {
        let key = Skipped {
            layer: layer.to_owned(),
            entity: entity.to_owned(),
            reason: reason.to_owned(),
        };
        *self.out.skipped.entry(key).or_default() += 1;
    }

    fn push_linework(&mut self, layer: &str, entity: &'static str, geometry: Value, text: Option<(String, f64)>) {
        self.out.layers.entry(layer.to_owned()).or_default().entities += 1;
        let (text, text_height) = match text {
            Some((value, height)) => (Some(value), Some(height)),
            None => (None, None),
        };
        self.out.linework.push(Linework {
            layer: layer.to_owned(),
            entity,
            geometry,
            text,
            text_height,
        });
    }

    /// `inherited` is the layer an entity on layer "0" takes, which is how a
    /// block drawn on layer 0 lands on the layer it is inserted on.
    fn entity(&mut self, entity: &'a Entity, inherited: &str, at: &Affine) -> Result<(), ImportError> {
        self.visited += 1;
        if self.visited > MAX_ENTITIES {
            return Err(ImportError::TooManyEntities);
        }
        let layer = match entity.common.layer.as_str() {
            "0" => inherited.to_owned(),
            own => own.to_owned(),
        };
        match &entity.specific {
            EntityType::Line(line) => {
                let points = vec![at.apply(line.p1.x, line.p1.y), at.apply(line.p2.x, line.p2.y)];
                self.push_linework(&layer, "LINE", line_string(&points), None);
            }
            EntityType::LwPolyline(poly) => {
                let vertices: Vec<_> = poly.vertices.iter().map(|v| (v.x, v.y, v.bulge)).collect();
                self.polyline(&layer, "LWPOLYLINE", &vertices, poly.is_closed(), at);
            }
            EntityType::Polyline(poly) => {
                // Bit 16 is a 3D mesh and bit 64 a polyface mesh; neither is a
                // plan outline.
                if poly.flags & (16 | 64) != 0 {
                    self.skip(&layer, "POLYLINE", "mesh, not a plan outline");
                } else {
                    let vertices: Vec<_> = poly.vertices().map(|v| (v.location.x, v.location.y, v.bulge)).collect();
                    self.polyline(&layer, "POLYLINE", &vertices, poly.is_closed(), at);
                }
            }
            EntityType::Arc(arc) => {
                let mut end = arc.end_angle;
                while end <= arc.start_angle {
                    end += 360.0;
                }
                let points = arc_points(arc.center.x, arc.center.y, arc.radius, arc.start_angle, end);
                self.push_linework(&layer, "ARC", line_string(&at.apply_all(&points)), None);
            }
            EntityType::Circle(circle) => {
                let points = arc_points(circle.center.x, circle.center.y, circle.radius, 0.0, 360.0);
                self.push_linework(&layer, "CIRCLE", line_string(&at.apply_all(&points)), None);
            }
            EntityType::Text(text) => {
                let point = at.apply(text.location.x, text.location.y);
                let height = text.text_height * at.scale();
                self.push_linework(&layer, "TEXT", point_json(point), Some((text.value.clone(), height)));
            }
            EntityType::MText(text) => {
                let point = at.apply(text.insertion_point.x, text.insertion_point.y);
                let mut raw = text.extended_text.concat();
                raw.push_str(&text.text);
                let height = text.initial_text_height * at.scale();
                self.push_linework(&layer, "MTEXT", point_json(point), Some((plain_mtext(&raw), height)));
            }
            EntityType::Insert(insert) => self.insert(insert, &layer, at)?,
            other => self.skip(&layer, entity_name(other), "entity type not read"),
        }
        Ok(())
    }

    fn insert(&mut self, insert: &'a dxf::entities::Insert, layer: &str, at: &Affine) -> Result<(), ImportError> {
        let name = insert.name.as_str();
        let Some(block) = self.blocks.get(name).copied() else {
            self.skip(layer, "INSERT", "names a block the drawing does not define");
            return Ok(());
        };
        if self.stack.contains(&name) {
            return Err(ImportError::SelfReference(name.to_owned()));
        }
        if self.stack.len() >= MAX_BLOCK_DEPTH {
            return Err(ImportError::TooDeep(name.to_owned()));
        }
        let placed = at.then(&Affine::insert(insert, &block.base_point));
        if let Some(LayerTarget::Object(kind)) = self.target(layer) {
            let point = at.apply(insert.location.x, insert.location.y);
            self.out.layers.entry(layer.to_owned()).or_default().objects += 1;
            self.out.objects.push(ObjectCandidate {
                layer: layer.to_owned(),
                kind: kind.clone(),
                block: name.to_owned(),
                point: point_json(point),
            });
        }
        self.stack.push(name);
        for entity in &block.entities {
            self.entity(entity, layer, &placed)?;
        }
        self.stack.pop();
        Ok(())
    }

    fn polyline(&mut self, layer: &str, entity: &'static str, vertices: &[(f64, f64, f64)], closed: bool, at: &Affine) {
        let mut points = polyline_points(vertices, closed);
        if points.len() < 2 {
            self.skip(layer, entity, "fewer than two vertices");
            return;
        }
        if points.len() > MAX_VERTICES {
            self.skip(layer, entity, "more vertices than the limit");
            return;
        }
        points = at.apply_all(&points);
        self.push_linework(layer, entity, line_string(&points), None);
        if let Some(LayerTarget::Space(kind)) = self.target(layer) {
            if closed && points.len() >= 4 {
                self.out.layers.entry(layer.to_owned()).or_default().spaces += 1;
                self.out.spaces.push(SpaceCandidate {
                    layer: layer.to_owned(),
                    kind: kind.clone(),
                    polygon: json!({"type": "Polygon", "coordinates": [points_json(&points)]}),
                });
            } else {
                self.skip(layer, entity, "open polyline on a space layer");
            }
        }
    }
}

/// A 2D affine map: (x, y) to (a x + b y + c, d x + e y + f).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Affine {
    fn user(unit_metres: f64, t: &Transform) -> Self {
        let (sin, cos) = t.rotation.to_radians().sin_cos();
        let s = unit_metres * t.scale;
        Self {
            a: s * cos,
            b: -s * sin,
            c: t.offset_x,
            d: s * sin,
            e: s * cos,
            f: t.offset_y,
        }
    }

    /// Block coordinates to the coordinates of the drawing the INSERT is in.
    fn insert(insert: &dxf::entities::Insert, base: &dxf::Point) -> Self {
        let (sin, cos) = insert.rotation.to_radians().sin_cos();
        let (sx, sy) = (insert.x_scale_factor, insert.y_scale_factor);
        let (a, b, d, e) = (sx * cos, -sy * sin, sx * sin, sy * cos);
        Self {
            a,
            b,
            c: insert.location.x - (a * base.x + b * base.y),
            d,
            e,
            f: insert.location.y - (d * base.x + e * base.y),
        }
    }

    /// `self` after `inner`.
    fn then(&self, inner: &Affine) -> Self {
        Self {
            a: self.a * inner.a + self.b * inner.d,
            b: self.a * inner.b + self.b * inner.e,
            c: self.a * inner.c + self.b * inner.f + self.c,
            d: self.d * inner.a + self.e * inner.d,
            e: self.d * inner.b + self.e * inner.e,
            f: self.d * inner.c + self.e * inner.f + self.f,
        }
    }

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.b * y + self.c, self.d * x + self.e * y + self.f)
    }

    fn apply_all(&self, points: &[(f64, f64)]) -> Vec<(f64, f64)> {
        points.iter().map(|&(x, y)| self.apply(x, y)).collect()
    }

    /// How much the map stretches lengths, for text heights.
    fn scale(&self) -> f64 {
        (self.a * self.e - self.b * self.d).abs().sqrt()
    }
}

/// Points along an arc, counterclockwise from `start` to `end` degrees.
fn arc_points(cx: f64, cy: f64, radius: f64, start: f64, end: f64) -> Vec<(f64, f64)> {
    let steps = ((end - start) / ARC_STEP_DEGREES).ceil().max(2.0) as usize;
    (0..=steps)
        .map(|i| {
            let angle = (start + (end - start) * i as f64 / steps as f64).to_radians();
            (cx + radius * angle.cos(), cy + radius * angle.sin())
        })
        .collect()
}

/// A polyline's vertices with its bulges drawn as arcs. A closed polyline
/// repeats its first point at the end.
fn polyline_points(vertices: &[(f64, f64, f64)], closed: bool) -> Vec<(f64, f64)> {
    let mut points = Vec::with_capacity(vertices.len() + 1);
    let segments = if closed {
        vertices.len()
    } else {
        vertices.len().saturating_sub(1)
    };
    if let Some(&(x, y, _)) = vertices.first() {
        points.push((x, y));
    }
    for i in 0..segments {
        let (x0, y0, bulge) = vertices[i];
        let (x1, y1, _) = vertices[(i + 1) % vertices.len()];
        points.extend(bulge_points((x0, y0), (x1, y1), bulge));
        points.push((x1, y1));
    }
    points
}

/// The points strictly between two polyline vertices joined by a bulge: the
/// tangent of a quarter of the arc's included angle, negative for clockwise.
fn bulge_points(p0: (f64, f64), p1: (f64, f64), bulge: f64) -> Vec<(f64, f64)> {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let chord = dx.hypot(dy);
    if bulge.abs() < 1e-9 || chord == 0.0 {
        return Vec::new();
    }
    let sweep = 4.0 * bulge.atan();
    let offset = chord / (2.0 * (sweep / 2.0).tan());
    let (cx, cy) = (
        (p0.0 + p1.0) / 2.0 - dy / chord * offset,
        (p0.1 + p1.1) / 2.0 + dx / chord * offset,
    );
    let radius = (p0.0 - cx).hypot(p0.1 - cy);
    let start = (p0.1 - cy).atan2(p0.0 - cx);
    let steps = (sweep.abs().to_degrees() / ARC_STEP_DEGREES).ceil().max(2.0) as usize;
    (1..steps)
        .map(|i| {
            let angle = start + sweep * i as f64 / steps as f64;
            (cx + radius * angle.cos(), cy + radius * angle.sin())
        })
        .collect()
}

/// MTEXT with its formatting codes removed: paragraph breaks become spaces,
/// and font, height, colour and similar codes are dropped.
fn plain_mtext(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('P' | 'p' | 'X' | '~') => out.push(' '),
                Some(escaped @ ('\\' | '{' | '}')) => out.push(escaped),
                Some('U') => {
                    let code: String = chars.by_ref().skip(1).take(4).collect();
                    out.extend(u32::from_str_radix(&code, 16).ok().and_then(char::from_u32));
                }
                Some('L' | 'l' | 'O' | 'o' | 'K' | 'k') | None => {}
                // Every other code carries a value that ends at a semicolon.
                Some(_) => {
                    for c in chars.by_ref() {
                        if c == ';' {
                            break;
                        }
                    }
                }
            },
            '{' | '}' => {}
            other => out.push(other),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn points_json(points: &[(f64, f64)]) -> Value {
    Value::Array(points.iter().map(|&(x, y)| json!([x, y])).collect())
}

fn line_string(points: &[(f64, f64)]) -> Value {
    json!({"type": "LineString", "coordinates": points_json(points)})
}

fn point_json((x, y): (f64, f64)) -> Value {
    json!({"type": "Point", "coordinates": [x, y]})
}

fn entity_name(entity: &EntityType) -> &'static str {
    match entity {
        EntityType::Line(_) => "LINE",
        EntityType::LwPolyline(_) => "LWPOLYLINE",
        EntityType::Polyline(_) => "POLYLINE",
        EntityType::Arc(_) => "ARC",
        EntityType::Circle(_) => "CIRCLE",
        EntityType::Text(_) => "TEXT",
        EntityType::MText(_) => "MTEXT",
        EntityType::Insert(_) => "INSERT",
        EntityType::Ellipse(_) => "ELLIPSE",
        EntityType::Spline(_) => "SPLINE",
        EntityType::Solid(_) => "SOLID",
        EntityType::ModelPoint(_) => "POINT",
        EntityType::Attribute(_) => "ATTRIB",
        EntityType::Leader(_) => "LEADER",
        EntityType::Face3D(_) => "3DFACE",
        EntityType::Image(_) => "IMAGE",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn half_circle_bulge_passes_through_the_far_side() {
        // A bulge of 1 is a half circle. From (0,0) to (2,0) counterclockwise
        // the arc dips below the chord, through (1,-1).
        let points = bulge_points((0.0, 0.0), (2.0, 0.0), 1.0);
        assert!(points.iter().any(|&p| close(p, (1.0, -1.0))), "{points:?}");
        assert!(points.iter().all(|&(x, y)| ((x - 1.0).hypot(y) - 1.0).abs() < 1e-9));
    }

    #[test]
    fn negative_bulge_curves_the_other_way() {
        let points = bulge_points((0.0, 0.0), (2.0, 0.0), -1.0);
        assert!(points.iter().any(|&p| close(p, (1.0, 1.0))), "{points:?}");
    }

    #[test]
    fn closed_polyline_ends_where_it_starts() {
        let square = [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (1.0, 1.0, 0.0), (0.0, 1.0, 0.0)];
        let points = polyline_points(&square, true);
        assert_eq!(points.len(), 5);
        assert!(close(points[0], points[4]));
    }

    #[test]
    fn nested_insert_composes_scale_rotation_and_base_point() {
        // Block point (2,1) with base point (1,1) is (1,0) relative to the
        // base; scaled by 2 and turned 90 degrees that is (0,2), and the
        // insertion at (10,5) puts it at (10,7). Millimetres then make it
        // (0.010, 0.007) in metres.
        let insert = dxf::entities::Insert {
            location: dxf::Point::new(10.0, 5.0, 0.0),
            x_scale_factor: 2.0,
            y_scale_factor: 2.0,
            rotation: 90.0,
            ..Default::default()
        };
        let base = dxf::Point::new(1.0, 1.0, 0.0);
        let world = Affine::user(0.001, &Transform::default()).then(&Affine::insert(&insert, &base));
        assert!(
            close(world.apply(2.0, 1.0), (0.010, 0.007)),
            "{:?}",
            world.apply(2.0, 1.0)
        );
    }

    #[test]
    fn user_transform_scales_then_rotates_then_offsets() {
        let t = Transform {
            offset_x: 100.0,
            offset_y: 50.0,
            rotation: 90.0,
            scale: 2.0,
        };
        assert!(close(Affine::user(1.0, &t).apply(3.0, 0.0), (100.0, 56.0)));
    }

    #[test]
    fn mtext_codes_are_removed() {
        assert_eq!(
            plain_mtext(r"{\fArial|b1;Living}\Proom \H2.5;12 m\U+00B2"),
            "Living room 12 m\u{b2}"
        );
        assert_eq!(plain_mtext(r"a \{b\} c\\d"), r"a {b} c\d");
    }
}

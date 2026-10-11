//! Draws a plan as SVG and rasterizes it to PNG.
//!
//! Geometry arrives as WKB and is read through geozero; the arithmetic here
//! is the map from site metres to pixels and the placing of labels. Label
//! anchors and the extent come from PostGIS.

use std::{fmt::Write as _, sync::Arc};

use geozero::{GeomProcessor, GeozeroGeometry, wkb::Wkb};
use resvg::{tiny_skia, usvg};
use schemars::JsonSchema;
use serde::Deserialize;
use skrifa::{FontRef, MetadataProvider, instance::LocationRef, instance::Size};

use crate::markup::escape;

pub const MIN_SIDE_PX: u32 = 200;
pub const MAX_SIDE_PX: u32 = 4096;
const MARGIN: f64 = 40.0;
/// The strip below the plan for the scale bar and the title. A legend makes
/// it taller.
const FOOTER: f64 = 50.0;
/// The strip right of the plan that the north arrow stands in.
const NORTH_COLUMN: f64 = 40.0;
/// How far the arrow and its letter reach from their centre at any rotation:
/// the tip is 22 px out and the far corner of the N about 24.5 px.
const ARROW_REACH: f64 = 25.0;
/// The height of the arrow's centre.
const ARROW_Y: f64 = MARGIN + 10.0;
// The arrow clears the plan, the footer and the image edges; see
// `draw_north_arrow`.
const _: () = assert!(
    NORTH_COLUMN > ARROW_REACH && ARROW_Y + ARROW_REACH < 2.0 * MARGIN && MARGIN > ARROW_REACH && ARROW_Y > ARROW_REACH
);
const FONT: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");
const HIGHLIGHT: &str = "#e8590c";
const LINEWORK_STROKE: &str = "#8a8a8a";
const LINEWORK_WIDTH: f64 = 0.75;
/// The dashes of an arch's face lines, in pixels.
const ARCH_DASHES: &str = "4 3";
/// A door leaf is drawn heavier than the wall lines, its swing lighter.
const DOOR_LEAF_WIDTH: f64 = 1.5;
const DOOR_SWING_WIDTH: f64 = 0.5;
/// Label font sizes at `LabelSize::Normal`, in pixels.
const SPACE_FONT: f64 = 14.0;
const OBJECT_FONT: f64 = 11.0;
const LEGEND_FONT: f64 = 12.0;
/// How far an object's label starts right of its symbol, in pixels.
const OBJECT_LABEL_GAP: f64 = 10.0;
/// The font sizes a label tries, as fractions of its base size.
const LABEL_STEPS: [f64; 3] = [1.0, 0.8, 0.65];
/// How many marker widths a marker may move from its anchor to find space.
const MARKER_REACH: u32 = 6;
/// The white halo round label text, in pixels.
const HALO: f64 = 3.0;
/// The most of the long side a legend takes from the plan.
const MAX_FOOTER_SHARE: f64 = 0.4;

pub struct Plan {
    pub title: String,
    /// Degrees clockwise from grid north to the site's y axis.
    pub rotation: f64,
    pub extent: Extent,
    pub features: Vec<Feature>,
    pub label_size: LabelSize,
}

#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub xmin: f64,
    pub ymin: f64,
    pub xmax: f64,
    pub ymax: f64,
}

pub struct Feature {
    pub wkb: Vec<u8>,
    pub style: Style,
    pub label: Option<Label>,
    pub highlight: bool,
}

pub struct Label {
    pub text: String,
    pub x: f64,
    pub y: f64,
}

/// How large labels are drawn: smaller for a phone held close, larger for
/// print or a plan seen across a room.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LabelSize {
    Small,
    #[default]
    Normal,
    Large,
}

impl LabelSize {
    fn factor(self) -> f64 {
        match self {
            Self::Small => 0.8,
            Self::Normal => 1.0,
            Self::Large => 1.35,
        }
    }
}

pub enum Style {
    Linework,
    Space {
        kind: String,
    },
    Object {
        kind: String,
    },
    /// A gap in a wall from one jamb to the other, along the wall's centre
    /// line, in site metres. The linework is cut before it gets here; this
    /// draws the jamb ticks and the opening's symbol.
    Opening {
        from: (f64, f64),
        to: (f64, f64),
        wall_depth: f64,
        symbol: Symbol,
    },
}

/// What is drawn across an opening besides its jamb ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbol {
    /// Nothing: an open doorway.
    Plain,
    /// A dashed line along each face of the wall: structure overhead.
    Arch,
    /// A leaf square to the wall from the hinge jamb, and its swing.
    Door {
        /// Hung on the jamb at the end of the line rather than its start.
        hinge_end: bool,
        /// Opens to the right of the line's direction rather than its left.
        swing_right: bool,
        /// Two half leaves, one hung on each jamb.
        double: bool,
    },
}

pub struct Rendered {
    pub svg: String,
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Labels with no room to stand by their feature, in the order of their
    /// numbered markers: entry `i` is marker `i + 1`.
    pub legend: Vec<String>,
}

/// Site metres to pixels, y flipped so north is up.
struct View {
    xmin: f64,
    ymax: f64,
    scale: f64,
    width: f64,
    height: f64,
    footer: f64,
}

impl View {
    fn new(extent: Extent, long_side: f64, footer: f64) -> Self {
        // A plan of a single point still needs a window around it.
        let dx = (extent.xmax - extent.xmin).max(1.0);
        let dy = (extent.ymax - extent.ymin).max(1.0);
        // The scale is whichever of the width and the height fits the long
        // side first, so neither side of the image passes it. A legend may
        // take up to `MAX_FOOTER_SHARE` of the long side from the plan; a
        // longer one makes the image taller instead, since a label is never
        // cut and the plan must stay large enough to read.
        let room = footer.min(FOOTER.max(MAX_FOOTER_SHARE * long_side));
        let scale = ((long_side - 2.0 * MARGIN - NORTH_COLUMN) / dx).min((long_side - 2.0 * MARGIN - room) / dy);
        let width = dx * scale + 2.0 * MARGIN + NORTH_COLUMN;
        let height = dy * scale + 2.0 * MARGIN + footer;
        Self {
            xmin: extent.xmin,
            ymax: extent.ymax,
            scale,
            width,
            height,
            footer,
        }
    }

    fn px(&self, x: f64, y: f64) -> (f64, f64) {
        (
            MARGIN + (x - self.xmin) * self.scale,
            MARGIN + (self.ymax - y) * self.scale,
        )
    }
}

pub fn render(plan: &Plan, long_side_px: u32) -> Result<Rendered, String> {
    let long_side = f64::from(long_side_px.clamp(MIN_SIDE_PX, MAX_SIDE_PX));
    let font = Measure::new()?;
    let factor = plan.label_size.factor();
    let mut features: Vec<&Feature> = plan.features.iter().collect();
    // Linework under spaces under openings under objects, highlights last so
    // nothing covers them.
    features.sort_by_key(|f| match (&f.style, f.highlight) {
        (_, true) => 4,
        (Style::Linework, _) => 0,
        (Style::Space { .. }, _) => 1,
        (Style::Opening { .. }, _) => 2,
        (Style::Object { .. }, _) => 3,
    });
    // Labels that do not fit go to a legend in the footer, which may make
    // the footer taller and so the plan smaller, which may push out more
    // labels. The footer only grows, and no further than every label in the
    // legend needs, so this ends.
    let extent = with_door_swings(plan.extent, &plan.features);
    let mut footer = FOOTER;
    let (view, labels, legend) = loop {
        let view = View::new(extent, long_side, footer);
        let labels = place_labels(&features, &view, factor, &font)?;
        let legend = Legend::lay_out(&labels, &view, &plan.title, factor, &font);
        if legend.footer <= footer {
            break (view, labels, legend);
        }
        footer = legend.footer;
    };
    let svg = svg(plan, &features, &view, &labels, &legend, factor)?;
    let png = rasterize(&svg)?;
    Ok(Rendered {
        svg,
        png,
        width: view.width.round() as u32,
        height: view.height.round() as u32,
        legend: legend.entries.into_iter().map(|e| e.text).collect(),
    })
}

fn svg(
    plan: &Plan,
    features: &[&Feature],
    view: &View,
    labels: &[Placed],
    legend: &Legend,
    factor: f64,
) -> Result<String, String> {
    let mut out = String::new();
    let (w, h) = (view.width.round(), view.height.round());
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="Noto Sans">"##
    );
    let _ = write!(out, r#"<rect width="{w}" height="{h}" fill="white"/>"#);
    for feature in features {
        draw_feature(&mut out, feature, view)?;
    }
    for placed in labels {
        draw_placed(&mut out, placed, factor);
    }
    draw_scale_bar(&mut out, view);
    legend.draw(&mut out);
    draw_north_arrow(&mut out, view, plan.rotation);
    let _ = write!(
        out,
        r##"<text x="{MARGIN}" y="{}" font-size="14" fill="#333">{}</text>"##,
        view.height - 14.0,
        escape(&plan.title)
    );
    out.push_str("</svg>");
    Ok(out)
}

fn draw_feature(out: &mut String, feature: &Feature, view: &View) -> Result<(), String> {
    if let Style::Opening {
        from,
        to,
        wall_depth,
        symbol,
    } = feature.style
    {
        draw_opening(out, from, to, wall_depth, symbol, feature.highlight, view);
        return Ok(());
    }
    let shapes = Shapes::of(&feature.wkb, view)?;
    let (fill, stroke, width, radius) = match (&feature.style, feature.highlight) {
        (Style::Linework | Style::Opening { .. }, _) => {
            ("none".to_owned(), LINEWORK_STROKE.to_owned(), LINEWORK_WIDTH, 2.0)
        }
        (Style::Space { kind }, false) => (space_fill(kind).to_owned(), "#555".to_owned(), 1.5, 3.0),
        (Style::Object { kind }, false) => {
            let colour = kind_colour(kind);
            (colour.clone(), colour, 2.5, 6.0)
        }
        (Style::Space { .. }, true) => ("none".to_owned(), HIGHLIGHT.to_owned(), 4.0, 6.0),
        (Style::Object { .. }, true) => (HIGHLIGHT.to_owned(), HIGHLIGHT.to_owned(), 4.0, 9.0),
    };
    if !shapes.path.is_empty() {
        let area_fill = if shapes.rings.is_empty() { "none" } else { fill.as_str() };
        let _ = write!(
            out,
            r##"<path d="{}" fill="{area_fill}" fill-opacity="0.6" fill-rule="evenodd" stroke="{stroke}" stroke-width="{width}" stroke-linejoin="round"/>"##,
            shapes.path
        );
    }
    for (x, y) in shapes.points {
        let _ = write!(
            out,
            r##"<circle cx="{x:.2}" cy="{y:.2}" r="{radius}" fill="{fill}" stroke="white" stroke-width="1.5"/>"##
        );
    }
    Ok(())
}

/// A jamb tick across the wall at each end of the opening, `wall_depth`
/// long, and the opening's symbol between them.
fn draw_opening(
    out: &mut String,
    from: (f64, f64),
    to: (f64, f64),
    wall_depth: f64,
    symbol: Symbol,
    highlight: bool,
    view: &View,
) {
    let stroke = if highlight { HIGHLIGHT } else { LINEWORK_STROKE };
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let width = dx.hypot(dy);
    if width <= 0.0 {
        return;
    }
    // Along the opening, and its left normal.
    let (ux, uy) = (dx / width, dy / width);
    let (nx, ny) = (-uy, ux);
    let half = wall_depth / 2.0;
    let at = |p: (f64, f64), across: f64| view.px(p.0 + nx * across, p.1 + ny * across);
    let line = |out: &mut String, a: (f64, f64), b: (f64, f64), extra: &str| {
        let _ = write!(
            out,
            r#"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="{stroke}" {extra}/>"#,
            a.0, a.1, b.0, b.1
        );
    };
    let tick = format!(r#"stroke-width="{LINEWORK_WIDTH}""#);
    for jamb in [from, to] {
        line(out, at(jamb, -half), at(jamb, half), &tick);
    }
    match symbol {
        Symbol::Plain => {}
        Symbol::Arch => {
            // Straight across along each face of the wall, as a plan shows
            // an opening with structure overhead.
            let dashed =
                format!(r#"class="arch-face" stroke-width="{LINEWORK_WIDTH}" stroke-dasharray="{ARCH_DASHES}""#);
            for face in [-half, half] {
                line(out, at(from, face), at(to, face), &dashed);
            }
        }
        Symbol::Door {
            hinge_end,
            swing_right,
            double,
        } => {
            // The leaf stands square to the wall from the face on the side
            // it opens to, and the swing runs from its tip back to the
            // other jamb, or for a pair of leaves to the middle where they
            // meet.
            let side = if swing_right { -1.0 } else { 1.0 };
            let middle = ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
            let leaves = if double {
                vec![(from, middle, width / 2.0), (to, middle, width / 2.0)]
            } else if hinge_end {
                vec![(to, from, width)]
            } else {
                vec![(from, to, width)]
            };
            for (hinge, closes_at, length) in leaves {
                let hinge_px = at(hinge, side * half);
                let tip_px = at(hinge, side * (half + length));
                let shut_px = at(closes_at, side * half);
                line(
                    out,
                    hinge_px,
                    tip_px,
                    &format!(r#"class="door-leaf" stroke-width="{DOOR_LEAF_WIDTH}""#),
                );
                let radius = length * view.scale;
                // A quarter turn from the tip to where the leaf shuts,
                // clockwise on screen when the turn's cross product is
                // positive, since y points down.
                let (ax, ay) = (tip_px.0 - hinge_px.0, tip_px.1 - hinge_px.1);
                let (bx, by) = (shut_px.0 - hinge_px.0, shut_px.1 - hinge_px.1);
                let sweep = u8::from(ax * by - ay * bx > 0.0);
                let _ = write!(
                    out,
                    r#"<path class="door-swing" d="M{:.2} {:.2} A{radius:.2} {radius:.2} 0 0 {sweep} {:.2} {:.2}" fill="none" stroke="{stroke}" stroke-width="{DOOR_SWING_WIDTH}"/>"#,
                    tip_px.0, tip_px.1, shut_px.0, shut_px.1
                );
            }
        }
    }
}

/// The extent widened to hold every door's leaf at its widest open, which
/// may reach past the floor, as a door to the garden does.
fn with_door_swings(extent: Extent, features: &[Feature]) -> Extent {
    let mut extent = extent;
    for feature in features {
        let Style::Opening {
            from,
            to,
            wall_depth,
            symbol: Symbol::Door {
                swing_right, double, ..
            },
        } = feature.style
        else {
            continue;
        };
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let width = dx.hypot(dy);
        if width <= 0.0 {
            continue;
        }
        let side = if swing_right { -1.0 } else { 1.0 };
        let reach = wall_depth / 2.0 + if double { width / 2.0 } else { width };
        let (nx, ny) = (-dy / width * side * reach, dx / width * side * reach);
        for (x, y) in [(from.0 + nx, from.1 + ny), (to.0 + nx, to.1 + ny)] {
            extent.xmin = extent.xmin.min(x);
            extent.xmax = extent.xmax.max(x);
            extent.ymin = extent.ymin.min(y);
            extent.ymax = extent.ymax.max(y);
        }
    }
    extent
}

/// A box in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Rect {
    fn centred(cx: f64, cy: f64, w: f64, h: f64) -> Self {
        Self {
            x0: cx - w / 2.0,
            y0: cy - h / 2.0,
            x1: cx + w / 2.0,
            y1: cy + h / 2.0,
        }
    }

    fn overlaps(&self, other: &Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    fn within(&self, outer: &Rect) -> bool {
        self.x0 >= outer.x0 && self.y0 >= outer.y0 && self.x1 <= outer.x1 && self.y1 <= outer.y1
    }

    fn centre(&self) -> (f64, f64) {
        ((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }
}

/// Text widths from the embedded font, the one the plan is drawn with.
struct Measure<'a> {
    font: FontRef<'a>,
    units_per_em: f64,
}

impl Measure<'static> {
    fn new() -> Result<Self, String> {
        let font = FontRef::new(FONT).map_err(|e| format!("reading the embedded font: {e}"))?;
        let units_per_em = f64::from(font.metrics(Size::unscaled(), LocationRef::default()).units_per_em);
        Ok(Self { font, units_per_em })
    }
}

impl Measure<'_> {
    /// The advance width of `text` at `size` pixels. Kerning is left out,
    /// which only ever makes the box a little wide.
    fn width(&self, text: &str, size: f64) -> f64 {
        let charmap = self.font.charmap();
        let metrics = self.font.glyph_metrics(Size::unscaled(), LocationRef::default());
        let units: f64 = text
            .chars()
            .map(|c| {
                charmap
                    .map(c)
                    .and_then(|glyph| metrics.advance_width(glyph))
                    .map_or(0.6 * self.units_per_em, f64::from)
            })
            .sum();
        units / self.units_per_em * size
    }
}

/// Where a label ended up.
enum Spot {
    /// The text itself, centred on this box.
    Text { rect: Rect, size: f64 },
    /// A numbered marker at the anchor, its text in the legend.
    Marker {
        x: f64,
        y: f64,
        radius: f64,
        number: usize,
        anchor: (f64, f64),
    },
}

struct Placed {
    text: String,
    spot: Spot,
}

/// A label waiting to be placed.
struct Pending<'a> {
    text: &'a str,
    anchor: (f64, f64),
    base: f64,
    object: bool,
    /// A space's rings in pixels, which its label must keep touching.
    rings: Vec<Vec<(f64, f64)>>,
    highlight: bool,
    area: f64,
}

/// Places every label after the plan is laid out: highlighted features first,
/// then spaces from the largest, then objects. Each takes the first spot
/// that overlaps no label already placed and stays in the image, trying its
/// anchor, then eight spots one text height around it, at its full size and
/// then smaller. A label with no spot becomes a numbered marker and a legend
/// entry. A label is never cut short and never drawn over another.
fn place_labels(features: &[&Feature], view: &View, factor: f64, font: &Measure) -> Result<Vec<Placed>, String> {
    let mut pending = Vec::new();
    for feature in features {
        let Some(label) = &feature.label else { continue };
        let (object, rings) = match &feature.style {
            Style::Object { .. } => (true, Vec::new()),
            Style::Space { .. } => (false, Shapes::of(&feature.wkb, view)?.rings),
            Style::Linework | Style::Opening { .. } => continue,
        };
        let area = rings.iter().map(|ring| ring_area(ring)).sum::<f64>().abs();
        pending.push(Pending {
            text: &label.text,
            anchor: view.px(label.x, label.y),
            base: if object { OBJECT_FONT } else { SPACE_FONT } * factor,
            object,
            rings,
            highlight: feature.highlight,
            area,
        });
    }
    // Stable, so objects keep the order they came in.
    pending.sort_by(|a, b| {
        (!a.highlight, a.object)
            .cmp(&(!b.highlight, b.object))
            .then(b.area.total_cmp(&a.area))
    });
    let bounds = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: view.width,
        y1: view.height - view.footer,
    };
    let arrow = Rect::centred(view.width - MARGIN, ARROW_Y, 2.0 * ARROW_REACH, 2.0 * ARROW_REACH);
    // Labels keep off the arrow and off every object's dot, so no label
    // hides a symbol.
    let mut taken: Vec<Rect> = vec![arrow];
    for feature in features {
        if let Style::Object { .. } = feature.style {
            let radius = if feature.highlight { 9.0 } else { 6.0 };
            for (x, y) in Shapes::of(&feature.wkb, view)?.points {
                taken.push(Rect::centred(x, y, 2.0 * radius + 2.0, 2.0 * radius + 2.0));
            }
        }
    }
    let mut placed = Vec::with_capacity(pending.len());
    let mut markers = 0;
    for label in &pending {
        let fits = |rect: &Rect, taken: &[Rect]| {
            rect.within(&bounds)
                && !taken.iter().any(|t| t.overlaps(rect))
                && (label.object || touches(rect, &label.rings))
        };
        let found = LABEL_STEPS.iter().find_map(|step| {
            let size = label.base * step;
            candidates(label, size, font)
                .into_iter()
                .find(|rect| fits(rect, &taken))
                .map(|rect| (rect, size))
        });
        let spot = match found {
            Some((rect, size)) => {
                taken.push(rect);
                Spot::Text { rect, size }
            }
            None => {
                markers += 1;
                let radius = marker_radius(markers, factor);
                let (x, y) = marker_spot(label.anchor, radius, &taken, &bounds);
                taken.push(Rect::centred(x, y, 2.0 * radius + 2.0, 2.0 * radius + 2.0));
                Spot::Marker {
                    x,
                    y,
                    radius,
                    number: markers,
                    anchor: label.anchor,
                }
            }
        };
        placed.push(Placed {
            text: label.text.to_owned(),
            spot,
        });
    }
    Ok(placed)
}

/// The box a label of this size takes at its anchor, then at the eight spots
/// around the anchor, each one text height clear of it.
fn candidates(label: &Pending, size: f64, font: &Measure) -> Vec<Rect> {
    let w = font.width(label.text, size) + HALO + 1.0;
    let h = 1.2 * size + 2.0;
    let (ax, ay) = label.anchor;
    let first = if label.object {
        let gap = OBJECT_LABEL_GAP * size / OBJECT_FONT;
        Rect::centred(ax + gap + w / 2.0, ay, w, h)
    } else {
        Rect::centred(ax, ay, w, h)
    };
    let mut boxes = vec![first];
    for (sx, sy) in [
        (0.0, -1.0),
        (1.0, 0.0),
        (0.0, 1.0),
        (-1.0, 0.0),
        (1.0, -1.0),
        (1.0, 1.0),
        (-1.0, 1.0),
        (-1.0, -1.0),
    ] {
        // Clear of the anchor by a text height on each side it moves to.
        let cx = ax + sx * (h + w / 2.0);
        let cy = ay + sy * (h + h / 2.0);
        boxes.push(Rect::centred(cx, cy, w, h));
    }
    boxes
}

fn marker_radius(number: usize, factor: f64) -> f64 {
    let digits = number.to_string().len() as f64;
    (5.0 + 2.5 * digits) * factor
}

/// The anchor itself when it is free, otherwise the nearest free spot on a
/// grid of marker sized steps around it, up to `MARKER_REACH` steps out, and
/// otherwise the anchor anyway: the marker must stay by its feature. A
/// marker moved off its anchor is drawn with a leader line back to it.
fn marker_spot(anchor: (f64, f64), radius: f64, taken: &[Rect], bounds: &Rect) -> (f64, f64) {
    let side = 2.0 * radius + 2.0;
    let reach = MARKER_REACH as i32;
    let mut steps: Vec<(i32, i32)> = (-reach..=reach)
        .flat_map(|i| (-reach..=reach).map(move |j| (i, j)))
        .collect();
    steps.sort_by_key(|&(i, j)| (i * i + j * j, j, i));
    steps
        .into_iter()
        .map(|(i, j)| (anchor.0 + f64::from(i) * side, anchor.1 + f64::from(j) * side))
        .find(|&(x, y)| {
            let rect = Rect::centred(x, y, side, side);
            rect.within(bounds) && !taken.iter().any(|t| t.overlaps(&rect))
        })
        .unwrap_or(anchor)
}

/// Whether a box covers some of a polygon: its centre is inside, or an edge
/// of the polygon crosses it.
fn touches(rect: &Rect, rings: &[Vec<(f64, f64)>]) -> bool {
    let (cx, cy) = rect.centre();
    let inside = rings.iter().filter(|ring| crosses_right(ring, cx, cy)).count() % 2 == 1;
    inside
        || rings
            .iter()
            .flat_map(|ring| ring.windows(2))
            .any(|edge| segment_meets_rect(edge[0], edge[1], rect))
}

/// Whether a ray from (x, y) to the right crosses the ring an odd number of
/// times.
fn crosses_right(ring: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut odd = false;
    for edge in ring.windows(2) {
        let ((x0, y0), (x1, y1)) = (edge[0], edge[1]);
        if (y0 > y) != (y1 > y) && x < x0 + (y - y0) / (y1 - y0) * (x1 - x0) {
            odd = !odd;
        }
    }
    odd
}

/// Liang-Barsky: whether any part of the segment lies strictly inside the
/// box.
fn segment_meets_rect(a: (f64, f64), b: (f64, f64), rect: &Rect) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, a.0 - rect.x0),
        (dx, rect.x1 - a.0),
        (-dy, a.1 - rect.y0),
        (dy, rect.y1 - a.1),
    ] {
        if p == 0.0 {
            if q <= 0.0 {
                return false;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
        }
    }
    t0 < t1
}

/// Signed area by the shoelace formula, in square pixels.
fn ring_area(ring: &[(f64, f64)]) -> f64 {
    ring.windows(2).map(|e| e[0].0 * e[1].1 - e[1].0 * e[0].1).sum::<f64>() / 2.0
}

fn draw_placed(out: &mut String, placed: &Placed, factor: f64) {
    match placed.spot {
        Spot::Text { rect, size } => {
            let (cx, cy) = rect.centre();
            let _ = write!(
                out,
                r##"<text x="{cx:.2}" y="{:.2}" font-size="{size:.2}" text-anchor="middle" fill="#111" stroke="white" stroke-width="{HALO}" paint-order="stroke">{}</text>"##,
                cy + size / 3.0,
                escape(&placed.text)
            );
        }
        Spot::Marker {
            x,
            y,
            radius,
            number,
            anchor,
        } => {
            let (dx, dy) = (x - anchor.0, y - anchor.1);
            let distance = dx.hypot(dy);
            if distance > radius {
                // A leader from the feature to the edge of its marker.
                let _ = write!(
                    out,
                    r##"<line class="label-leader" x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="#111" stroke-width="0.75"/>"##,
                    anchor.0,
                    anchor.1,
                    x - dx / distance * radius,
                    y - dy / distance * radius
                );
            }
            let size = 0.9 * radius.min(8.0 * factor) + 1.0;
            let _ = write!(
                out,
                concat!(
                    r##"<g class="label-marker"><circle cx="{x:.2}" cy="{y:.2}" r="{radius:.2}" fill="white" stroke="#111" stroke-width="1"/>"##,
                    r##"<text x="{x:.2}" y="{ty:.2}" font-size="{size:.2}" text-anchor="middle" fill="#111">{number}</text></g>"##,
                ),
                x = x,
                y = y,
                radius = radius,
                ty = y + size / 3.0,
                size = size,
                number = number,
            );
        }
    }
}

/// One numbered line of the legend.
struct Entry {
    number: usize,
    text: String,
    /// The text broken to fit when the image is narrow, else one line.
    lines: Vec<String>,
}

/// The labels that became markers, listed in the footer beside the scale
/// bar and title, in columns; on an image too narrow for that, in one column
/// above them, broken between words to fit.
struct Legend {
    entries: Vec<Entry>,
    /// The footer height the legend needs, at least `FOOTER`.
    footer: f64,
    x: f64,
    top: f64,
    column: f64,
    columns: usize,
    size: f64,
}

impl Legend {
    fn lay_out(labels: &[Placed], view: &View, title: &str, factor: f64, font: &Measure) -> Self {
        let size = LEGEND_FONT * factor;
        let line = 1.3 * size;
        let mut entries: Vec<Entry> = labels
            .iter()
            .filter_map(|p| match p.spot {
                Spot::Marker { number, .. } => Some(Entry {
                    number,
                    text: p.text.clone(),
                    lines: vec![p.text.clone()],
                }),
                Spot::Text { .. } => None,
            })
            .collect();
        let mut legend = Self {
            footer: FOOTER,
            x: MARGIN,
            top: 0.0,
            column: 0.0,
            columns: 1,
            size,
            entries: Vec::new(),
        };
        if entries.is_empty() {
            return legend;
        }
        let widest = entries.iter().map(|e| font.width(&e.text, size)).fold(0.0, f64::max);
        legend.column = 2.0 * size + widest + size;
        // Beside the scale bar and the title, which stay where they are, or
        // across the whole width above them, whichever needs the shorter
        // footer.
        let scale_bar = scale_bar_width(view);
        let title_width = font.width(title, 14.0);
        let beside = MARGIN + scale_bar.max(title_width) + 2.0 * size;
        let right = view.width - 10.0;
        let columns = |from: f64| ((right - from) / legend.column).floor() as usize;
        let rows = |columns: usize| entries.len().div_ceil(columns) as f64;
        let beside_footer = (columns(beside) > 0).then(|| FOOTER.max(rows(columns(beside)) * line + 16.0));
        let across_footer = (columns(10.0) > 0).then(|| FOOTER + rows(columns(10.0)) * line + 8.0);
        match (beside_footer, across_footer) {
            (Some(b), a) if a.is_none_or(|a| b <= a) => {
                legend.x = beside;
                legend.columns = columns(beside);
                legend.footer = b;
            }
            (_, Some(a)) => {
                legend.x = 10.0;
                legend.columns = columns(10.0);
                legend.footer = a;
            }
            _ => {
                // Too narrow for even one column of whole entries: one
                // column, each entry broken between words to fit.
                legend.x = 10.0;
                legend.columns = 1;
                let room = right - legend.x - 2.0 * size;
                for entry in &mut entries {
                    entry.lines = wrap(&entry.text, room, size, font);
                }
                let lines: usize = entries.iter().map(|e| e.lines.len()).sum();
                legend.footer = FOOTER + lines as f64 * line + 8.0;
            }
        }
        legend.top = view.height - view.footer.max(legend.footer) + 6.0;
        legend.entries = entries;
        legend
    }

    fn draw(&self, out: &mut String) {
        let line = 1.3 * self.size;
        let rows = self.entries.len().div_ceil(self.columns);
        let mut y = self.top + self.size;
        for (i, entry) in self.entries.iter().enumerate() {
            let (x, entry_y) = if self.columns > 1 {
                let (column, row) = (i / rows, i % rows);
                (
                    self.x + column as f64 * self.column,
                    self.top + self.size + row as f64 * line,
                )
            } else {
                (self.x, y)
            };
            let text_x = x + 2.0 * self.size;
            let _ = write!(
                out,
                r##"<text x="{x:.2}" y="{entry_y:.2}" font-size="{:.2}" font-weight="bold" fill="#111">{}</text>"##,
                self.size, entry.number
            );
            if entry.lines.len() == 1 {
                let _ = write!(
                    out,
                    r##"<text x="{text_x:.2}" y="{entry_y:.2}" font-size="{:.2}" fill="#111">{}</text>"##,
                    self.size,
                    escape(&entry.text)
                );
            } else {
                let _ = write!(out, r##"<text font-size="{:.2}" fill="#111">"##, self.size);
                for (j, part) in entry.lines.iter().enumerate() {
                    let _ = write!(
                        out,
                        r#"<tspan x="{text_x:.2}" y="{:.2}">{}</tspan>"#,
                        entry_y + j as f64 * line,
                        escape(part)
                    );
                }
                out.push_str("</text>");
            }
            y += entry.lines.len() as f64 * line;
        }
    }
}

/// Breaks text between words into lines no wider than `room`, and a word
/// wider than that between its characters. Nothing is dropped.
fn wrap(text: &str, room: f64, size: f64, font: &Measure) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let candidate = if current.is_empty() {
            word.to_owned()
        } else {
            format!("{current} {word}")
        };
        if font.width(&candidate, size) <= room || current.is_empty() && font.width(word, size) <= room {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        for c in word.chars() {
            current.push(c);
            if font.width(&current, size) > room && current.chars().count() > 1 {
                let last = current.pop().expect("two characters");
                lines.push(std::mem::take(&mut current));
                current.push(last);
            }
        }
    }
    lines.push(current);
    lines
}

/// The scale bar's length in metres and pixels: a round length about a
/// quarter of the plan's width.
fn scale_bar(view: &View) -> (f64, f64) {
    let target = (view.width - 2.0 * MARGIN - NORTH_COLUMN) / view.scale / 4.0;
    let metres = round_length(target);
    (metres, metres * view.scale)
}

/// How far the bar and its length reach right of the margin.
fn scale_bar_width(view: &View) -> f64 {
    let (metres, length) = scale_bar(view);
    length + 8.0 + 7.5 * format_metres(metres).len() as f64
}

fn draw_scale_bar(out: &mut String, view: &View) {
    let (metres, length) = scale_bar(view);
    let (x, y) = (MARGIN, view.height - 34.0);
    let half = length / 2.0;
    let _ = write!(
        out,
        concat!(
            r##"<g stroke="#111" stroke-width="1.5">"##,
            r##"<rect x="{x:.2}" y="{y:.2}" width="{half:.2}" height="6" fill="#111"/>"##,
            r##"<rect x="{mid:.2}" y="{y:.2}" width="{half:.2}" height="6" fill="white"/>"##,
            r##"</g>"##,
            r##"<text x="{end:.2}" y="{ty:.2}" font-size="12" fill="#111">{label}</text>"##,
        ),
        x = x,
        y = y,
        half = half,
        mid = x + half,
        end = x + length + 8.0,
        ty = y + 7.0,
        label = format_metres(metres),
    );
}

/// Points at grid north, which is the site's y axis turned back by the
/// site's rotation. It stands in its own column right of the plan, near the
/// top, so at any rotation and any shape it stays off the plan, which ends
/// `NORTH_COLUMN` to its left, and above the footer, which starts at least
/// `2 * MARGIN` down. Labels keep out of the square it turns in.
fn draw_north_arrow(out: &mut String, view: &View, rotation: f64) {
    let (x, y) = (view.width - MARGIN, ARROW_Y);
    let _ = write!(
        out,
        concat!(
            r##"<g transform="translate({x:.2} {y:.2}) rotate({r:.3})">"##,
            r##"<path d="M 0 -22 L 8 8 L 0 2 L -8 8 Z" fill="#111"/>"##,
            r##"<text x="0" y="24" font-size="13" text-anchor="middle" fill="#111">N</text>"##,
            r##"</g>"##,
        ),
        x = x,
        y = y,
        r = -rotation,
    );
}

/// The largest 1, 2 or 5 times a power of ten that does not exceed `target`.
fn round_length(target: f64) -> f64 {
    let power = 10f64.powf(target.log10().floor());
    [5.0, 2.0, 1.0]
        .into_iter()
        .map(|m| m * power)
        .find(|&v| v <= target)
        .unwrap_or(power)
}

fn format_metres(metres: f64) -> String {
    if metres >= 1000.0 {
        format!("{} km", metres / 1000.0)
    } else if metres >= 1.0 {
        format!("{metres} m")
    } else {
        format!("{} cm", (metres * 100.0).round())
    }
}

fn space_fill(kind: &str) -> &'static str {
    match kind {
        "room" => "#f3e9d2",
        "terrace" => "#e0dcd5",
        "garden_bed" => "#cfe8c4",
        "pool" => "#bfe0f2",
        "building" => "#e6e6e6",
        _ => "none",
    }
}

/// A stable colour per object kind, so a kind looks the same in every plan.
fn kind_colour(kind: &str) -> String {
    let hue = kind
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)))
        % 360;
    format!("hsl({hue}, 65%, 42%)")
}

fn rasterize(svg: &str) -> Result<Vec<u8>, String> {
    let mut options = usvg::Options::default();
    let fonts = Arc::make_mut(&mut options.fontdb);
    fonts.load_font_data(FONT.to_vec());
    fonts.set_sans_serif_family("Noto Sans");
    let tree = usvg::Tree::from_str(svg, &options).map_err(|e| format!("invalid SVG: {e}"))?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).ok_or("plan has no area")?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|e| format!("PNG encoding failed: {e}"))
}

/// Collects one geometry as SVG path data in pixels, its points as circle
/// centres, and its polygon rings.
struct Shapes<'a> {
    view: &'a View,
    path: String,
    points: Vec<(f64, f64)>,
    rings: Vec<Vec<(f64, f64)>>,
    polygon_depth: usize,
    point_depth: usize,
    ring_start: bool,
}

impl<'a> Shapes<'a> {
    fn of(wkb: &[u8], view: &'a View) -> Result<Self, String> {
        let mut shapes = Self {
            view,
            path: String::new(),
            points: Vec::new(),
            rings: Vec::new(),
            polygon_depth: 0,
            point_depth: 0,
            ring_start: false,
        };
        Wkb(wkb)
            .process_geom(&mut shapes)
            .map_err(|e| format!("unreadable geometry: {e}"))?;
        Ok(shapes)
    }
}

impl GeomProcessor for Shapes<'_> {
    fn xy(&mut self, x: f64, y: f64, _idx: usize) -> geozero::error::Result<()> {
        let (px, py) = self.view.px(x, y);
        if self.point_depth > 0 {
            self.points.push((px, py));
        } else {
            let command = if self.ring_start { 'M' } else { 'L' };
            let _ = write!(self.path, "{command}{px:.2} {py:.2} ");
            self.ring_start = false;
            if self.polygon_depth > 0
                && let Some(ring) = self.rings.last_mut()
            {
                ring.push((px, py));
            }
        }
        Ok(())
    }

    fn point_begin(&mut self, _idx: usize) -> geozero::error::Result<()> {
        self.point_depth += 1;
        Ok(())
    }

    fn point_end(&mut self, _idx: usize) -> geozero::error::Result<()> {
        self.point_depth -= 1;
        Ok(())
    }

    fn multipoint_begin(&mut self, _size: usize, _idx: usize) -> geozero::error::Result<()> {
        self.point_depth += 1;
        Ok(())
    }

    fn multipoint_end(&mut self, _idx: usize) -> geozero::error::Result<()> {
        self.point_depth -= 1;
        Ok(())
    }

    fn linestring_begin(&mut self, _tagged: bool, _size: usize, _idx: usize) -> geozero::error::Result<()> {
        self.ring_start = true;
        if self.polygon_depth > 0 {
            self.rings.push(Vec::new());
        }
        Ok(())
    }

    fn linestring_end(&mut self, _tagged: bool, _idx: usize) -> geozero::error::Result<()> {
        if self.polygon_depth > 0 {
            self.path.push_str("Z ");
        }
        Ok(())
    }

    fn polygon_begin(&mut self, _tagged: bool, _size: usize, _idx: usize) -> geozero::error::Result<()> {
        self.polygon_depth += 1;
        Ok(())
    }

    fn polygon_end(&mut self, _tagged: bool, _idx: usize) -> geozero::error::Result<()> {
        self.polygon_depth -= 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_bar_lengths_are_round() {
        assert_eq!(round_length(7.3), 5.0);
        assert_eq!(round_length(3.0), 2.0);
        assert_eq!(round_length(1.4), 1.0);
        assert_eq!(round_length(0.6), 0.5);
        assert_eq!(round_length(240.0), 200.0);
    }

    #[test]
    fn text_is_measured_in_proportion_to_its_size() {
        let font = Measure::new().expect("font");
        let small = font.width("Kitchen", 10.0);
        assert!(small > 25.0 && small < 50.0, "{small}");
        assert!((font.width("Kitchen", 20.0) - 2.0 * small).abs() < 1e-9);
        assert!(font.width("WWW", 10.0) > font.width("iii", 10.0));
    }

    #[test]
    fn a_box_touches_a_polygon_it_covers_any_part_of() {
        let square = vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]];
        let rect = |x0, y0, x1, y1| Rect { x0, y0, x1, y1 };
        assert!(touches(&rect(2.0, 2.0, 4.0, 4.0), &square), "inside");
        assert!(touches(&rect(-5.0, -5.0, 15.0, 15.0), &square), "around");
        assert!(touches(&rect(8.0, 4.0, 20.0, 6.0), &square), "across an edge");
        assert!(!touches(&rect(11.0, 4.0, 20.0, 6.0), &square), "beside");
        assert!(!touches(&rect(10.0, 4.0, 20.0, 6.0), &square), "only touching the edge");
    }

    #[test]
    fn placed_labels_never_overlap_and_space_labels_keep_to_their_space() {
        let font = Measure::new().expect("font");
        let square = |x: f64, y: f64, side: f64| {
            let ring = [[x, y], [x + side, y], [x + side, y + side], [x, y + side], [x, y]];
            crate::geom::to_wkb(&serde_json::json!({"type": "Polygon", "coordinates": [ring]})).expect("wkb")
        };
        let point = |x: f64, y: f64| {
            crate::geom::to_wkb(&serde_json::json!({"type": "Point", "coordinates": [x, y]})).expect("wkb")
        };
        // A big room, and a small one crowded with five objects.
        let mut features = vec![
            Feature {
                wkb: square(0.0, 0.0, 8.0),
                style: Style::Space { kind: "room".into() },
                label: Some(Label {
                    text: "Living room".into(),
                    x: 4.0,
                    y: 4.0,
                }),
                highlight: false,
            },
            Feature {
                wkb: square(8.0, 0.0, 1.2),
                style: Style::Space { kind: "room".into() },
                label: Some(Label {
                    text: "Alcove".into(),
                    x: 8.6,
                    y: 0.6,
                }),
                highlight: false,
            },
        ];
        for i in 0..5 {
            let (x, y) = (8.3 + 0.15 * f64::from(i), 0.5 + 0.05 * f64::from(i));
            features.push(Feature {
                wkb: point(x, y),
                style: Style::Object { kind: "lamp".into() },
                label: Some(Label {
                    text: format!("Reading lamp {i}"),
                    x,
                    y,
                }),
                highlight: i == 4,
            });
        }
        let extent = Extent {
            xmin: 0.0,
            ymin: 0.0,
            xmax: 9.2,
            ymax: 8.0,
        };
        for long_side in [400.0, 800.0, 1600.0] {
            let view = View::new(extent, long_side, FOOTER);
            let refs: Vec<&Feature> = features.iter().collect();
            let placed = place_labels(&refs, &view, 1.0, &font).expect("placed");
            assert_eq!(placed.len(), 7);
            // The highlighted lamp goes first, then the rooms by size.
            assert_eq!(placed[0].text, "Reading lamp 4");
            assert_eq!(placed[1].text, "Living room");
            assert_eq!(placed[2].text, "Alcove");
            let rects: Vec<Rect> = placed
                .iter()
                .map(|p| match p.spot {
                    Spot::Text { rect, .. } => rect,
                    Spot::Marker { x, y, radius, .. } => Rect::centred(x, y, 2.0 * radius, 2.0 * radius),
                })
                .collect();
            for (i, a) in rects.iter().enumerate() {
                for b in &rects[i + 1..] {
                    assert!(!a.overlaps(b), "at {long_side} px {a:?} overlaps {b:?}");
                }
                assert!(a.x0 >= 0.0 && a.x1 <= view.width && a.y0 >= 0.0 && a.y1 <= view.height - view.footer);
            }
            if let Spot::Text { rect, .. } = placed[2].spot {
                let alcove = Shapes::of(&features[1].wkb, &view).expect("shapes").rings;
                assert!(touches(&rect, &alcove), "the alcove's label covers part of the alcove");
            }
        }
    }

    #[test]
    fn long_text_wraps_without_losing_anything() {
        let font = Measure::new().expect("font");
        let text = "A rather long name for a small cupboard under the stairs";
        let lines = wrap(text, 80.0, 12.0, &font);
        assert!(lines.len() > 1);
        assert_eq!(lines.join(" "), text);
        for line in &lines {
            assert!(font.width(line, 12.0) <= 80.0, "{line}");
        }
        let word = wrap("Supercalifragilistic", 40.0, 12.0, &font);
        assert_eq!(word.concat(), "Supercalifragilistic");
    }
}

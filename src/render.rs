//! Draws a plan as SVG and rasterizes it to PNG.
//!
//! Geometry arrives as WKB and is read through geozero; the only arithmetic
//! here is the map from site metres to pixels. Label positions and the
//! extent come from PostGIS.

use std::{fmt::Write as _, sync::Arc};

use geozero::{GeomProcessor, GeozeroGeometry, wkb::Wkb};
use resvg::{tiny_skia, usvg};

use crate::markup::escape;

pub const MIN_SIDE_PX: u32 = 200;
pub const MAX_SIDE_PX: u32 = 4096;
const MARGIN: f64 = 40.0;
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
/// The dashes of an arch's curve, in pixels.
const ARCH_DASHES: &str = "4 3";
/// The deepest an arch's curve reaches from its chord, in metres.
const MAX_ARCH_DEPTH: f64 = 0.4;
/// Straight pieces in an arch's half ellipse.
const ARCH_SEGMENTS: u32 = 24;

pub struct Plan {
    pub title: String,
    /// Degrees clockwise from grid north to the site's y axis.
    pub rotation: f64,
    pub extent: Extent,
    pub features: Vec<Feature>,
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
    /// draws the jamb ticks, and for an arch the dashed curve across the gap.
    Opening {
        from: (f64, f64),
        to: (f64, f64),
        wall_depth: f64,
        arch: bool,
    },
}

pub struct Rendered {
    pub svg: String,
    pub png: Vec<u8>,
}

/// Site metres to pixels, y flipped so north is up.
struct View {
    xmin: f64,
    ymax: f64,
    scale: f64,
    width: f64,
    height: f64,
}

impl View {
    fn new(extent: Extent, long_side: f64) -> Self {
        // A plan of a single point still needs a window around it.
        let dx = (extent.xmax - extent.xmin).max(1.0);
        let dy = (extent.ymax - extent.ymin).max(1.0);
        // The scale is whichever of the width and the height fits the long
        // side first, so neither side of the image passes it.
        let scale = ((long_side - 2.0 * MARGIN - NORTH_COLUMN) / dx).min((long_side - 2.0 * MARGIN - FOOTER) / dy);
        let width = dx * scale + 2.0 * MARGIN + NORTH_COLUMN;
        let height = dy * scale + 2.0 * MARGIN + FOOTER;
        Self {
            xmin: extent.xmin,
            ymax: extent.ymax,
            scale,
            width,
            height,
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
    let view = View::new(plan.extent, f64::from(long_side_px.clamp(MIN_SIDE_PX, MAX_SIDE_PX)));
    let svg = svg(plan, &view)?;
    let png = rasterize(&svg)?;
    Ok(Rendered { svg, png })
}

fn svg(plan: &Plan, view: &View) -> Result<String, String> {
    let mut out = String::new();
    let (w, h) = (view.width.round(), view.height.round());
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="Noto Sans">"##
    );
    let _ = write!(out, r#"<rect width="{w}" height="{h}" fill="white"/>"#);
    // Linework under spaces under openings under objects, highlights last so
    // nothing covers them.
    let order = |f: &Feature| match (&f.style, f.highlight) {
        (_, true) => 4,
        (Style::Linework, _) => 0,
        (Style::Space { .. }, _) => 1,
        (Style::Opening { .. }, _) => 2,
        (Style::Object { .. }, _) => 3,
    };
    let mut features: Vec<&Feature> = plan.features.iter().collect();
    features.sort_by_key(|f| order(f));
    for feature in &features {
        draw_feature(&mut out, feature, view)?;
    }
    draw_labels(&mut out, &features, view);
    draw_scale_bar(&mut out, view);
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
        arch,
    } = feature.style
    {
        draw_opening(out, from, to, wall_depth, arch, feature.highlight, view);
        return Ok(());
    }
    let mut shapes = Shapes::new(view);
    Wkb(&feature.wkb)
        .process_geom(&mut shapes)
        .map_err(|e| format!("unreadable geometry: {e}"))?;
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
        let area_fill = if shapes.has_area { fill.as_str() } else { "none" };
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
/// long, and for an arch a dashed half ellipse whose chord is the opening,
/// on the left of the line's direction, as deep as a quarter of its width
/// up to `MAX_ARCH_DEPTH`.
fn draw_opening(
    out: &mut String,
    from: (f64, f64),
    to: (f64, f64),
    wall_depth: f64,
    arch: bool,
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
    for (x, y) in [from, to] {
        let (x1, y1) = view.px(x - nx * half, y - ny * half);
        let (x2, y2) = view.px(x + nx * half, y + ny * half);
        let _ = write!(
            out,
            r#"<line x1="{x1:.2}" y1="{y1:.2}" x2="{x2:.2}" y2="{y2:.2}" stroke="{stroke}" stroke-width="{LINEWORK_WIDTH}"/>"#
        );
    }
    if !arch {
        return;
    }
    let depth = (width / 4.0).min(MAX_ARCH_DEPTH);
    let (cx, cy) = ((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
    let mut path = String::new();
    for i in 0..=ARCH_SEGMENTS {
        // From the start of the opening at t = pi to its end at t = 0, so
        // sin t, and the curve, stays on the left.
        let t = std::f64::consts::PI * f64::from(ARCH_SEGMENTS - i) / f64::from(ARCH_SEGMENTS);
        let (along, across) = (width / 2.0 * t.cos(), depth * t.sin());
        let (px, py) = view.px(cx + ux * along + nx * across, cy + uy * along + ny * across);
        let command = if i == 0 { 'M' } else { 'L' };
        let _ = write!(path, "{command}{px:.2} {py:.2} ");
    }
    let _ = write!(
        out,
        r#"<path d="{path}" fill="none" stroke="{stroke}" stroke-width="{LINEWORK_WIDTH}" stroke-dasharray="{ARCH_DASHES}"/>"#
    );
}

/// Draws every label in a group that clips them out of the square the north
/// arrow turns in. A label is as long as its name, so one that starts near
/// the plan's right edge would otherwise run under the arrow. The clip's id
/// carries the image size, so two plans inlined in one page do not share a
/// clip unless they share its shape too.
fn draw_labels(out: &mut String, features: &[&Feature], view: &View) {
    let (w, h) = (view.width.round(), view.height.round());
    let id = format!("house-plan-labels-{w}x{h}");
    let _ = write!(
        out,
        concat!(
            r#"<clipPath id="{id}">"#,
            r#"<rect width="{arrow_left:.2}" height="{h}"/>"#,
            r#"<rect y="{arrow_bottom:.2}" width="{w}" height="{h}"/>"#,
            r#"</clipPath><g clip-path="url(#{id})">"#,
        ),
        id = id,
        arrow_left = view.width - MARGIN - ARROW_REACH,
        arrow_bottom = ARROW_Y + ARROW_REACH,
        w = w,
        h = h,
    );
    for feature in features {
        if let Some(label) = &feature.label {
            draw_label(out, label, &feature.style, view);
        }
    }
    out.push_str("</g>");
}

fn draw_label(out: &mut String, label: &Label, style: &Style, view: &View) {
    let (x, y) = view.px(label.x, label.y);
    let (size, dx, anchor) = match style {
        Style::Object { .. } => (11, 10.0, "start"),
        _ => (14, 0.0, "middle"),
    };
    let _ = write!(
        out,
        r##"<text x="{:.2}" y="{:.2}" font-size="{size}" text-anchor="{anchor}" fill="#111" stroke="white" stroke-width="3" paint-order="stroke">{}</text>"##,
        x + dx,
        y + f64::from(size) / 3.0,
        escape(&label.text)
    );
}

/// A bar of a round length, about a quarter of the plan's width.
fn draw_scale_bar(out: &mut String, view: &View) {
    let target = (view.width - 2.0 * MARGIN - NORTH_COLUMN) / view.scale / 4.0;
    let metres = round_length(target);
    let length = metres * view.scale;
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
/// `2 * MARGIN` down.
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

/// Collects one geometry as SVG path data in pixels, and its points as
/// circle centres.
struct Shapes<'a> {
    view: &'a View,
    path: String,
    points: Vec<(f64, f64)>,
    polygon_depth: usize,
    point_depth: usize,
    ring_start: bool,
    has_area: bool,
}

impl<'a> Shapes<'a> {
    fn new(view: &'a View) -> Self {
        Self {
            view,
            path: String::new(),
            points: Vec::new(),
            polygon_depth: 0,
            point_depth: 0,
            ring_start: false,
            has_area: false,
        }
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
        self.has_area = true;
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
}

//! The plan renderer, checked by the pixels it produces.

use house_mcp::{
    geom::to_wkb,
    render::{Extent, Feature, Label, Plan, Rendered, Style, render},
};
use resvg::tiny_skia::Pixmap;
use serde_json::json;

fn feature(geometry: serde_json::Value, style: Style, label: Option<(&str, f64, f64)>, highlight: bool) -> Feature {
    Feature {
        wkb: to_wkb(&geometry).expect("geometry"),
        style,
        label: label.map(|(text, x, y)| Label {
            text: text.into(),
            x,
            y,
        }),
        highlight,
    }
}

/// The two rooms of the test drawing, 9 by 3 metres, with the light at
/// (2, 2.5) and the site turned 30 degrees from grid north.
fn plan(highlight_light: bool) -> Plan {
    let room = |x0: f64, x1: f64| json!({"type": "Polygon", "coordinates": [[[x0, 0.0], [x1, 0.0], [x1, 3.0], [x0, 3.0], [x0, 0.0]]]});
    Plan {
        title: "Test site, Ground".into(),
        rotation: 30.0,
        extent: Extent {
            xmin: 0.0,
            ymin: 0.0,
            xmax: 9.0,
            ymax: 3.0,
        },
        features: vec![
            feature(
                room(0.0, 4.0),
                Style::Space { kind: "room".into() },
                Some(("Kitchen", 2.0, 1.5)),
                false,
            ),
            feature(
                room(4.0, 9.0),
                Style::Space { kind: "room".into() },
                Some(("Living room", 6.5, 1.5)),
                false,
            ),
            feature(
                json!({"type": "Point", "coordinates": [2.0, 2.5]}),
                Style::Object { kind: "light".into() },
                Some(("LIGHT", 2.0, 2.5)),
                highlight_light,
            ),
        ],
    }
}

fn pixel(png: &[u8], x: u32, y: u32) -> (u8, u8, u8) {
    let pixmap = Pixmap::decode_png(png).expect("a PNG");
    let p = pixmap.pixel(x, y).expect("inside the image");
    (p.red(), p.green(), p.blue())
}

/// Pixel position of a site point at the default size: 40 px margins and
/// (1600 - 80) / 9 px per metre.
fn at(x: f64, y: f64) -> (u32, u32) {
    let scale = 1520.0 / 9.0;
    (
        (40.0 + x * scale).round() as u32,
        (40.0 + (3.0 - y) * scale).round() as u32,
    )
}

fn rendered(highlight: bool) -> Rendered {
    render(&plan(highlight), 1600).expect("renders")
}

#[test]
fn long_side_is_the_requested_size() {
    let pixmap = Pixmap::decode_png(&rendered(false).png).expect("a PNG");
    assert_eq!(pixmap.width(), 1600);
    // 3 m of 9 m at the same scale, plus margins and the footer.
    assert_eq!(pixmap.height(), (3.0 * 1520.0 / 9.0 + 80.0 + 50.0_f64).round() as u32);
}

#[test]
fn rooms_are_filled_and_the_light_is_drawn_where_it_is() {
    let png = rendered(false).png;
    let (x, y) = at(3.0, 0.5);
    let room = pixel(&png, x, y);
    assert_ne!(room, (255, 255, 255), "inside the kitchen is filled");
    let (x, y) = at(2.0, 2.5);
    let light = pixel(&png, x, y);
    assert_ne!(light, room, "the light is drawn over the room");
    let (x, y) = at(9.0, 3.0);
    assert_eq!(
        pixel(&png, x + 20, y - 20),
        (255, 255, 255),
        "outside the plan is background"
    );
}

#[test]
fn highlight_changes_the_light_colour() {
    let (x, y) = at(2.0, 2.5);
    let plain = pixel(&rendered(false).png, x, y);
    let highlighted = pixel(&rendered(true).png, x, y);
    assert_ne!(plain, highlighted);
    // The highlight orange, #e8590c.
    assert_eq!(highlighted, (0xe8, 0x59, 0x0c));
}

#[test]
fn svg_has_labels_scale_bar_and_north_arrow() {
    let svg = rendered(false).svg;
    for label in ["Kitchen", "Living room", "LIGHT", "Test site, Ground"] {
        assert!(svg.contains(&format!(">{label}</text>")), "{label} missing");
    }
    // A quarter of 9 m rounds down to 2 m.
    assert!(svg.contains(">2 m</text>"), "scale bar");
    assert!(
        svg.contains("rotate(-30.000)"),
        "north arrow turned back by the site rotation"
    );
}

#[test]
fn labels_are_escaped() {
    let mut plan = plan(false);
    plan.title = "<script>&".into();
    let svg = render(&plan, 400).expect("renders").svg;
    assert!(svg.contains("&lt;script&gt;&amp;"));
    assert!(!svg.contains("<script>"));
}

#[test]
fn labels_are_drawn_with_the_embedded_font() {
    let dark_pixels_near_kitchen = |plan: &Plan| {
        let pixmap = Pixmap::decode_png(&render(plan, 1600).expect("renders").png).expect("a PNG");
        let (cx, cy) = at(2.0, 1.5);
        let mut dark = 0;
        for x in cx - 40..cx + 40 {
            for y in cy - 15..cy + 15 {
                let p = pixmap.pixel(x, y).expect("inside");
                if p.red() < 100 && p.green() < 100 && p.blue() < 100 {
                    dark += 1;
                }
            }
        }
        dark
    };
    let labelled = plan(false);
    let mut unlabelled = plan(false);
    unlabelled.features[0].label = None;
    assert!(dark_pixels_near_kitchen(&labelled) > 50);
    assert_eq!(dark_pixels_near_kitchen(&unlabelled), 0);
}

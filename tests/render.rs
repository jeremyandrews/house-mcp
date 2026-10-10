//! The plan renderer, checked by the pixels it produces.

use std::ops::RangeInclusive;

use house_mcp::{
    geom::to_wkb,
    render::{Extent, Feature, Label, Plan, Rendered, Style, render},
};
use resvg::tiny_skia::Pixmap;
use serde_json::json;

/// The footer's height in pixels, as in src/render.rs.
const FOOTER: u32 = 50;

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

/// Pixel position of a site point at the default size: 40 px margins, the
/// 40 px north arrow column, and (1600 - 120) / 9 px per metre.
fn at(x: f64, y: f64) -> (u32, u32) {
    let scale = 1480.0 / 9.0;
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
    assert_eq!(pixmap.height(), (3.0 * 1480.0 / 9.0 + 80.0 + 50.0_f64).round() as u32);
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

/// The pixels that differ between two renders of the same size.
fn changed_pixels(a: &Pixmap, b: &Pixmap) -> Vec<(u32, u32)> {
    (0..a.height())
        .flat_map(|y| (0..a.width()).map(move |x| (x, y)))
        .filter(|&(x, y)| a.pixel(x, y) != b.pixel(x, y))
        .collect()
}

/// The smallest box, as inclusive ranges, holding every pixel.
fn bounds(pixels: &[(u32, u32)]) -> (RangeInclusive<u32>, RangeInclusive<u32>) {
    let xs = pixels.iter().map(|p| p.0);
    let ys = pixels.iter().map(|p| p.1);
    (
        xs.clone().min().expect("pixels")..=xs.max().expect("pixels"),
        ys.clone().min().expect("pixels")..=ys.max().expect("pixels"),
    )
}

fn pixmap_of(plan: &Plan, size: u32) -> Pixmap {
    Pixmap::decode_png(&render(plan, size).expect("renders").png).expect("a PNG")
}

/// A plan of one room filling the extent, `dx` by `dy` metres.
fn room_plan(dx: f64, dy: f64, rotation: f64) -> Plan {
    let room = json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [dx, 0.0], [dx, dy], [0.0, dy], [0.0, 0.0]]]});
    Plan {
        title: "Shape".into(),
        rotation,
        extent: Extent {
            xmin: 0.0,
            ymin: 0.0,
            xmax: dx,
            ymax: dy,
        },
        features: vec![feature(room, Style::Space { kind: "room".into() }, None, false)],
    }
}

#[test]
fn north_arrow_stays_off_the_plan_and_the_footer_at_every_angle_and_shape() {
    // Renders that differ in one thing differ only in its pixels: the room's
    // when it is left out, the arrow's when the rotation changes by any turn
    // short of a whole circle. The arrow, margins, column and footer are
    // fixed in pixels, so small sizes are the hardest case.
    for (dx, dy) in [(9.0, 3.0), (3.0, 9.0), (4.0, 4.0), (40.0, 1.0), (1.0, 40.0)] {
        for size in [200, 600] {
            let mut empty = room_plan(dx, dy, 0.0);
            empty.features.clear();
            let (plan_x, plan_y) = bounds(&changed_pixels(
                &pixmap_of(&room_plan(dx, dy, 0.0), size),
                &pixmap_of(&empty, size),
            ));
            for rotation in (0..8).map(|i| f64::from(i) * 45.0) {
                let a = pixmap_of(&room_plan(dx, dy, rotation), size);
                let b = pixmap_of(&room_plan(dx, dy, rotation + 100.0), size);
                let (w, h) = (a.width(), a.height());
                let arrow = changed_pixels(&a, &b);
                let case = format!("{dx} by {dy} m at {size} px, {rotation} degrees");
                assert!(!arrow.is_empty(), "{case}: no arrow drawn");
                for (x, y) in arrow {
                    let on_plan = plan_x.contains(&x) && plan_y.contains(&y);
                    assert!(!on_plan, "{case}: arrow on the plan at ({x}, {y})");
                    assert!(y < h - FOOTER, "{case}: arrow in the footer at ({x}, {y})");
                    assert!(
                        x > 0 && x < w - 1 && y > 0,
                        "{case}: arrow at the image edge at ({x}, {y})"
                    );
                }
            }
        }
    }
}

#[test]
fn a_long_label_at_the_plan_edge_stays_out_from_under_the_north_arrow() {
    let with_light = |rotation, y: f64, label: bool| {
        let mut plan = room_plan(9.0, 3.0, rotation);
        let light = json!({"type": "Point", "coordinates": [9.0, y]});
        let label = label.then_some(("A ceiling light with a long name", 9.0, y));
        plan.features
            .push(feature(light, Style::Object { kind: "light".into() }, label, false));
        plan
    };
    for size in [200, 1600] {
        let mut arrow = Vec::new();
        for rotation in (0..8).map(|i| f64::from(i) * 45.0) {
            arrow.extend(changed_pixels(
                &pixmap_of(&with_light(rotation, 3.0, false), size),
                &pixmap_of(&with_light(rotation + 100.0, 3.0, false), size),
            ));
        }
        let (arrow_x, arrow_y) = bounds(&arrow);
        // Labels level with the arrow's top, middle and lower rows, and one
        // well below it.
        for y in [3.0, 2.9, 2.8, 0.5] {
            let text = changed_pixels(
                &pixmap_of(&with_light(0.0, y, true), size),
                &pixmap_of(&with_light(0.0, y, false), size),
            );
            let case = format!("at {size} px, a label at y = {y} m");
            assert!(!text.is_empty(), "{case}: no label drawn");
            for &(x, y) in &text {
                let under_arrow = arrow_x.contains(&x) && arrow_y.contains(&y);
                assert!(!under_arrow, "{case}: label under the arrow at ({x}, {y})");
            }
            // At 1600 px the lowest label is well below the arrow, so it runs
            // on into the arrow's column unclipped.
            if size == 1600 && y == 0.5 {
                assert!(
                    text.iter().any(|&(x, _)| arrow_x.contains(&x)),
                    "{case}: label clipped in the column below the arrow"
                );
            }
        }
    }
}

#[test]
fn neither_side_passes_the_requested_size() {
    for (dx, dy) in [(9.0, 3.0), (3.0, 9.0), (10.0, 10.0), (10.0, 9.99), (0.0, 0.0)] {
        for size in [200, 1600] {
            let pixmap = pixmap_of(&room_plan(dx, dy, 0.0), size);
            let long = pixmap.width().max(pixmap.height());
            assert_eq!(
                long,
                size,
                "{dx} by {dy} m at {size} px is {}x{}",
                pixmap.width(),
                pixmap.height()
            );
        }
    }
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

#[test]
fn tall_plan_fits_the_long_side_with_room_for_the_footer() {
    // 3 m wide and 9 m tall, with a room filling the bottom 3 m. The scale
    // is (1600 - 80 - 50) / 9 pixels per metre, so the footer never covers
    // the plan.
    let room =
        json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [3.0, 0.0], [3.0, 3.0], [0.0, 3.0], [0.0, 0.0]]]});
    let outline = json!({"type": "LineString", "coordinates": [[0.0, 0.0], [3.0, 9.0]]});
    let plan = Plan {
        title: "Tall".into(),
        rotation: 0.0,
        extent: Extent {
            xmin: 0.0,
            ymin: 0.0,
            xmax: 3.0,
            ymax: 9.0,
        },
        features: vec![
            feature(room, Style::Space { kind: "room".into() }, None, false),
            feature(outline, Style::Linework, None, false),
        ],
    };
    let png = render(&plan, 1600).expect("renders").png;
    let pixmap = Pixmap::decode_png(&png).expect("a PNG");
    let scale = 1470.0 / 9.0;
    assert_eq!(pixmap.height(), 1600);
    assert_eq!(pixmap.width(), (3.0 * scale + 120.0_f64).round() as u32);
    let (x, y) = ((40.0 + 2.5 * scale).round() as u32, (40.0 + 8.8 * scale).round() as u32);
    assert_ne!(pixel(&png, x, y), (255, 255, 255), "the bottom of the room is drawn");
}

//! The plan renderer, checked by the pixels it produces.

use std::ops::RangeInclusive;

use house_mcp::{
    geom::to_wkb,
    render::{Extent, Feature, Label, LabelSize, Plan, Rendered, Sheet, Style, Symbol, render},
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
        label_size: LabelSize::Normal,
        not_placed: 0,
        sheet: Sheet::default(),
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
                Style::Object {
                    kind: "light".into(),
                    approximate: false,
                },
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
        label_size: LabelSize::Normal,
        not_placed: 0,
        sheet: Sheet::default(),
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
        plan.features.push(feature(
            light,
            Style::Object {
                kind: "light".into(),
                approximate: false,
            },
            label,
            false,
        ));
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
            // Nothing is cut: the label is drawn whole somewhere, or listed
            // whole in the legend.
            let rendered = render(&with_light(0.0, y, true), size).expect("renders");
            assert!(
                rendered.svg.contains(">A ceiling light with a long name</text>"),
                "{case}: label cut"
            );
            // At 1600 px there is room left of the light, so the label is
            // drawn there rather than off the image's right edge.
            if size == 1600 {
                assert!(rendered.legend.is_empty(), "{case}: {:?}", rendered.legend);
                let (light_x, _) = at(9.0, y);
                assert!(
                    text.iter().all(|&(x, _)| x < light_x + 8),
                    "{case}: label right of the light"
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
        label_size: LabelSize::Normal,
        not_placed: 0,
        sheet: Sheet::default(),
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

/// Every `<text>` element's attributes and content, in order.
fn texts(svg: &str) -> Vec<(String, String)> {
    svg.split("<text ")
        .skip(1)
        .filter_map(|rest| {
            let (attributes, rest) = rest.split_once('>')?;
            let (content, _) = rest.split_once("</text>")?;
            Some((attributes.to_owned(), content.to_owned()))
        })
        .collect()
}

/// A number attribute of an element.
fn number(attributes: &str, name: &str) -> f64 {
    let start = attributes
        .find(&format!(" {name}=\""))
        .or_else(|| attributes.find(&format!("{name}=\"")))
        .expect(name);
    let rest = &attributes[start..];
    let value = rest.split('"').nth(1).expect("a value");
    value.parse().expect("a number")
}

/// The attributes of the `<text>` holding exactly `text`.
fn text_attributes(svg: &str, text: &str) -> String {
    texts(svg)
        .into_iter()
        .find(|(_, content)| content == text)
        .unwrap_or_else(|| panic!("no text {text}"))
        .0
}

/// One room filling a 9 by 3 m plan, and lights at the given points with
/// the given labels.
fn lights(at: &[(&str, f64, f64)]) -> Plan {
    let mut plan = room_plan(9.0, 3.0, 0.0);
    for &(label, x, y) in at {
        plan.features.push(feature(
            json!({"type": "Point", "coordinates": [x, y]}),
            Style::Object {
                kind: "light".into(),
                approximate: false,
            },
            Some((label, x, y)),
            false,
        ));
    }
    plan
}

#[test]
fn two_overlapping_labels_move_apart() {
    let rendered = render(&lights(&[("Pendant", 4.0, 1.5), ("Spotlight", 4.0, 1.5)]), 1600).expect("renders");
    let a = text_attributes(&rendered.svg, "Pendant");
    let b = text_attributes(&rendered.svg, "Spotlight");
    let size = number(&a, "font-size");
    assert_eq!(size, 11.0, "both fit at full size");
    let (ya, yb) = (number(&a, "y"), number(&b, "y"));
    assert!((ya - yb).abs() >= 1.2 * size, "labels {ya} and {yb} overlap");
    // The first keeps its place right of the light; the second moved.
    let (light_x, light_y) = at(4.0, 1.5);
    assert!(number(&a, "x") > f64::from(light_x));
    assert!(
        (ya - (f64::from(light_y) + size / 3.0)).abs() < 1.0,
        "first label at its anchor"
    );
    assert!(rendered.legend.is_empty());
}

#[test]
fn a_label_with_no_room_becomes_a_numbered_marker_and_a_legend_entry() {
    // Twenty labels on one point of a small image cannot all stand there.
    let names: Vec<String> = (1..=20).map(|i| format!("Ceiling light number {i}")).collect();
    let crowd: Vec<(&str, f64, f64)> = names.iter().map(|n| (n.as_str(), 4.5, 1.5)).collect();
    let rendered = render(&lights(&crowd), 400).expect("renders");
    assert!(!rendered.legend.is_empty(), "some labels went to the legend");
    assert!(rendered.svg.contains(r#"class="label-marker""#));
    let markers = rendered.svg.matches(r#"class="label-marker""#).count();
    assert_eq!(markers, rendered.legend.len(), "one marker per legend entry");
    for (i, entry) in rendered.legend.iter().enumerate() {
        // The marker's number and the legend's, and the entry in full.
        assert!(rendered.svg.contains(&format!(">{}</text>", i + 1)));
        assert!(rendered.svg.contains(&format!(">{entry}</text>")), "{entry}");
    }
    // The legend is in the footer, below every marker on the plan, and the
    // image still keeps to its size.
    let pixmap = Pixmap::decode_png(&rendered.png).expect("a PNG");
    assert_eq!(pixmap.width().max(pixmap.height()), 400);
    let lowest_marker = rendered
        .svg
        .split(r#"class="label-marker"><circle "#)
        .skip(1)
        .map(|circle| number(circle, "cy"))
        .fold(0.0, f64::max);
    for entry in &rendered.legend {
        assert!(number(&text_attributes(&rendered.svg, entry), "y") > lowest_marker + 20.0);
    }
}

#[test]
fn no_label_is_ever_cut_short() {
    let names: Vec<String> = (1..=30)
        .map(|i| format!("A light with a long descriptive name, number {i}"))
        .collect();
    // A wrapped legend entry's text with its line breaks put back as spaces.
    let plain = |content: &str| {
        let mut text = String::new();
        let mut in_tag = false;
        for c in content.replace("</tspan><tspan", "</tspan> <tspan").chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        text
    };
    for size in [200, 600, 1600] {
        let crowd: Vec<(&str, f64, f64)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), 0.3 * (i % 10) as f64 + 3.0, 1.0 + 0.1 * (i / 10) as f64))
            .collect();
        let rendered = render(&lights(&crowd), size).expect("renders");
        let drawn: Vec<String> = texts(&rendered.svg).into_iter().map(|(_, t)| plain(&t)).collect();
        let mut in_legend = 0;
        for name in &names {
            // Each name appears whole, once, either by its light or in the
            // legend.
            assert_eq!(
                drawn.iter().filter(|t| *t == name).count(),
                1,
                "at {size} px, {name} is missing, cut or doubled"
            );
            in_legend += usize::from(rendered.legend.contains(name));
        }
        assert_eq!(in_legend, rendered.legend.len());
        assert!(in_legend > 0, "at {size} px some labels need the legend");
        // A legend the image can hold leaves its size alone; at the smaller
        // sizes this one cannot be held, and the image grows taller rather
        // than cut it, keeping its width.
        let pixmap = Pixmap::decode_png(&rendered.png).expect("a PNG");
        assert!(pixmap.width() <= size);
        if size == 1600 {
            assert_eq!(pixmap.width().max(pixmap.height()), size);
        }
    }
}

#[test]
fn label_size_scales_the_label_font() {
    let sizes: Vec<f64> = [LabelSize::Small, LabelSize::Normal, LabelSize::Large]
        .into_iter()
        .map(|label_size| {
            let mut plan = plan(false);
            plan.label_size = label_size;
            let svg = render(&plan, 1600).expect("renders").svg;
            number(&text_attributes(&svg, "Kitchen"), "font-size")
        })
        .collect();
    assert_eq!(sizes[1], 14.0);
    assert!(sizes[0] < sizes[1] && sizes[1] < sizes[2], "{sizes:?}");
}

/// A 6 by 4 m plan, with room for any swing, and one opening from (2, 2)
/// to (3.6, 2), running east along a wall 0.3 m deep.
fn opening(symbol: Symbol) -> String {
    let mut plan = room_plan(6.0, 4.0, 0.0);
    plan.features.push(feature(
        json!({"type": "LineString", "coordinates": [[2.0, 2.0], [3.6, 2.0]]}),
        Style::Opening {
            from: (2.0, 2.0),
            to: (3.6, 2.0),
            wall_depth: 0.3,
            symbol,
        },
        None,
        false,
    ));
    render(&plan, 1600).expect("renders").svg
}

/// The pixel of a site point on the 6 by 4 m plan at 1600 px.
fn px6(x: f64, y: f64) -> (f64, f64) {
    let scale = 1480.0 / 6.0;
    (40.0 + x * scale, 40.0 + (4.0 - y) * scale)
}

/// The `<line>` elements with this class, as (x1, y1, x2, y2).
fn lines(svg: &str, class: &str) -> Vec<(f64, f64, f64, f64)> {
    svg.split("<line ")
        .skip(1)
        .filter_map(|rest| rest.split_once("/>").map(|(a, _)| a))
        .filter(|a| a.contains(&format!(r#"class="{class}""#)))
        .map(|a| (number(a, "x1"), number(a, "y1"), number(a, "x2"), number(a, "y2")))
        .collect()
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.05
}

#[test]
fn an_arch_is_two_dashed_lines_along_the_wall_faces_and_no_curve() {
    let svg = opening(Symbol::Arch);
    let faces = lines(&svg, "arch-face");
    assert_eq!(faces.len(), 2);
    assert_eq!(svg.matches("<line ").count(), 4, "two jamb ticks and two faces");
    assert_eq!(svg.matches("stroke-dasharray").count(), 2);
    assert!(!svg.contains("<path class"), "no curve");
    let (x0, _) = px6(2.0, 1.5);
    let (x1, _) = px6(3.6, 1.5);
    for face in &faces {
        // Straight across from jamb to jamb.
        assert!(near(face.1, face.3), "straight across");
        assert!(near(face.0.min(face.2), x0) && near(face.0.max(face.2), x1));
    }
    // One along each face of the 0.3 m wall, 0.15 m either side of its
    // centre line.
    let mut ys: Vec<f64> = faces.iter().map(|f| f.1).collect();
    ys.sort_by(f64::total_cmp);
    assert!(near(ys[0], px6(0.0, 2.15).1) && near(ys[1], px6(0.0, 1.85).1), "{ys:?}");
    let plain = opening(Symbol::Plain);
    assert_eq!(plain.matches("<line ").count(), 2);
    assert_eq!(plain.matches("stroke-dasharray").count(), 0);
}

#[test]
fn a_door_hangs_from_its_hinge_and_swings_to_its_side() {
    let (start, _) = px6(2.0, 1.5);
    let (end, _) = px6(3.6, 1.5);
    let width = 1.6 * 1480.0 / 6.0;
    for (hinge_end, swing_right) in [(false, false), (true, true), (false, true), (true, false)] {
        let svg = opening(Symbol::Door {
            hinge_end,
            swing_right,
            double: false,
        });
        let case = format!(
            "hinge at the {}, swinging {}",
            if hinge_end { "end" } else { "start" },
            if swing_right { "right" } else { "left" }
        );
        let leaves = lines(&svg, "door-leaf");
        assert_eq!(leaves.len(), 1, "{case}");
        let (x1, y1, x2, y2) = leaves[0];
        // The leaf stands square to the wall at the hinge jamb, from the
        // face on its side; the line runs east, so left is up the image.
        let hinge = if hinge_end { end } else { start };
        assert!(near(x1, hinge) && near(x2, hinge), "{case}: leaf at x = {x1}, {x2}");
        let face = px6(0.0, if swing_right { 1.85 } else { 2.15 }).1;
        assert!(near(y1, face), "{case}: leaf starts at y = {y1}");
        let reach = if swing_right { width } else { -width };
        assert!(near(y2 - y1, reach), "{case}: leaf reaches {}", y2 - y1);
        // The swing runs from the leaf's tip to the other jamb.
        assert_eq!(svg.matches(r#"class="door-swing""#).count(), 1, "{case}");
        let other = if hinge_end { start } else { end };
        let swing = svg
            .split(r#"class="door-swing" d="M"#)
            .nth(1)
            .expect("swing")
            .split('"')
            .next()
            .expect("d");
        let numbers: Vec<f64> = swing
            .split([' ', 'A'])
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().expect("a number"))
            .collect();
        assert!(
            near(numbers[0], x2) && near(numbers[1], y2),
            "{case}: swing starts at the tip"
        );
        let (ex, ey) = (numbers[numbers.len() - 2], numbers[numbers.len() - 1]);
        assert!(near(ex, other) && near(ey, face), "{case}: swing ends at ({ex}, {ey})");
        assert!(near(numbers[2], width), "{case}: a quarter circle as wide as the door");
    }
}

#[test]
fn a_pair_of_doors_draws_two_half_leaves_from_both_jambs() {
    let svg = opening(Symbol::Door {
        hinge_end: false,
        swing_right: false,
        double: true,
    });
    let leaves = lines(&svg, "door-leaf");
    assert_eq!(leaves.len(), 2);
    assert_eq!(svg.matches(r#"class="door-swing""#).count(), 2);
    let (start, _) = px6(2.0, 1.5);
    let (end, _) = px6(3.6, 1.5);
    let half = 0.8 * 1480.0 / 6.0;
    let mut xs: Vec<f64> = leaves.iter().map(|l| l.0).collect();
    xs.sort_by(f64::total_cmp);
    assert!(near(xs[0], start) && near(xs[1], end), "{xs:?}");
    for (_, y1, _, y2) in leaves {
        assert!(near(y1 - y2, half), "half a leaf each");
    }
}

#[test]
fn a_door_opening_out_of_the_plan_stays_in_the_image() {
    // A door in the north wall of a 6 by 3 m room, opening outwards.
    let mut plan = room_plan(6.0, 3.0, 0.0);
    plan.features.push(feature(
        json!({"type": "LineString", "coordinates": [[2.0, 3.0], [3.0, 3.0]]}),
        Style::Opening {
            from: (2.0, 3.0),
            to: (3.0, 3.0),
            wall_depth: 0.3,
            symbol: Symbol::Door {
                hinge_end: false,
                swing_right: false,
                double: false,
            },
        },
        None,
        false,
    ));
    let svg = render(&plan, 1600).expect("renders").svg;
    let (_, y1, _, y2) = lines(&svg, "door-leaf")[0];
    assert!(y2 < y1, "the leaf opens north, up the image");
    assert!(y2 >= 40.0 - 0.01, "the leaf's tip is inside the margin, at {y2}");
}

//! The DXF reader against drawings the tests write themselves.

mod common;

use std::collections::HashMap;

use common::{drawing, on_layer, rectangle, save, two_room_dxf};
use dxf::{
    Block, Point,
    entities::{Arc, Circle, Ellipse, Entity, EntityType, Insert, Line, LwPolyline, Polyline, Vertex},
    enums::Units,
};
use house_mcp::{
    dxf_import::{ImportError, LayerTarget, MAX_BLOCK_DEPTH, Options, Parsed, Transform, parse},
    units::LengthUnit,
};
use serde_json::json;

fn mapping() -> HashMap<String, LayerTarget> {
    HashMap::from([
        // Different case from the drawing's layer names on purpose.
        ("a-room".to_owned(), LayerTarget::Space("room".into())),
        ("E-Light".to_owned(), LayerTarget::Object("light".into())),
    ])
}

fn options(unit: Option<LengthUnit>, transform: Transform) -> Options {
    Options {
        unit,
        layers: mapping(),
        transform,
    }
}

fn read(bytes: &[u8]) -> Parsed {
    parse(bytes, &options(None, Transform::default())).expect("the fixture reads")
}

#[test]
fn rooms_become_spaces_in_metres() {
    let parsed = read(&two_room_dxf());
    assert_eq!(parsed.unit, LengthUnit::Millimetres);
    assert!(!parsed.unit_from_call);
    let polygons: Vec<_> = parsed.spaces.iter().map(|s| (s.kind.as_str(), &s.polygon)).collect();
    assert_eq!(
        polygons,
        vec![
            (
                "room",
                &json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [4.0, 0.0], [4.0, 3.0], [0.0, 3.0], [0.0, 0.0]]]})
            ),
            (
                "room",
                &json!({"type": "Polygon", "coordinates": [[[4.0, 0.0], [9.0, 0.0], [9.0, 3.0], [4.0, 3.0], [4.0, 0.0]]]})
            ),
        ]
    );
}

#[test]
fn block_insert_becomes_an_object_at_its_insertion_point() {
    let parsed = read(&two_room_dxf());
    assert_eq!(parsed.objects.len(), 1);
    let light = &parsed.objects[0];
    assert_eq!(
        (light.kind.as_str(), light.block.as_str(), light.layer.as_str()),
        ("light", "LIGHT", "E-LIGHT")
    );
    assert_eq!(light.point, json!({"type": "Point", "coordinates": [2.0, 2.5]}));
}

#[test]
fn every_entity_lands_in_linework_with_per_layer_counts() {
    let parsed = read(&two_room_dxf());
    let count = |layer: &str| parsed.layers.get(layer).cloned().unwrap_or_default();
    assert_eq!((count("A-ROOM").entities, count("A-ROOM").spaces), (2, 2));
    assert_eq!(count("A-TEXT").entities, 2);
    // The block's circle is drawn on layer 0, so it takes the INSERT's layer.
    assert_eq!((count("E-LIGHT").entities, count("E-LIGHT").objects), (1, 1));
    assert_eq!(parsed.linework.len(), 5);
    let texts: Vec<_> = parsed
        .linework
        .iter()
        .filter_map(|l| Some((l.text.as_deref()?, l.text_height?)))
        .collect();
    assert_eq!(texts, vec![("Kitchen", 0.25), ("Living room", 0.25)]);
    assert!(parsed.skipped.is_empty(), "{:?}", parsed.skipped);
}

#[test]
fn circle_in_a_block_is_placed_scaled_and_in_metres() {
    let parsed = read(&two_room_dxf());
    let circle = parsed
        .linework
        .iter()
        .find(|l| l.entity == "CIRCLE")
        .expect("the light's circle");
    let first = &circle.geometry["coordinates"][0];
    // Radius 150 mm at angle 0 from the insertion point (2000, 2500) mm.
    let (x, y) = (first[0].as_f64().expect("x"), first[1].as_f64().expect("y"));
    assert!((x - 2.15).abs() < 1e-9 && (y - 2.5).abs() < 1e-9, "({x}, {y})");
}

#[test]
fn unitless_drawing_fails_naming_the_header() {
    let mut drawing = drawing(Units::Unitless);
    drawing.add_entity(on_layer("A-ROOM", rectangle(0.0, 0.0, 10.0, 10.0)));
    let bytes = save(&drawing);
    let error = parse(&bytes, &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::NoUnits));
    assert!(error.to_string().contains("$INSUNITS"));
}

#[test]
fn stated_unit_replaces_the_header() {
    let mut drawing = drawing(Units::Unitless);
    drawing.add_entity(on_layer("A-ROOM", rectangle(0.0, 0.0, 10.0, 10.0)));
    let parsed = parse(&save(&drawing), &options(Some(LengthUnit::Feet), Transform::default())).expect("reads");
    assert!(parsed.unit_from_call);
    assert_eq!(parsed.spaces[0].polygon["coordinates"][0][2], json!([3.048, 3.048]));
}

#[test]
fn transform_scales_rotates_then_offsets() {
    let transform = Transform {
        offset_x: 100.0,
        offset_y: 50.0,
        rotation: 90.0,
        scale: 2.0,
    };
    let parsed = parse(&two_room_dxf(), &options(None, transform)).expect("reads");
    let point = &parsed.objects[0].point["coordinates"];
    // (2.0, 2.5) m doubled is (4, 5); turned 90 degrees is (-5, 4); offset is (95, 54).
    let (x, y) = (point[0].as_f64().expect("x"), point[1].as_f64().expect("y"));
    assert!((x - 95.0).abs() < 1e-9 && (y - 54.0).abs() < 1e-9, "({x}, {y})");
}

#[test]
fn self_inserting_block_fails_instead_of_recursing() {
    let mut drawing = drawing(Units::Meters);
    let mut block = Block {
        name: "LOOP".into(),
        ..Default::default()
    };
    block.entities.push(Entity::new(insert("LOOP")));
    drawing.add_block(block);
    drawing.add_entity(Entity::new(insert("LOOP")));
    let error = parse(&save(&drawing), &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(
        matches!(error, ImportError::SelfReference(ref name) if name == "LOOP"),
        "{error}"
    );
}

#[test]
fn blocks_inserting_each_other_fail_too() {
    let mut drawing = drawing(Units::Meters);
    for (name, inner) in [("A", "B"), ("B", "A")] {
        let mut block = Block {
            name: name.into(),
            ..Default::default()
        };
        block.entities.push(Entity::new(insert(inner)));
        drawing.add_block(block);
    }
    drawing.add_entity(Entity::new(insert("A")));
    let error = parse(&save(&drawing), &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::SelfReference(_)), "{error}");
}

#[test]
fn nesting_beyond_the_limit_fails() {
    let mut drawing = drawing(Units::Meters);
    let depth = MAX_BLOCK_DEPTH + 1;
    for level in 0..depth {
        let mut block = Block {
            name: format!("L{level}"),
            ..Default::default()
        };
        if level + 1 < depth {
            block.entities.push(Entity::new(insert(&format!("L{}", level + 1))));
        }
        drawing.add_block(block);
    }
    drawing.add_entity(Entity::new(insert("L0")));
    let error = parse(&save(&drawing), &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::TooDeep(_)), "{error}");
}

#[test]
fn nesting_at_the_limit_reads() {
    let mut drawing = drawing(Units::Meters);
    for level in 0..MAX_BLOCK_DEPTH {
        let mut block = Block {
            name: format!("L{level}"),
            ..Default::default()
        };
        if level + 1 < MAX_BLOCK_DEPTH {
            block.entities.push(Entity::new(insert(&format!("L{}", level + 1))));
        } else {
            block
                .entities
                .push(Entity::new(EntityType::Circle(Circle::new(Point::origin(), 1.0))));
        }
        drawing.add_block(block);
    }
    drawing.add_entity(Entity::new(insert("L0")));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    assert_eq!(parsed.linework.len(), 1);
}

#[test]
fn repeated_blocks_stop_at_the_entity_limit() {
    let mut drawing = drawing(Units::Meters);
    let mut inner = Block {
        name: "MANY".into(),
        ..Default::default()
    };
    for _ in 0..1000 {
        inner.entities.push(Entity::new(EntityType::Line(Line::new(
            Point::origin(),
            Point::new(1.0, 0.0, 0.0),
        ))));
    }
    drawing.add_block(inner);
    let mut outer = Block {
        name: "TIMES".into(),
        ..Default::default()
    };
    for _ in 0..1000 {
        outer.entities.push(Entity::new(insert("MANY")));
    }
    drawing.add_block(outer);
    drawing.add_entity(Entity::new(insert("TIMES")));
    let error = parse(&save(&drawing), &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::TooManyEntities), "{error}");
}

#[test]
fn open_outline_and_unread_entities_are_reported() {
    let mut drawing = drawing(Units::Meters);
    let mut open = LwPolyline::default();
    for (x, y) in [(0.0, 0.0), (5.0, 0.0), (5.0, 5.0)] {
        open.vertices.push(dxf::LwPolylineVertex {
            x,
            y,
            ..Default::default()
        });
    }
    drawing.add_entity(on_layer("A-ROOM", EntityType::LwPolyline(open)));
    drawing.add_entity(on_layer("A-ROOM", EntityType::Ellipse(Ellipse::default())));
    let mut on_paper = on_layer("A-ROOM", rectangle(0.0, 0.0, 1.0, 1.0));
    on_paper.common.is_in_paper_space = true;
    drawing.add_entity(on_paper);
    let mesh = Polyline {
        flags: 16,
        ..Default::default()
    };
    drawing.add_entity(on_layer("A-ROOM", EntityType::Polyline(mesh)));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    assert!(parsed.spaces.is_empty());
    // The open outline is the only linework; the rest were skipped.
    assert_eq!(parsed.linework.len(), 1);
    let reasons: Vec<_> = parsed
        .skipped
        .keys()
        .map(|s| (s.entity.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            ("ELLIPSE", "entity type not read"),
            ("LWPOLYLINE", "in paper space, not model space"),
            ("LWPOLYLINE", "open polyline on a space layer"),
            ("POLYLINE", "mesh, not a plan outline"),
        ]
    );
}

#[test]
fn garbage_is_not_a_drawing() {
    let error = parse(b"this is not a drawing", &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::Unreadable(_)), "{error}");
}

fn insert(name: &str) -> EntityType {
    EntityType::Insert(Insert {
        name: name.into(),
        ..Default::default()
    })
}

/// A 4 by 3 metre room whose east wall bows out as a half circle: the
/// vertex at (4, 0) carries a bulge of 1, so the arc to (4, 3) runs
/// counterclockwise round the centre (4, 1.5) through (5.5, 1.5).
const BULGED_ROOM: [(f64, f64, f64); 4] = [(0.0, 0.0, 0.0), (4.0, 0.0, 1.0), (4.0, 3.0, 0.0), (0.0, 3.0, 0.0)];

fn assert_bulged_room(parsed: &Parsed) {
    assert_eq!(parsed.spaces.len(), 1);
    let ring = parsed.spaces[0].polygon["coordinates"][0].as_array().expect("ring");
    assert!(ring.len() > 5, "the bulge adds points: {ring:?}");
    let points: Vec<(f64, f64)> = ring
        .iter()
        .map(|p| (p[0].as_f64().expect("x"), p[1].as_f64().expect("y")))
        .collect();
    assert!(
        points
            .iter()
            .any(|&(x, y)| (x - 5.5).abs() < 1e-9 && (y - 1.5).abs() < 1e-9),
        "{points:?}"
    );
    assert!(points.iter().all(|&(x, _)| x <= 5.5 + 1e-9));
}

#[test]
fn lwpolyline_bulge_becomes_an_arc_in_the_outline() {
    let mut drawing = drawing(Units::Meters);
    let mut room = LwPolyline::default();
    for (x, y, bulge) in BULGED_ROOM {
        room.vertices.push(dxf::LwPolylineVertex {
            x,
            y,
            bulge,
            ..Default::default()
        });
    }
    room.set_is_closed(true);
    drawing.add_entity(on_layer("A-ROOM", EntityType::LwPolyline(room)));
    assert_bulged_room(&parse(&save(&drawing), &options(None, Transform::default())).expect("reads"));
}

#[test]
fn old_polyline_bulge_becomes_an_arc_in_the_outline() {
    let mut drawing = drawing(Units::Meters);
    let mut room = Polyline::default();
    for (x, y, bulge) in BULGED_ROOM {
        let mut vertex = Vertex::new(Point::new(x, y, 0.0));
        vertex.bulge = bulge;
        room.add_vertex(&mut drawing, vertex);
    }
    room.set_is_closed(true);
    drawing.add_entity(on_layer("A-ROOM", EntityType::Polyline(room)));
    assert_bulged_room(&parse(&save(&drawing), &options(None, Transform::default())).expect("reads"));
}

fn coordinates(geometry: &serde_json::Value) -> Vec<(f64, f64)> {
    geometry["coordinates"]
        .as_array()
        .expect("coordinates")
        .iter()
        .map(|p| (p[0].as_f64().expect("x"), p[1].as_f64().expect("y")))
        .collect()
}

fn near(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
}

#[test]
fn line_and_wrapping_arc_are_drawn_where_they_are() {
    let mut drawing = drawing(Units::Meters);
    drawing.add_entity(on_layer(
        "A-WALL",
        EntityType::Line(Line::new(Point::new(1.0, 2.0, 0.0), Point::new(3.0, 4.0, 0.0))),
    ));
    // From 270 to 90 degrees counterclockwise passes through 0, on the
    // right of the centre.
    drawing.add_entity(on_layer(
        "A-WALL",
        EntityType::Arc(Arc::new(Point::origin(), 1.0, 270.0, 90.0)),
    ));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    let line = parsed.linework.iter().find(|l| l.entity == "LINE").expect("line");
    assert_eq!(
        line.geometry,
        json!({"type": "LineString", "coordinates": [[1.0, 2.0], [3.0, 4.0]]})
    );
    let arc = coordinates(
        &parsed
            .linework
            .iter()
            .find(|l| l.entity == "ARC")
            .expect("arc")
            .geometry,
    );
    assert!(near(arc[0], (0.0, -1.0)), "{arc:?}");
    assert!(near(arc[arc.len() - 1], (0.0, 1.0)), "{arc:?}");
    assert!(arc.iter().any(|&p| near(p, (1.0, 0.0))), "{arc:?}");
    assert!(arc.iter().all(|&(x, _)| x > -1e-9), "{arc:?}");
}

#[test]
fn arc_with_a_downward_normal_is_mirrored() {
    // A normal of -Z flips the entity's own x axis: the arc drawn from 0 to
    // 90 degrees round (2, 0) lands round (-2, 0), from (-3, 0) to (-2, 1).
    let mut drawing = drawing(Units::Meters);
    let mut arc = Arc::new(Point::new(2.0, 0.0, 0.0), 1.0, 0.0, 90.0);
    arc.normal = dxf::Vector::new(0.0, 0.0, -1.0);
    drawing.add_entity(on_layer("A-WALL", EntityType::Arc(arc)));
    let mut tilted = Arc::new(Point::origin(), 1.0, 0.0, 90.0);
    tilted.normal = dxf::Vector::new(1.0, 0.0, 0.0);
    drawing.add_entity(on_layer("A-WALL", EntityType::Arc(tilted)));
    // Tilted but still facing up: not a plan entity either.
    let mut leaning = Arc::new(Point::origin(), 1.0, 0.0, 90.0);
    leaning.normal = dxf::Vector::new(0.6, 0.0, 0.8);
    drawing.add_entity(on_layer("A-WALL", EntityType::Arc(leaning)));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    assert_eq!(parsed.linework.len(), 1);
    let arc = coordinates(&parsed.linework[0].geometry);
    assert!(near(arc[0], (-3.0, 0.0)), "{arc:?}");
    assert!(near(arc[arc.len() - 1], (-2.0, 1.0)), "{arc:?}");
    let skips: Vec<_> = parsed.skipped.iter().map(|(s, n)| (s.reason.as_str(), *n)).collect();
    assert_eq!(skips, [("extrusion is not along the z axis", 2)]);
}

#[test]
fn arc_with_absurd_angles_is_drawn_in_one_turn() {
    // Angles far past one turn are read modulo 360, so the sweep stays
    // bounded: 720 + 0 to 1e12 degrees is 0 to 280 here.
    let mut drawing = drawing(Units::Meters);
    drawing.add_entity(on_layer(
        "A-WALL",
        EntityType::Arc(Arc::new(Point::origin(), 1.0, 720.0, 1e12)),
    ));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    let arc = coordinates(&parsed.linework[0].geometry);
    // 0 to 280 degrees in 5 degree steps.
    assert_eq!(arc.len(), 57);
    assert!(near(arc[0], (1.0, 0.0)), "{arc:?}");
    assert!(arc.iter().any(|&p| near(p, (0.0, 1.0))), "{arc:?}");
    let end = 280_f64.to_radians();
    assert!(near(arc[56], (end.cos(), end.sin())), "{arc:?}");
}

#[test]
fn repeated_long_polylines_stop_at_the_vertex_limit() {
    // 300 copies of a 10,000 vertex polyline is 600 entities but three
    // million points.
    let mut drawing = drawing(Units::Meters);
    let mut long = LwPolyline::default();
    for i in 0..10_000 {
        long.vertices.push(dxf::LwPolylineVertex {
            x: f64::from(i),
            y: 0.0,
            ..Default::default()
        });
    }
    let mut block = Block {
        name: "LONG".into(),
        ..Default::default()
    };
    block.entities.push(Entity::new(EntityType::LwPolyline(long)));
    drawing.add_block(block);
    for _ in 0..300 {
        drawing.add_entity(Entity::new(insert("LONG")));
    }
    let error = parse(&save(&drawing), &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::TooManyVertices), "{error}");
}

#[test]
fn polyline_whose_arcs_pass_the_limit_is_skipped_and_reported() {
    // 300 half circles draw about 36 points each, well past the vertex limit,
    // while the drawing as a whole stays far below its own.
    let mut drawing = drawing(Units::Meters);
    let mut bulged = LwPolyline::default();
    for i in 0..300 {
        bulged.vertices.push(dxf::LwPolylineVertex {
            x: f64::from(i),
            y: 0.0,
            bulge: 1.0,
            ..Default::default()
        });
    }
    drawing.add_entity(on_layer("A-WALL", EntityType::LwPolyline(bulged)));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    assert!(parsed.linework.is_empty());
    let skipped: Vec<_> = parsed
        .skipped
        .iter()
        .map(|(s, count)| (s.entity.as_str(), s.reason.as_str(), *count))
        .collect();
    assert_eq!(skipped, vec![("LWPOLYLINE", "more vertices than the limit", 1)]);
}

#[test]
fn mirrored_arc_in_a_rotated_block_is_mirrored_before_it_is_placed() {
    // In the block, the -Z arc round (2, 0) is the arc round (-2, 0) from
    // (-3, 0) to (-2, 1). The insertion turns that 90 degrees and moves it to
    // (10, 5), so it runs from (10, 2) to (9, 3).
    let mut drawing = drawing(Units::Meters);
    let mut arc = Arc::new(Point::new(2.0, 0.0, 0.0), 1.0, 0.0, 90.0);
    arc.normal = dxf::Vector::new(0.0, 0.0, -1.0);
    let mut block = Block {
        name: "MIRRORED".into(),
        ..Default::default()
    };
    block.entities.push(Entity::new(EntityType::Arc(arc)));
    drawing.add_block(block);
    drawing.add_entity(Entity::new(EntityType::Insert(Insert {
        name: "MIRRORED".into(),
        location: Point::new(10.0, 5.0, 0.0),
        rotation: 90.0,
        ..Default::default()
    })));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    let arc = coordinates(&parsed.linework[0].geometry);
    assert!(near(arc[0], (10.0, 2.0)), "{arc:?}");
    assert!(near(arc[arc.len() - 1], (9.0, 3.0)), "{arc:?}");
}

#[test]
fn downward_polylines_and_circles_are_mirrored() {
    let mut drawing = drawing(Units::Meters);
    let down = dxf::Vector::new(0.0, 0.0, -1.0);
    let corners = [(1.0, 0.0), (2.0, 0.0), (2.0, 1.0), (1.0, 1.0)];
    let mut light = LwPolyline::default();
    for (x, y) in corners {
        light.vertices.push(dxf::LwPolylineVertex {
            x,
            y,
            ..Default::default()
        });
    }
    light.set_is_closed(true);
    light.extrusion_direction = down.clone();
    drawing.add_entity(on_layer("A-ROOM", EntityType::LwPolyline(light)));
    let mut old = Polyline::default();
    for (x, y) in corners {
        old.add_vertex(&mut drawing, Vertex::new(Point::new(x, y, 0.0)));
    }
    old.set_is_closed(true);
    old.normal = down.clone();
    drawing.add_entity(on_layer("A-ROOM", EntityType::Polyline(old)));
    let mut circle = Circle::new(Point::new(2.0, 0.0, 0.0), 1.0);
    circle.normal = down;
    drawing.add_entity(on_layer("A-WALL", EntityType::Circle(circle)));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    assert_eq!(parsed.spaces.len(), 2);
    for space in &parsed.spaces {
        let ring = coordinates(&json!({"coordinates": space.polygon["coordinates"][0]}));
        assert!(
            ring.iter().all(|&(x, _)| (-2.0 - 1e-9..=-1.0 + 1e-9).contains(&x)),
            "{ring:?}"
        );
    }
    let circle = coordinates(
        &parsed
            .linework
            .iter()
            .find(|l| l.entity == "CIRCLE")
            .expect("circle")
            .geometry,
    );
    assert!(near(circle[0], (-3.0, 0.0)), "{circle:?}");
    assert!(circle.iter().all(|&(x, y)| ((x + 2.0).hypot(y) - 1.0).abs() < 1e-9));
}

#[test]
fn downward_text_and_insert_are_mirrored_and_tilted_ones_skipped() {
    let mut drawing = drawing(Units::Meters);
    let down = dxf::Vector::new(0.0, 0.0, -1.0);
    let mut label = dxf::entities::Text {
        location: Point::new(5.0, 1.0, 0.0),
        text_height: 0.2,
        value: "Hall".into(),
        ..Default::default()
    };
    label.normal = down.clone();
    drawing.add_entity(on_layer("A-TEXT", EntityType::Text(label)));
    let mut light = Block {
        name: "LIGHT".into(),
        ..Default::default()
    };
    light.entities.push(Entity::new(EntityType::Line(Line::new(
        Point::new(0.0, 0.0, 0.0),
        Point::new(1.0, 0.0, 0.0),
    ))));
    drawing.add_block(light);
    for (x, normal) in [(5.0, down), (7.0, dxf::Vector::new(0.6, 0.0, 0.8))] {
        drawing.add_entity(on_layer(
            "E-LIGHT",
            EntityType::Insert(Insert {
                name: "LIGHT".into(),
                location: Point::new(x, 2.0, 0.0),
                extrusion_direction: normal,
                ..Default::default()
            }),
        ));
    }
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    let text = parsed.linework.iter().find(|l| l.entity == "TEXT").expect("text");
    assert_eq!(text.geometry["coordinates"], json!([-5.0, 1.0]));
    assert_eq!(parsed.objects.len(), 1);
    assert_eq!(parsed.objects[0].point["coordinates"], json!([-5.0, 2.0]));
    // The block's contents are mirrored with it, not just its insertion point.
    let line = parsed
        .linework
        .iter()
        .find(|l| l.entity == "LINE")
        .expect("the block's line");
    assert_eq!(line.geometry["coordinates"], json!([[-5.0, 2.0], [-6.0, 2.0]]));
    let skips: Vec<_> = parsed
        .skipped
        .iter()
        .map(|(s, n)| (s.entity.as_str(), s.reason.as_str(), *n))
        .collect();
    assert_eq!(skips, [("INSERT", "extrusion is not along the z axis", 1)]);
}

#[test]
fn oversized_bulged_polylines_count_toward_the_vertex_limit() {
    // Each copy is skipped for having too many vertices once its arcs are
    // drawn. Without counting the skipped ones the reader would expand all
    // 1,000 copies and still accept the drawing.
    let mut drawing = drawing(Units::Meters);
    let mut bulged = LwPolyline::default();
    for i in 0..9_000 {
        bulged.vertices.push(dxf::LwPolylineVertex {
            x: f64::from(i),
            y: 0.0,
            bulge: 1.0,
            ..Default::default()
        });
    }
    let mut block = Block {
        name: "BULGED".into(),
        ..Default::default()
    };
    block.entities.push(Entity::new(EntityType::LwPolyline(bulged)));
    drawing.add_block(block);
    for _ in 0..1_000 {
        drawing.add_entity(Entity::new(insert("BULGED")));
    }
    let error = parse(&save(&drawing), &options(None, Transform::default()))
        .err()
        .expect("refused");
    assert!(matches!(error, ImportError::TooManyVertices), "{error}");
}

#[test]
fn nul_in_text_is_dropped() {
    let mut drawing = drawing(Units::Meters);
    drawing.add_entity(on_layer(
        "A-TEXT",
        EntityType::Text(dxf::entities::Text {
            location: Point::new(1.0, 1.0, 0.0),
            text_height: 0.2,
            value: "Ki\0tchen".into(),
            ..Default::default()
        }),
    ));
    let parsed = parse(&save(&drawing), &options(None, Transform::default())).expect("reads");
    let text = parsed.linework.iter().find(|l| l.entity == "TEXT").expect("text");
    assert_eq!(text.text.as_deref(), Some("Kitchen"));
}

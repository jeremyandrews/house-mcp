//! The levels below an object (details) and above a site (areas), over MCP:
//! that they never reach a floor or site answer, that they nest, refuse
//! cycles and hide with what they are inside, and how they are drawn.
//! Every name, shape and place here is made up.

mod common;

use std::io::Cursor;

use base64::{Engine, engine::general_purpose::STANDARD};
use common::{
    drawing,
    mcp::{RO, RW, call, client, ok, start},
    on_layer, rectangle, save,
};
use dxf::{Point, entities::EntityType, entities::Text, enums::Units};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use rmcp::{RoleClient, model::CallToolResult, service::RunningService};
use serde_json::{Value, json};
use sqlx::PgPool;

type Client = RunningService<RoleClient, ()>;

fn ring(x: f64, y: f64, w: f64, h: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[x, y], [x + w, y], [x + w, y + h], [x, y + h], [x, y]]]})
}

fn point(x: f64, y: f64) -> Value {
    json!({"type": "Point", "coordinates": [x, y]})
}

/// Calls a write tool with a reason and returns the id it made.
async fn make(rw: &Client, tool: &'static str, mut args: Value) -> i64 {
    args["reason"] = json!("test");
    ok(rw, tool, args).await["id"].as_i64().expect("an id")
}

/// Calls a tool, expecting it to be refused, and returns the message.
async fn refused(client: &Client, tool: &'static str, mut args: Value) -> String {
    if tool.starts_with("upsert") || ["delete", "restore", "set_site_area", "import_dxf"].contains(&tool) {
        args["reason"] = json!("test");
    }
    call(client, tool, args).await.expect_err("refused")
}

/// The image in a tool's reply, with its media type.
fn image_of(result: &CallToolResult) -> Option<(Vec<u8>, String)> {
    result.content.iter().find_map(|c| {
        c.as_image()
            .map(|i| (STANDARD.decode(&i.data).expect("base64"), i.mime_type.clone()))
    })
}

/// The JSON text block in a tool's reply.
fn json_of(result: &CallToolResult) -> Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .find(|t| !t.text.starts_with("<svg"))
        .expect("a JSON block");
    serde_json::from_str(&text.text).expect("JSON")
}

fn svg_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .find(|t| t.text.starts_with("<svg"))
        .map(|t| t.text.clone())
        .expect("an SVG")
}

async fn render(client: &Client, args: Value) -> CallToolResult {
    call(client, "render_plan", args).await.expect("render_plan")
}

async fn pool(server: &common::mcp::Server) -> PgPool {
    house_mcp::db::connect(&server.db.house_url())
        .await
        .expect("connect as house")
}

/// A georeferenced site at 9 E, 45 N with a parcel, a floor, a room, and a
/// table object in the room. Returns (site, floor, room, table).
async fn house(rw: &Client, name: &str) -> (i64, i64, i64, i64) {
    let site = make(
        rw,
        "create_site",
        json!({"name": name, "origin": {"epsg": 4326, "x": 9.0, "y": 45.0}}),
    )
    .await;
    make(
        rw,
        "upsert_space",
        json!({"site_id": site, "kind": "parcel", "name": "Plot", "geometry": ring(0.0, 0.0, 20.0, 20.0)}),
    )
    .await;
    let floor = make(
        rw,
        "upsert_floor",
        json!({"site_id": site, "name": "Ground", "level": 0}),
    )
    .await;
    let room = make(
        rw,
        "upsert_space",
        json!({"site_id": site, "floor_id": floor, "kind": "room", "name": "Dining room", "geometry": ring(2.0, 2.0, 6.0, 4.0)}),
    )
    .await;
    let table = make(
        rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": floor, "kind": "table", "name": "table", "geometry": point(5.0, 4.0)}),
    )
    .await;
    (site, floor, room, table)
}

/// Parts in a detail's millimetres: a top and two legs.
fn top_parts() -> [(&'static str, Value); 3] {
    [
        ("top", ring(0.0, 0.0, 2400.0, 600.0)),
        ("left leg", ring(50.0, 50.0, 80.0, 500.0)),
        ("right leg", ring(2270.0, 50.0, 80.0, 500.0)),
    ]
}

/// A drawing on layer PARTS: one outline labelled inside and one not, and
/// a line on another layer, in `units`. `scale` is how many drawing units
/// make the outline's 1 by 0.5 sides.
fn parts_dxf(units: Units, scale: f64) -> Vec<u8> {
    let mut drawing = drawing(units);
    drawing.add_entity(on_layer("PARTS", rectangle(0.0, 0.0, scale, 0.5 * scale)));
    drawing.add_entity(on_layer("PARTS", rectangle(2.0 * scale, 0.0, 3.0 * scale, 0.5 * scale)));
    drawing.add_entity(on_layer(
        "PARTS",
        EntityType::Text(Text {
            location: Point::new(0.4 * scale, 0.2 * scale, 0.0),
            text_height: 0.05 * scale,
            value: "front panel".into(),
            ..Default::default()
        }),
    ));
    drawing.add_entity(on_layer("EDGE", rectangle(0.0, 0.0, 3.0 * scale, scale)));
    save(&drawing)
}

/// The x and y range of every coordinate in a GeoJSON geometry.
fn bounds(geometry: &Value) -> (f64, f64, f64, f64) {
    fn walk(v: &Value, out: &mut Vec<(f64, f64)>) {
        match v.as_array() {
            Some(a) if a.len() == 2 && a.iter().all(Value::is_number) => {
                out.push((a[0].as_f64().expect("x"), a[1].as_f64().expect("y")));
            }
            Some(a) => a.iter().for_each(|item| walk(item, out)),
            None => {}
        }
    }
    let mut points = Vec::new();
    walk(&geometry["coordinates"], &mut points);
    points
        .iter()
        .fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |(x0, y0, x1, y1), &(x, y)| {
            (x0.min(x), y0.min(y), x1.max(x), y1.max(y))
        })
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[tokio::test]
async fn a_detail_never_changes_a_floor_or_site_answer() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor, room, table) = house(&rw, "Detail house").await;

    let floor_plan = json!({"site_id": site, "floor_id": floor});
    let site_plan = json!({"site_id": site});
    let at = json!({"site_id": site, "x": 5.0, "y": 4.0, "radius": 5.0});
    let before_floor = image_of(&render(&ro, floor_plan.clone()).await).expect("png").0;
    let before_site = image_of(&render(&ro, site_plan.clone()).await).expect("png").0;
    let before_get_floor = ok(&ro, "get_floor", json!({"floor_id": floor})).await;
    let before_at = ok(&ro, "what_is_at", at.clone()).await;

    // A detail with linework, parts and a child detail, all in millimetres
    // that would be far off the plan if they were read as metres.
    let top = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "top", "view": "top",
               "measurements": {"width": {"value": 2400, "source": "measured"}}}),
    )
    .await;
    for (name, geometry) in top_parts() {
        make(
            &rw,
            "upsert_detail_part",
            json!({"detail_id": top, "name": name, "geometry": geometry}),
        )
        .await;
    }
    let drawing = parts_dxf(Units::Millimeters, 1000.0);
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "top.dxf", "base64": STANDARD.encode(&drawing), "links": {"detail_id": top}, "reason": "test"}),
    )
    .await;
    let import = ok(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "detail_id": top, "layers": {"PARTS": {"part": "panel"}}, "reason": "test"}),
    )
    .await;
    assert_eq!(import["linework"], 4, "{import}");
    let drawer = make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "drawer", "view": "top", "placement": {"x": 800.0, "y": 100.0}}),
    )
    .await;
    make(
        &rw,
        "upsert_detail_part",
        json!({"detail_id": drawer, "name": "drawer front", "geometry": ring(0.0, 0.0, 600.0, 400.0)}),
    )
    .await;

    // The floor and site plans, get_floor and what_is_at are as they were.
    assert_eq!(
        image_of(&render(&ro, floor_plan).await).expect("png").0,
        before_floor,
        "the floor plan changed"
    );
    assert_eq!(
        image_of(&render(&ro, site_plan).await).expect("png").0,
        before_site,
        "the site plan changed"
    );
    assert_eq!(ok(&ro, "get_floor", json!({"floor_id": floor})).await, before_get_floor);
    let after_at = ok(&ro, "what_is_at", at).await;
    assert_eq!(after_at, before_at);
    let object_ids: Vec<&Value> = after_at["objects"]
        .as_array()
        .expect("objects")
        .iter()
        .map(|o| &o["id"])
        .collect();
    assert_eq!(object_ids, [&json!(table)]);
    let space_ids: Vec<&Value> = after_at["spaces"]
        .as_array()
        .expect("spaces")
        .iter()
        .map(|s| &s["id"])
        .collect();
    assert!(
        space_ids.contains(&&json!(room)) && space_ids.len() == 2,
        "the parcel and the room: {after_at}"
    );

    // nearest finds the table and nothing from its detail, and counts the
    // table's detail. The file is linked to the detail, not to the table, so
    // no file is counted.
    let nearest = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "table", "from": {"point": [5.0, 4.0]}}),
    )
    .await;
    assert_eq!(nearest["objects"].as_array().expect("objects").len(), 1);
    assert_eq!(nearest["objects"][0]["id"], table);
    assert_eq!(nearest["objects"][0]["details"], 1);
    assert!(nearest["objects"][0].get("files").is_none(), "{nearest}");
    let panels = ok(
        &ro,
        "nearest",
        json!({"site_id": site, "kind": "panel", "from": {"point": [5.0, 4.0]}}),
    )
    .await;
    assert_eq!(panels["objects"], json!([]));
    let found = ok(&ro, "find", json!({"name": "front panel"})).await;
    assert_eq!(found["objects"], json!([]), "a part is not an object");
    assert_eq!(found["spaces"], json!([]), "a part is not a space");
}

#[tokio::test]
async fn details_have_one_parent_no_cycle_and_one_site() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let (_, _, _, table) = house(&rw, "One parent").await;
    let (_, _, _, other_table) = house(&rw, "Another site").await;
    let top = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "top", "view": "top"}),
    )
    .await;
    let drawer = make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "drawer", "view": "front"}),
    )
    .await;

    // Exactly one parent, from the tool and in the table itself.
    let both = refused(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "parent_detail_id": top, "name": "x", "view": "top"}),
    )
    .await;
    assert!(both.contains("not both"), "{both}");
    let neither = refused(&rw, "upsert_detail", json!({"name": "x", "view": "top"})).await;
    assert!(neither.contains("needs a parent"), "{neither}");
    let pool = pool(&server).await;
    let site: i64 = sqlx::query_scalar("SELECT site_id FROM detail WHERE id = $1")
        .bind(top)
        .fetch_one(&pool)
        .await
        .expect("site");
    for (object, parent) in [(None, None), (Some(table), Some(top))] {
        let error = sqlx::query(
            "INSERT INTO detail (site_id, object_id, parent_detail_id, name, view) VALUES ($1, $2, $3, 'x', 'top')",
        )
        .bind(site)
        .bind(object)
        .bind(parent)
        .execute(&pool)
        .await
        .expect_err("one parent");
        assert_eq!(
            error.as_database_error().and_then(|e| e.constraint()),
            Some("detail_has_one_parent")
        );
    }

    // A detail cannot end up inside itself.
    let cycle = refused(&rw, "upsert_detail", json!({"id": top, "parent_detail_id": drawer})).await;
    assert!(cycle.contains("inside itself"), "{cycle}");
    let own = refused(&rw, "upsert_detail", json!({"id": top, "parent_detail_id": top})).await;
    assert!(own.contains("inside itself"), "{own}");

    // A child is on its parent's site: written directly with another site,
    // or by moving a detail that has children to another site's object.
    let other_site: i64 = sqlx::query_scalar("SELECT site_id FROM object WHERE id = $1")
        .bind(other_table)
        .fetch_one(&pool)
        .await
        .expect("site");
    let error = sqlx::query("INSERT INTO detail (site_id, parent_detail_id, name, view) VALUES ($1, $2, 'x', 'top')")
        .bind(other_site)
        .bind(top)
        .execute(&pool)
        .await
        .expect_err("another site");
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("detail_same_site")
    );
    let moved = refused(&rw, "upsert_detail", json!({"id": top, "object_id": other_table})).await;
    assert!(moved.contains("another site"), "{moved}");
    // A detail with no children moves, and takes its new object's site.
    ok(
        &rw,
        "upsert_detail",
        json!({"id": drawer, "object_id": other_table, "reason": "test"}),
    )
    .await;
    let moved_site: i64 = sqlx::query_scalar("SELECT site_id FROM detail WHERE id = $1")
        .bind(drawer)
        .fetch_one(&pool)
        .await
        .expect("site");
    assert_eq!(moved_site, other_site);
}

#[tokio::test]
async fn deleting_a_detail_hides_what_is_inside_it_until_restored() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (_, _, _, table) = house(&rw, "Hidden details").await;
    let top = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "top", "view": "top"}),
    )
    .await;
    make(
        &rw,
        "upsert_detail_part",
        json!({"detail_id": top, "name": "top", "geometry": ring(0.0, 0.0, 2400.0, 600.0)}),
    )
    .await;
    let drawer = make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "drawer", "view": "top", "placement": {"x": 800, "y": 100}}),
    )
    .await;
    make(
        &rw,
        "upsert_detail_part",
        json!({"detail_id": drawer, "name": "drawer front", "geometry": ring(0.0, 0.0, 600.0, 400.0)}),
    )
    .await;
    let drawn = svg_of(&render(&ro, json!({"detail_id": top, "include_svg": true})).await);
    assert!(
        drawn.contains(">Drawer<") || drawn.contains(">drawer<"),
        "the drawer is drawn in the top"
    );

    // The drawer alone: gone from its parent's reply and drawing.
    ok(&rw, "delete", json!({"kind": "detail", "id": drawer, "reason": "test"})).await;
    let read = ok(&ro, "get_detail", json!({"detail_id": top})).await;
    assert_eq!(read["details"], json!([]));
    let drawn = svg_of(&render(&ro, json!({"detail_id": top, "include_svg": true})).await);
    assert!(!drawn.contains("drawer"), "a deleted drawer is not drawn");
    ok(
        &rw,
        "restore",
        json!({"kind": "detail", "id": drawer, "reason": "test"}),
    )
    .await;

    // The top: the drawer inside it, though not deleted itself, is hidden.
    ok(&rw, "delete", json!({"kind": "detail", "id": top, "reason": "test"})).await;
    for tool in ["get_detail", "render_plan"] {
        let error = refused(&ro, tool, json!({"detail_id": drawer})).await;
        assert!(error.contains("no live detail"), "{tool}: {error}");
    }
    let parts = refused(&ro, "context", json!({"detail_id": drawer})).await;
    assert!(parts.contains("no live detail"), "{parts}");
    ok(&rw, "restore", json!({"kind": "detail", "id": top, "reason": "test"})).await;
    let read = ok(&ro, "get_detail", json!({"detail_id": drawer})).await;
    assert_eq!(read["parts"][0]["name"], "drawer front");
    assert!(render(&ro, json!({"detail_id": drawer})).await.content.len() >= 2);

    // Deleting the table hides its details, and a part can be deleted too.
    ok(&rw, "delete", json!({"kind": "object", "id": table, "reason": "test"})).await;
    let error = refused(&ro, "get_detail", json!({"detail_id": top})).await;
    assert!(error.contains("no live detail"), "{error}");
    ok(&rw, "restore", json!({"kind": "object", "id": table, "reason": "test"})).await;
    let part = ok(&ro, "get_detail", json!({"detail_id": drawer})).await["parts"][0]["id"].clone();
    ok(
        &rw,
        "delete",
        json!({"kind": "detail_part", "id": part, "reason": "test"}),
    )
    .await;
    assert_eq!(
        ok(&ro, "get_detail", json!({"detail_id": drawer})).await["parts"],
        json!([])
    );
    ok(
        &rw,
        "restore",
        json!({"kind": "detail_part", "id": part, "reason": "test"}),
    )
    .await;
    assert_eq!(
        ok(&ro, "get_detail", json!({"detail_id": drawer})).await["parts"][0]["id"],
        part
    );
}

#[tokio::test]
async fn an_object_removed_before_placement_comes_back_placed_by_its_position() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor, _, _) = house(&rw, "Removed outright").await;
    let pool = pool(&server).await;
    let mut qgis = server.db.qgis().await;
    for (name, attributes, placement) in [
        ("stacked chair", json!({"position": "site_unplaced"}), "unplaced"),
        ("lamp in the middle", json!({"position": "room_centre"}), "approximate"),
        ("surveyed lamp", json!({}), "exact"),
    ] {
        let id = make(
            &rw,
            "upsert_object",
            json!({"site_id": site, "floor_id": floor, "kind": "thing", "name": name,
                   "geometry": point(3.0, 3.0), "attributes": attributes, "placement": placement}),
        )
        .await;
        sqlx::query("DELETE FROM object WHERE id = $1")
            .bind(id)
            .execute(&mut qgis)
            .await
            .expect("removed outright");
        // As if the row had been recorded before migration 0004.
        let changed = sqlx::query(
            "UPDATE change SET before = before - 'placement'
             WHERE table_name = 'object' AND row_id = $1 AND operation = 'DELETE'",
        )
        .bind(id)
        .execute(&pool)
        .await
        .expect("history");
        assert_eq!(changed.rows_affected(), 1);
        let restored = ok(&rw, "restore", json!({"kind": "object", "id": id, "reason": "test"})).await;
        assert_eq!(restored["how"], "reinserted from history");
        let found = ok(&ro, "find", json!({"site_id": site, "name": name})).await;
        assert_eq!(found["objects"][0]["placement"], placement, "{name}");
    }
}

#[tokio::test]
async fn a_drawing_imported_onto_a_detail_lands_in_millimetres() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (_, _, _, table) = house(&rw, "Units").await;
    // A 1 by 0.5 outline is 1000 by 500 mm drawn in metres, and 25.4 by
    // 12.7 mm drawn in inches.
    for (units, unit, side) in [(Units::Meters, "metres", 1000.0), (Units::Inches, "inches", 25.4)] {
        let detail = make(
            &rw,
            "upsert_detail",
            json!({"object_id": table, "name": format!("in {unit}"), "view": "front"}),
        )
        .await;
        let drawing = parts_dxf(units, 1.0);
        let file = ok(
            &rw,
            "upload_file",
            json!({"name": format!("{unit}.dxf"), "base64": STANDARD.encode(&drawing), "reason": "test"}),
        )
        .await;
        let import = ok(
            &rw,
            "import_dxf",
            json!({"file_sha256": file["sha256"], "detail_id": detail, "layers": {"parts": {"part": "panel"}}, "reason": "test"}),
        )
        .await;
        assert_eq!(import["unit"], unit);
        assert_eq!(import["frame"], "millimetres");
        let mut names: Vec<&str> = import["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .map(|p| p["name"].as_str().expect("name"))
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["front panel", "panel 1"], "{unit}");
        let read = ok(&ro, "get_detail", json!({"detail_id": detail})).await;
        assert_eq!(read["linework"], 4);
        let front = read["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .find(|p| p["name"] == "front panel")
            .expect("the labelled part");
        let (x0, y0, x1, y1) = bounds(&front["geometry"]);
        assert!(
            near(x0, 0.0) && near(y0, 0.0) && near(x1, side) && near(y1, side / 2.0),
            "{unit}: {:?}",
            (x0, y0, x1, y1)
        );
        // A second import of the same drawing replaces the first.
        let again = ok(
            &rw,
            "import_dxf",
            json!({"file_sha256": file["sha256"], "detail_id": detail, "layers": {"PARTS": {"part": "panel"}}, "reason": "test"}),
        )
        .await;
        assert_eq!(again["replaced_import_id"], import["import_id"]);
        let read = ok(&ro, "get_detail", json!({"detail_id": detail})).await;
        assert_eq!(
            (read["linework"].clone(), read["parts"].as_array().map(Vec::len)),
            (json!(4), Some(2))
        );
    }
    // A detail takes only part layers, and a floor no part layers.
    let detail = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "x", "view": "top"}),
    )
    .await;
    let file = ok(
        &rw,
        "upload_file",
        json!({"name": "x.dxf", "base64": STANDARD.encode(parts_dxf(Units::Millimeters, 1.0)), "reason": "test"}),
    )
    .await;
    let error = refused(
        &rw,
        "import_dxf",
        json!({"file_sha256": file["sha256"], "detail_id": detail, "layers": {"PARTS": {"space": "room"}}}),
    )
    .await;
    assert!(error.contains("only part layers"), "{error}");
}

/// A PDF of two pages, each a filled square: red on the left of page 1 and
/// blue on the right of page 2, drawn as vector paths.
fn two_page_pdf() -> Vec<u8> {
    use pdf_writer::{Content, Pdf, Rect, Ref};
    let mut pdf = Pdf::new();
    let (catalog, tree) = (Ref::new(1), Ref::new(2));
    let pages = [(Ref::new(3), Ref::new(4)), (Ref::new(5), Ref::new(6))];
    pdf.catalog(catalog).pages(tree);
    pdf.pages(tree).kids(pages.iter().map(|(page, _)| *page)).count(2);
    for (i, (page, contents)) in pages.into_iter().enumerate() {
        let mut writer = pdf.page(page);
        writer.media_box(Rect::new(0.0, 0.0, 800.0, 400.0));
        writer.parent(tree);
        writer.contents(contents);
        drop(writer);
        let mut content = Content::new();
        if i == 0 {
            content.set_fill_rgb(1.0, 0.0, 0.0);
            content.rect(40.0, 40.0, 320.0, 320.0);
        } else {
            content.set_fill_rgb(0.0, 0.0, 1.0);
            content.rect(440.0, 40.0, 320.0, 320.0);
        }
        content.fill_nonzero();
        pdf.stream(contents, &content.finish());
    }
    pdf.finish()
}

fn pixel(png: &[u8], fx: f64, fy: f64) -> [u8; 3] {
    let image = image::load_from_memory_with_format(png, ImageFormat::Png)
        .expect("a PNG")
        .to_rgb8();
    let (w, h) = image.dimensions();
    let x = ((f64::from(w) * fx) as u32).min(w - 1);
    let y = ((f64::from(h) * fy) as u32).min(h - 1);
    image.get_pixel(x, y).0
}

#[tokio::test]
async fn get_file_shows_images_and_pdf_pages() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;

    let mut png = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 32, Rgb([0, 128, 0])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .expect("png");
    let stored = ok(
        &rw,
        "upload_file",
        json!({"name": "swatch.png", "base64": STANDARD.encode(&png), "reason": "test"}),
    )
    .await;
    let shown = call(&ro, "get_file", json!({"sha256": stored["sha256"]}))
        .await
        .expect("get_file");
    let (bytes, media_type) = image_of(&shown).expect("an image");
    assert_eq!((bytes, media_type.as_str()), (png.clone(), "image/png"));
    let reply = json_of(&shown);
    assert_eq!(reply["content_kind"], "image");
    assert_eq!(
        (reply["width"].clone(), reply["height"].clone()),
        (json!(64), json!(32))
    );

    let pdf = two_page_pdf();
    let stored = ok(
        &rw,
        "upload_file",
        json!({"name": "two pages.pdf", "base64": STANDARD.encode(&pdf), "reason": "test"}),
    )
    .await;
    assert_eq!(stored["media_type"], "application/pdf");
    for (page, colour, fx) in [(1, [255, 0, 0], 0.25), (2, [0, 0, 255], 0.75)] {
        let shown = call(&ro, "get_file", json!({"sha256": stored["sha256"], "page": page}))
            .await
            .expect("get_file");
        let reply = json_of(&shown);
        assert_eq!(reply["content_kind"], "pdf_page");
        assert_eq!(
            (reply["page"].clone(), reply["page_count"].clone()),
            (json!(page), json!(2))
        );
        let (bytes, media_type) = image_of(&shown).expect("an image");
        assert_eq!(media_type, "image/png");
        assert_eq!(reply["width"], 2000, "the long side is 2000 px");
        assert_eq!(pixel(&bytes, fx, 0.5), colour, "page {page}'s square");
        assert_eq!(
            pixel(&bytes, 1.0 - fx, 0.5),
            [255, 255, 255],
            "page {page}'s other half"
        );
    }
    let error = refused(&ro, "get_file", json!({"sha256": stored["sha256"], "page": 3})).await;
    assert!(error.contains("page") && error.contains("2 pages"), "{error}");

    let blob = ok(
        &rw,
        "upload_file",
        json!({"name": "blob.bin", "base64": STANDARD.encode([0u8, 159, 146, 150, 0, 1]), "reason": "test"}),
    )
    .await;
    let shown = call(&ro, "get_file", json!({"sha256": blob["sha256"]}))
        .await
        .expect("get_file");
    assert!(image_of(&shown).is_none());
    let reply = json_of(&shown);
    assert_eq!(
        (reply["content_kind"].clone(), reply["content"].clone()),
        (json!("none"), Value::Null)
    );
    let error = refused(&ro, "get_file", json!({"sha256": blob["sha256"], "page": 1})).await;
    assert!(error.contains("only a PDF"), "{error}");

    let text = ok(
        &rw,
        "upload_file",
        json!({"name": "note.txt", "text": "oil the top yearly", "reason": "test"}),
    )
    .await;
    let reply = ok(&ro, "get_file", json!({"sha256": text["sha256"]})).await;
    assert_eq!(
        (reply["content_kind"].clone(), reply["content"].clone()),
        (json!("text"), json!("oil the top yearly"))
    );
}

#[tokio::test]
async fn find_marks_an_object_with_details_or_files() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor, room, table) = house(&rw, "Counts").await;
    let chair = make(
        &rw,
        "upsert_object",
        json!({"site_id": site, "floor_id": floor, "kind": "chair", "name": "chair", "geometry": point(4.0, 3.0)}),
    )
    .await;
    let found = ok(&ro, "find", json!({"site_id": site, "name": "table"})).await;
    let hit = &found["objects"][0];
    assert!(hit.get("details").is_none() && hit.get("files").is_none(), "{hit}");

    let top = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "top", "view": "top"}),
    )
    .await;
    // A detail inside the top is not counted on the table: only those drawn
    // directly of it are.
    make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "drawer", "view": "top"}),
    )
    .await;
    for name in ["quote.txt", "invoice.txt"] {
        ok(
            &rw,
            "upload_file",
            json!({"name": name, "text": name, "links": {"object_id": table}, "reason": "test"}),
        )
        .await;
    }
    let found = ok(&ro, "find", json!({"site_id": site, "name": "table"})).await;
    let hit = &found["objects"][0];
    assert_eq!(
        (hit["details"].clone(), hit["files"].clone()),
        (json!(1), json!(2)),
        "{hit}"
    );
    let full = ok(&ro, "find", json!({"site_id": site, "name": "table", "detail": true})).await;
    assert_eq!(
        full["objects"][0]["details"],
        json!([{"id": top, "name": "top", "view": "top"}])
    );
    assert_eq!(full["objects"][0]["files"], 2);
    let chair_hit = ok(&ro, "find", json!({"site_id": site, "name": "chair"})).await;
    assert!(chair_hit["objects"][0].get("details").is_none());
    let inside = ok(&ro, "in_space", json!({"space_id": room})).await;
    for object in inside["objects"].as_array().expect("objects") {
        if object["id"] == table {
            assert_eq!(object["details"], 1);
        } else {
            assert_eq!(object["id"], chair);
            assert!(object.get("details").is_none() && object.get("files").is_none());
        }
    }
}

#[tokio::test]
async fn a_detail_is_drawn_with_its_parts_dimensions_and_placed_children() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (_, _, _, table) = house(&rw, "Drawn details").await;
    let top = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "top", "view": "top",
               "measurements": {"width": {"value": 2400, "source": "measured"},
                                "depth": {"value": 600, "source": "estimated"},
                                "height": {"value": 720, "source": "estimated"}}}),
    )
    .await;
    for (name, geometry) in top_parts() {
        make(
            &rw,
            "upsert_detail_part",
            json!({"detail_id": top, "name": name, "geometry": geometry}),
        )
        .await;
    }
    let drawer = make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "drawer", "view": "top", "placement": {"x": 800, "y": 100}}),
    )
    .await;
    make(
        &rw,
        "upsert_detail_part",
        json!({"detail_id": drawer, "name": "drawer front", "geometry": ring(0.0, 0.0, 600.0, 400.0)}),
    )
    .await;
    let loose = make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "spare shelf", "view": "top"}),
    )
    .await;
    make(
        &rw,
        "upsert_detail_part",
        json!({"detail_id": loose, "name": "shelf", "geometry": ring(0.0, 0.0, 500.0, 300.0)}),
    )
    .await;

    let drawn = render(&ro, json!({"detail_id": top, "include_svg": true})).await;
    let svg = svg_of(&drawn);
    let summary = json_of(&drawn);
    assert!(svg.contains("class=\"dimension\""), "dimension lines");
    assert!(svg.contains(">width 2400 mm<"), "the measured width");
    assert!(svg.contains(">depth ~600 mm<"), "the estimated depth is marked");
    assert_eq!(summary["dimensions"], json!(["width 2400 mm", "depth ~600 mm"]));
    for label in [">Top<", ">Left leg<", ">Right leg<", ">Drawer<"] {
        assert!(
            svg.contains(label) || svg.contains(&label.to_lowercase()),
            "{label} is labelled"
        );
    }
    assert!(
        svg.contains("stroke-dasharray=\"6 4\""),
        "the drawer is outlined in place"
    );
    assert!(
        !svg.to_lowercase().contains("spare shelf"),
        "an unplaced child is not drawn"
    );
    assert!(
        summary["notes"].to_string().contains("1 detail inside, not placed"),
        "{summary}"
    );
    assert!(!svg.contains("M 0 -22 L 8 8"), "no north arrow on a detail");
    assert!(
        svg.contains(" cm</text>") || svg.contains(" mm</text>"),
        "a scale bar in cm or mm"
    );

    // The unplaced child is reported as such.
    let context = ok(&ro, "context", json!({"detail_id": loose})).await;
    assert_eq!(context["up"][0]["placed"], false);
    assert_eq!(
        ok(&ro, "context", json!({"detail_id": drawer})).await["up"][0]["placed"],
        true
    );
    let parent = ok(&ro, "context", json!({"detail_id": top})).await;
    assert_eq!(parent["down"]["details"]["count"], 2);

    // No measurements, no dimension lines.
    let drawer_svg = svg_of(&render(&ro, json!({"detail_id": drawer, "include_svg": true})).await);
    assert!(!drawer_svg.contains("class=\"dimension\""));
    // A measured detail with nothing drawn yet shows the outline its
    // measurements give.
    let blank = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "side", "view": "side",
               "measurements": {"depth": {"value": 600, "source": "from_drawing"}, "height": {"value": 720, "source": "estimated"}}}),
    )
    .await;
    let side = svg_of(&render(&ro, json!({"detail_id": blank, "include_svg": true})).await);
    assert!(side.contains(">depth 600 mm<") && side.contains(">height ~720 mm<"));
    // Clearing the measurements takes the lines away.
    ok(
        &rw,
        "upsert_detail",
        json!({"id": top, "measurements": null, "reason": "test"}),
    )
    .await;
    let svg = svg_of(&render(&ro, json!({"detail_id": top, "include_svg": true})).await);
    assert!(!svg.contains("class=\"dimension\""));
}

#[tokio::test]
async fn sites_are_in_areas_and_context_walks_every_level() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, floor, room, table) = house(&rw, "Walked house").await;
    let town = make(&rw, "upsert_area", json!({"name": "Testtown", "kind": "town"})).await;
    // About 160 m by 220 m around the site's origin at 9 E, 45 N.
    let outline = json!({"type": "Polygon", "coordinates": [[[8.999, 44.999], [9.001, 44.999], [9.001, 45.001], [8.999, 45.001], [8.999, 44.999]]]});
    let quarter = make(
        &rw,
        "upsert_area",
        json!({"name": "Old quarter", "kind": "neighbourhood", "parent_area_id": town, "geometry": outline}),
    )
    .await;
    // Named by area_id, with no georeference.
    let named = make(&rw, "create_site", json!({"name": "Named site", "area_id": town})).await;
    // Outside the outline, and in no area.
    let far = make(
        &rw,
        "create_site",
        json!({"name": "Far site", "origin": {"epsg": 4326, "x": 9.5, "y": 45.5}}),
    )
    .await;
    let sites = ok(&ro, "list_sites", json!({})).await;
    let area_of = |id: i64| {
        sites
            .as_array()
            .expect("sites")
            .iter()
            .find(|s| s["id"] == id)
            .expect("the site")["area"]
            .clone()
    };
    assert_eq!(
        area_of(site),
        json!({"id": quarter, "name": "Old quarter", "kind": "neighbourhood", "via": "origin"})
    );
    assert_eq!(
        area_of(named),
        json!({"id": town, "name": "Testtown", "kind": "town", "via": "area_id"})
    );
    assert_eq!(area_of(far), Value::Null);

    // Areas refuse a cycle.
    let cycle = refused(&rw, "upsert_area", json!({"id": town, "parent_area_id": quarter})).await;
    assert!(cycle.contains("inside itself"), "{cycle}");

    // From a nested detail up through every level.
    let top = make(
        &rw,
        "upsert_detail",
        json!({"object_id": table, "name": "top", "view": "top"}),
    )
    .await;
    let drawer = make(
        &rw,
        "upsert_detail",
        json!({"parent_detail_id": top, "name": "drawer", "view": "top", "placement": {"x": 1, "y": 1}}),
    )
    .await;
    let context = ok(&ro, "context", json!({"detail_id": drawer})).await;
    let steps: Vec<(String, i64)> = context["up"]
        .as_array()
        .expect("up")
        .iter()
        .map(|s| {
            (
                s["kind"].as_str().expect("kind").to_owned(),
                s["id"].as_i64().expect("id"),
            )
        })
        .collect();
    let expected: Vec<(String, i64)> = [
        ("detail", drawer),
        ("detail", top),
        ("object", table),
        ("space", room),
        ("floor", floor),
        ("site", site),
        ("area", quarter),
        ("area", town),
    ]
    .iter()
    .map(|(k, id)| ((*k).to_owned(), *id))
    .collect();
    assert_eq!(steps, expected);
    assert_eq!(context["up"][6]["via"], "origin");
    assert_eq!(context["up"][7]["via"], "parent_area_id");
    assert_eq!(context["up"][7]["placed"], false, "the town has no outline");

    // Down from the town and the quarter.
    let down = ok(&ro, "context", json!({"area_id": town})).await["down"].clone();
    assert_eq!(down["areas"]["items"][0]["id"], quarter);
    assert_eq!(down["sites"]["items"][0]["id"], named);
    assert_eq!(down["sites"]["items"][0]["via"], "area_id");
    let down = ok(&ro, "context", json!({"area_id": quarter})).await["down"].clone();
    assert_eq!(down["sites"]["items"][0]["id"], site);
    assert_eq!(down["sites"]["count"], 1);
    // find knows the areas by name; set_site_area moves a site.
    let found = ok(&ro, "find", json!({"name": "old quarter"})).await;
    assert_eq!(found["areas"][0]["id"], quarter);
    let moved = ok(
        &rw,
        "set_site_area",
        json!({"site_id": site, "area_id": town, "reason": "test"}),
    )
    .await;
    assert_eq!(moved["area"]["via"], "area_id");
    let cleared = ok(
        &rw,
        "set_site_area",
        json!({"site_id": site, "area_id": null, "reason": "test"}),
    )
    .await;
    assert_eq!(cleared["area"]["id"], quarter);
    assert_eq!(cleared["area"]["via"], "origin");
    // Deleting the town hides the quarter inside it.
    ok(&rw, "delete", json!({"kind": "area", "id": town, "reason": "test"})).await;
    let error = refused(&ro, "context", json!({"area_id": quarter})).await;
    assert!(error.contains("no live area"), "{error}");
    assert_eq!(area_of_now(&ro, site).await, Value::Null);
    ok(&rw, "restore", json!({"kind": "area", "id": town, "reason": "test"})).await;
    assert_eq!(area_of_now(&ro, site).await["id"], quarter);
}

async fn area_of_now(ro: &Client, site: i64) -> Value {
    let sites = ok(ro, "list_sites", json!({})).await;
    sites
        .as_array()
        .expect("sites")
        .iter()
        .find(|s| s["id"] == site)
        .expect("the site")["area"]
        .clone()
}

/// Every point in an SVG path's data, in pixels.
fn path_points(d: &str) -> Vec<(f64, f64)> {
    let numbers: Vec<f64> = d
        .split([' ', 'M', 'L', 'Z'])
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().expect("a number"))
        .collect();
    numbers.chunks(2).map(|c| (c[0], c[1])).collect()
}

/// The pixel bounds of the first path whose attributes contain `marker`.
fn path_bounds(svg: &str, marker: &str) -> (f64, f64, f64, f64) {
    let path = svg
        .split("<path ")
        .skip(1)
        .find(|p| {
            p.split("/>")
                .next()
                .is_some_and(|attributes| attributes.contains(marker))
        })
        .unwrap_or_else(|| panic!("a path with {marker}"));
    let d = path.split("d=\"").nth(1).and_then(|s| s.split('"').next()).expect("d");
    path_points(d)
        .iter()
        .fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |(x0, y0, x1, y1), &(x, y)| {
            (x0.min(x), y0.min(y), x1.max(x), y1.max(y))
        })
}

#[tokio::test]
async fn an_area_draws_its_sites_where_they_are() {
    let server = start().await;
    let rw = client(&server, RW).await;
    let ro = client(&server, RO).await;
    let (site, _, _, _) = house(&rw, "Mapped house").await;
    let outline = json!({"type": "Polygon", "coordinates": [[[8.999, 44.999], [9.001, 44.999], [9.001, 45.001], [8.999, 45.001], [8.999, 44.999]]]});
    let quarter = make(
        &rw,
        "upsert_area",
        json!({"name": "Old quarter", "kind": "neighbourhood", "geometry": outline}),
    )
    .await;
    make(&rw, "create_site", json!({"name": "Unmapped site", "area_id": quarter})).await;

    let drawn = render(&ro, json!({"area_id": quarter, "include_svg": true})).await;
    let summary = json_of(&drawn);
    let svg = svg_of(&drawn);
    assert_eq!(summary["sites"][0]["id"], site);
    assert_eq!(summary["not_drawn"][0]["name"], "Unmapped site");
    assert!(
        summary["notes"]
            .to_string()
            .contains("not drawn, no georeference: Unmapped site"),
        "{summary}"
    );
    assert!(svg.contains(">Mapped house<"), "the parcel is labelled with its site");
    assert!(svg.contains("M 0 -22 L 8 8"), "an area has a north arrow");

    // Where the parcel should be: its lower left corner is the site's origin,
    // 20 m on a side, placed against the outline in UTM 32N.
    let pool = pool(&server).await;
    let (xmin, ymax, origin_x, origin_y): (f64, f64, f64, f64) = sqlx::query_as(
        "SELECT ST_XMin(o), ST_YMax(o), ST_X(s.origin), ST_Y(s.origin)
         FROM area a CROSS JOIN LATERAL ST_Transform(a.geom, 32632) AS o, site s
         WHERE a.id = $1 AND s.id = $2",
    )
    .bind(quarter)
    .bind(site)
    .fetch_one(&pool)
    .await
    .expect("extents");
    let (ox0, oy0, ox1, _) = path_bounds(&svg, "stroke-dasharray=\"6 4\"");
    let (ux0, _, ux1): (f64, f64, f64) = sqlx::query_as(
        "SELECT ST_XMin(o), ST_YMin(o), ST_XMax(o) FROM area a CROSS JOIN LATERAL ST_Transform(a.geom, 32632) AS o WHERE a.id = $1",
    )
    .bind(quarter)
    .fetch_one(&pool)
    .await
    .expect("extent");
    assert!(near(ux0, xmin));
    let scale = (ox1 - ox0) / (ux1 - ux0);
    let expected_x = ox0 + (origin_x - xmin) * scale;
    let expected_y = oy0 + (ymax - origin_y) * scale;
    let (px0, _, px1, py1) = path_bounds(&svg, "stroke=\"#555\" stroke-width=\"1.5\"");
    assert!((px0 - expected_x).abs() < 1.0, "parcel x {px0}, expected {expected_x}");
    assert!((py1 - expected_y).abs() < 1.0, "parcel y {py1}, expected {expected_y}");
    assert!(((px1 - px0) - 20.0 * scale).abs() < 1.0, "parcel width");
}
